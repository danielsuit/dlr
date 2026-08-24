//! Receiver (DESIGN §2, §3.3, §6.4).
//!
//! Accumulates the per-session log in a content-addressed store and hands the
//! runtime a *pointer*, never a rebuild. Decodes APPEND deltas (decompress +
//! store + dedup), resolves Ref blocks locally, handles RESYNC (names the
//! missing set) and BULK (fountain-decode + store), and emits ACKs of the
//! session root.
//!
//! Importantly, reconstruction + store are cheap and continuous; the prune runs
//! on its own compute pool (§4), off the transfer hot path. The receiver never
//! blocks on the prune.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use rayon::prelude::*;

use dlr_coding::fountain::{FountainDecoder, FountainError};
use dlr_compress::Compressor;
use dlr_core::{
    append_root, from_canonical_owned, AckFrame, AppendFrame, Block, BlockId, BulkFrame,
    ContentStore, Frame, FrameBlock, MerkleRoot, MissingFrame, ResyncFrame, ROOT_ZERO,
};

#[derive(Debug, thiserror::Error)]
pub enum ReceiverError {
    #[error("fountain: {0}")]
    Fountain(String),
    #[error("compress: {0}")]
    Compress(String),
    #[error("frame: {0}")]
    Frame(String),
    #[error("referenced block id not in store")]
    MissingRef,
    #[error("session {0} not found")]
    NoSession(u128),
}

/// A handle the runtime uses to access an assembled session log without
/// rebuilding it — a pointer (session id + current root).
#[derive(Debug, Clone, Copy)]
pub struct LogPointer {
    pub session_id: u128,
    pub root: MerkleRoot,
    pub len: usize,
}

/// Per-session receiver state.
struct SessionState {
    /// Ordered block ids the receiver has stored.
    ids: Vec<BlockId>,
    root: MerkleRoot,
    /// In-flight fountain decoders by generation (cold start). Each is behind
    /// its own mutex so a generation's decode runs without the sessions lock.
    decoders: HashMap<u32, Arc<Mutex<FountainDecoder>>>,
}

/// The receiver: a content-addressed store plus per-session logs.
pub struct Receiver {
    store: ContentStore,
    compressor: Compressor,
    sessions: Mutex<HashMap<u128, SessionState>>,
}

impl Receiver {
    pub fn new(store: ContentStore, compressor: Compressor) -> Self {
        Self {
            store,
            compressor,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &ContentStore {
        &self.store
    }
    pub fn compressor(&self) -> &Compressor {
        &self.compressor
    }

    /// Handle an APPEND frame: decompress + store inline blocks, resolve refs,
    /// advance the session root, and return an ACK of the new root.
    ///
    /// If the frame's `base_root` does not match the receiver's current root,
    /// the receiver signals a divergence (cold start needed) by returning an
    /// error — the shim will then send RESYNC + BULK.
    pub fn handle_append(&self, frame: AppendFrame) -> Result<AckFrame, ReceiverError> {
        // Cheap divergence pre-check: a lock-free store read (`session_root` is a
        // DashMap get) catches an out-of-sync frame in O(1) *before* we spend a
        // full turn of decompression + per-block hashing on it. The store's root
        // and `SessionState.root` are kept in lockstep (both `append_root` the
        // same ids from `ROOT_ZERO`), so this matches the authoritative in-lock
        // check below; the authoritative check still runs to close the race
        // between this read and a concurrent store update.
        if frame.base_root != self.store.session_root(frame.session_id) {
            return Err(ReceiverError::Frame(format!(
                "base_root mismatch: client base {:?} != our root {:?}; cold start required",
                frame.base_root,
                self.store.session_root(frame.session_id),
            )));
        }
        // Pre-resolve every block *before* taking the sessions mutex: zstd
        // decompression and the per-block id hash are CPU-bound, and holding
        // the single sessions lock across them would serialize every session's
        // append behind one another's decompression. The lock is only needed
        // for the ordered id/root append + the store insert, which we do in a
        // single short critical section below. Appends still land in frame
        // order because the whole resolved batch is appended under one lock.
        enum Resolved {
            Inline { id: BlockId, block: Block },
            Ref(BlockId),
        }
        let mut resolved: Vec<Resolved> = Vec::with_capacity(frame.blocks.len());
        for fb in &frame.blocks {
            match fb {
                FrameBlock::Inline(wire_block) => {
                    // wire_block.payload is the *compressed canonical* bytes.
                    let comp = &wire_block.payload;
                    let canon = self
                        .compressor
                        .decompress(comp)
                        .map_err(|e| ReceiverError::Compress(e.to_string()))?;
                    // `from_canonical_owned` hashes + parses in one pass and
                    // keeps the payload as a zero-copy view of the decompressed
                    // buffer — no payload copy, no second full read.
                    let (block, id) = from_canonical_owned(canon)
                        .map_err(|e: &str| ReceiverError::Frame(e.to_string()))?;
                    resolved.push(Resolved::Inline { id, block });
                }
                FrameBlock::Ref(id) => {
                    resolved.push(Resolved::Ref(*id));
                }
            }
        }

        let mut g = self.sessions.lock();
        let st = g.entry(frame.session_id).or_insert_with(|| SessionState {
            ids: Vec::new(),
            root: ROOT_ZERO,
            decoders: HashMap::new(),
        });
        // divergence check: if the client's base_root does not match our root,
        // we are out of sync.
        if frame.base_root != st.root {
            return Err(ReceiverError::Frame(format!(
                "base_root mismatch: client base {:?} != our root {:?}; cold start required",
                frame.base_root, st.root
            )));
        }
        for r in resolved {
            match r {
                Resolved::Inline { id, block } => {
                    st.ids.push(id);
                    st.root = append_root(&st.root, &id);
                    // insert_with_id reuses the id we already derived from the
                    // canonical bytes, skipping a second hash of the block.
                    self.store.insert_with_id(frame.session_id, block, id);
                }
                Resolved::Ref(id) => {
                    if !self.store.contains(&id) {
                        return Err(ReceiverError::MissingRef);
                    }
                    st.ids.push(id);
                    st.root = append_root(&st.root, &id);
                    self.store
                        .reference(frame.session_id, id)
                        .map_err(|_| ReceiverError::MissingRef)?;
                }
            }
        }
        Ok(AckFrame {
            session_id: frame.session_id,
            root: st.root,
        })
    }

    /// Handle a RESYNC frame: compute the missing block set (the ones we don't
    /// have) and return the missing ids. On a cold gateway this is all of them.
    pub fn handle_resync(&self, frame: &ResyncFrame) -> Vec<BlockId> {
        let mut missing = Vec::new();
        for id in &frame.manifest {
            if !self.store.contains(id) {
                missing.push(*id);
            }
        }
        // stash the manifest so the receiver knows the expected root / order
        let mut g = self.sessions.lock();
        let st = g.entry(frame.session_id).or_insert_with(|| SessionState {
            ids: Vec::new(),
            root: ROOT_ZERO,
            decoders: HashMap::new(),
        });
        // record the client root as the target once all manifest blocks arrive.
        st.ids = frame.manifest.clone();
        st.root = frame.client_root;
        missing
    }

    /// Handle a BULK frame: fountain-decode the generation and store recovered
    /// blocks. Once a generation decodes, the recovered *compressed canonical*
    /// bytes are decompressed and stored. Returns true if this generation newly
    /// decoded.
    pub fn handle_bulk(&self, frame: &BulkFrame) -> Result<bool, ReceiverError> {
        // Phase 1 (sessions lock held only briefly): fetch or create the
        // per-generation decoder, then clone its `Arc` and release the sessions
        // lock. The full fountain decode is CPU-bound; holding the single
        // sessions mutex across it (the old shape) serialized every session's
        // cold-start decode behind every other's and blocked APPEND handling.
        let dec = {
            let mut g = self.sessions.lock();
            let st = g.entry(frame.session_id).or_insert_with(|| SessionState {
                ids: Vec::new(),
                root: ROOT_ZERO,
                decoders: HashMap::new(),
            });
            st.decoders
                .entry(frame.generation)
                .or_insert_with(|| {
                    Arc::new(Mutex::new(FountainDecoder::new(
                        frame.k as usize,
                        frame.symbol_size as usize,
                    )))
                })
                .clone()
        };
        // Phase 2 (per-generation lock, NOT the sessions lock): add symbols and
        // attempt decode. Same-generation bulk frames serialize here, which is
        // correct (a generation's symbols must be assembled in order); other
        // generations and sessions proceed concurrently, and APPEND handling
        // is no longer blocked by an in-flight decode.
        let decode_result = {
            let mut d = dec.lock();
            for s in &frame.symbols {
                d.add(s)
                    .map_err(|e| ReceiverError::Fountain(e.to_string()))?;
            }
            d.decode()
        };
        match decode_result {
            Ok(symbols) => {
                // concatenate the generation's source symbols into the flat stream
                let total: usize = symbols.iter().map(|s| s.len()).sum();
                let mut flat = Vec::with_capacity(total);
                for s in &symbols {
                    flat.extend_from_slice(s);
                }
                // Pass 1 (serial): parse the variable-length [len:u32][comp]
                // framing into slice descriptors. The flat stream is a sequence
                // of compressed canonical blocks; each carries the compressor's
                // marker, so the boundary isn't knowable without the framing
                // length the shim prepended. Stop at the zero-length padding
                // marker (the shim zero-pads the stream to a whole symbol) or a
                // truncated tail.
                let mut frames: Vec<(usize, usize)> = Vec::new();
                let mut off = 0usize;
                while off + 4 <= flat.len() {
                    let blen = u32::from_le_bytes(flat[off..off + 4].try_into().unwrap()) as usize;
                    off += 4;
                    if blen == 0 {
                        break;
                    } // zero-pad tail of the coded stream
                    if off + blen > flat.len() {
                        break;
                    } // truncated tail
                    frames.push((off, blen));
                    off += blen;
                }
                // Pass 2 (parallel): decompress + parse each recovered block
                // concurrently. zstd decompression is the CPU-heavy part of
                // cold-start recovery; the store is DashMap-sharded and the
                // compressor's zstd contexts are thread-local, so this is safe
                // under rayon. Recovered (block, id) pairs land in frame order so
                // the serial store pass below preserves the order the Merkle root
                // depends on.
                let session_id = frame.session_id;
                let recovered: Result<Vec<(Block, BlockId)>, ReceiverError> = frames
                    .par_iter()
                    .map(|&(off, blen)| {
                        let comp = &flat[off..off + blen];
                        let canon = self
                            .compressor
                            .decompress(comp)
                            .map_err(|e| ReceiverError::Compress(e.to_string()))?;
                        from_canonical_owned(canon)
                            .map_err(|e: &str| ReceiverError::Frame(e.to_string()))
                    })
                    .collect();
                let recovered = recovered?;
                // Pass 3 (serial, in frame/manifest order): store each block.
                // Order matters — the session Merkle root is the rolling hash of
                // ids in insertion order, and the bulk stream is in manifest order
                // (the shim iterates the missing set, which `handle_resync`
                // derived in manifest order), so storing serially here reproduces
                // the client's root exactly.
                for (block, id) in recovered {
                    self.store.insert_with_id(session_id, block, id);
                }
                // generation complete: drop its in-flight decoder. Brief sessions
                // lock only for the map removal; the store writes above needed no
                // sessions lock (the store is concurrent).
                let mut g = self.sessions.lock();
                if let Some(st) = g.get_mut(&frame.session_id) {
                    st.decoders.remove(&frame.generation);
                }
                Ok(true)
            }
            Err(FountainError::Underdetermined { .. }) => Ok(false),
            Err(e) => Err(ReceiverError::Fountain(e.to_string())),
        }
    }

    /// Handle any frame, returning an optional frame to send back (ACK).
    pub fn handle_frame(&self, f: Frame) -> Result<Option<Frame>, ReceiverError> {
        match f {
            Frame::Append(a) => {
                let ack = self.handle_append(a)?;
                Ok(Some(Frame::Ack(ack)))
            }
            Frame::Resync(r) => {
                let missing = self.handle_resync(&r);
                // Close the §3.3 handshake: name the missing set back to the sender
                // so it codes *only* what we lack (sparse reconstruction).
                Ok(Some(Frame::Missing(MissingFrame {
                    session_id: r.session_id,
                    missing,
                })))
            }
            Frame::Bulk(b) => {
                let _ = self.handle_bulk(&b)?;
                Ok(None)
            }
            Frame::Ack(_) => Ok(None),
            Frame::Missing(_) => Ok(None),
        }
    }

    /// Return a pointer to an assembled session log (for the runtime / prune).
    pub fn pointer(&self, session_id: u128) -> Option<LogPointer> {
        let g = self.sessions.lock();
        g.get(&session_id).map(|st| LogPointer {
            session_id,
            root: st.root,
            len: st.ids.len(),
        })
    }

    /// Reconstruct the full ordered session log (for the prune / distillation
    /// sink). Cheap: clones refcounted `Bytes`.
    ///
    /// The authoritative order is `SessionState.ids` — the manifest order set at
    /// RESYNC, extended by APPEND arrival order. The store's own session log is
    /// in *insertion* order, which for a cold start is generation-decode order
    /// and can differ from the original order when bulk frames arrive out of
    /// order (multipath QUIC / RDMA / multicast all reorder). Fetch by id in
    /// `st.ids` order so the prune always sees the true ordered log regardless
    /// of arrival order. Blocks not yet stored (a partial cold start) are
    /// dropped, yielding the correct partial prefix.
    pub fn reconstruct(&self, session_id: u128) -> Vec<Block> {
        let ids = {
            let g = self.sessions.lock();
            match g.get(&session_id) {
                Some(st) => st.ids.clone(),
                None => return Vec::new(),
            }
        };
        ids.iter().filter_map(|id| self.store.get(id)).collect()
    }

    /// Reconstruct a range (e.g. the last N blocks) for a candidate prune window.
    ///
    /// Uses the same `SessionState.ids` (manifest) order as [`reconstruct`] — the
    /// store's own session log is in insertion order, which under out-of-order
    /// bulk arrival (multipath / RDMA / multicast) differs from the original
    /// order, so a range taken over insertion order would return the wrong slice.
    /// Blocks not yet stored (a partial cold start) are dropped.
    pub fn reconstruct_range(&self, session_id: u128, from: usize, to: usize) -> Vec<Block> {
        let ids = {
            let g = self.sessions.lock();
            match g.get(&session_id) {
                Some(st) => st.ids.clone(),
                None => return Vec::new(),
            }
        };
        let to = to.min(ids.len());
        if from >= to {
            return Vec::new();
        }
        ids[from..to]
            .iter()
            .filter_map(|id| self.store.get(id))
            .collect()
    }
}
