//! Append-only write-ahead log for the content-addressed store.
//!
//! DESIGN §3 / §4: the receiver accumulates the per-session log in a
//! content-addressed store. If that store is in-memory only, a receiver
//! restart forces a full cold-start re-transfer of the ~200 MB log — which
//! would weaken the "cold resume is paid ONCE" bound. This WAL makes the
//! store **durable**: every insert/reference is appended to a sequential
//! file, and on restart the store is rebuilt by replaying the log. The
//! append is a buffered sequential write (a memcpy into the BufWriter); the
//! expensive `fsync` is deferred to an explicit [`Wal::flush`], so the hot
//! path stays cheap while crash-safety is tunable.
//!
//! Record format (all little-endian):
//!   `session_id : 16` | `variant : 1` (0=insert, 1=reference) | `len : u4` | `bytes[len]`
//! For `insert`, `bytes` is the block's canonical encoding; for `reference`,
//! `bytes` is the 32-byte `block_id`. Replay re-runs `insert`/`reference` in
//! order, so session logs and Merkle roots rebuild identically (the DAG is a
//! deterministic function of the ordered block ids).

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use parking_lot::Mutex;

use crate::store::ContentStore;

const VAR_INSERT: u8 = 0;
const VAR_REFERENCE: u8 = 1;
const HEADER: usize = 16 + 1 + 4;
// NOTE: `HEADER` is still referenced by `replay` (which reads a fixed-size
// header buffer); the append path now writes the fields directly instead of
// building a `HEADER`-sized `Vec`, so this constant is no longer used for the
// write capacity hint but remains the on-disk header length.

/// Handle to an open WAL file. Append-only; concurrent appends are serialized
/// by an internal mutex. Reads (replay) happen once at open.
pub struct Wal {
    writer: Mutex<BufWriter<File>>,
}

impl Wal {
    /// Open (creating if absent) the WAL at `path`, replay it into `store`,
    /// and return a handle for further appends.
    pub fn open<P: AsRef<Path>>(path: P, store: &ContentStore) -> std::io::Result<Self> {
        let path = path.as_ref();
        // Replay first, from any existing log, before opening for append.
        if path.exists() {
            let f = File::open(path)?;
            replay(BufReader::new(f), store)?;
        }
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(f)),
        })
    }

    /// Append an insert record. `canonical` is the block's canonical encoding
    /// (the caller already has it, or can obtain it via `canonical_bytes`).
    /// On replay the block is re-inserted, which re-appends its id to the
    /// session log and re-derives the same Merkle root.
    pub fn append_insert(&self, session_id: u128, canonical: &[u8]) -> std::io::Result<()> {
        // Write the record header fields directly to the BufWriter instead of
        // allocating a temp `Vec` of `HEADER + canonical.len()`, copying the
        // header + payload into it, then `write_all`-ing it. The BufWriter
        // already batches the four small writes into one logical record; the
        // on-disk format is byte-identical and the per-durable-append
        // payload-sized allocation disappears.
        let mut w = self.writer.lock();
        w.write_all(&session_id.to_le_bytes())?;
        w.write_all(&[VAR_INSERT])?;
        w.write_all(&(canonical.len() as u32).to_le_bytes())?;
        w.write_all(canonical)
    }

    /// Append a reference record (dedup path: the block is already stored, only
    /// the session-log append must be replayed).
    pub fn append_reference(&self, session_id: u128, id: [u8; 32]) -> std::io::Result<()> {
        let mut w = self.writer.lock();
        w.write_all(&session_id.to_le_bytes())?;
        w.write_all(&[VAR_REFERENCE])?;
        w.write_all(&32u32.to_le_bytes())?;
        w.write_all(&id)
    }

    /// Flush the in-process buffer. With `sync=true`, also fsync the file
    /// descriptor so records survive a crash. Batching flushes (e.g. once per
    /// turn or per flush tick) keeps the hot path off the fsync latency.
    pub fn flush(&self, sync: bool) -> std::io::Result<()> {
        let mut w = self.writer.lock();
        w.flush()?;
        if sync {
            w.get_ref().sync_data()?;
        }
        Ok(())
    }
}

fn replay<R: Read>(mut r: R, store: &ContentStore) -> std::io::Result<()> {
    let mut hdr = [0u8; HEADER];
    loop {
        match read_exact(&mut r, &mut hdr)? {
            true => {}
            false => return Ok(()), // clean EOF
        }
        let mut sid_bytes = [0u8; 16];
        sid_bytes.copy_from_slice(&hdr[..16]);
        let session_id = u128::from_le_bytes(sid_bytes);
        let variant = hdr[16];
        let len = u32::from_le_bytes([hdr[17], hdr[18], hdr[19], hdr[20]]) as usize;
        let mut payload = vec![0u8; len];
        if !read_exact(&mut r, &mut payload)? {
            // truncated tail: a crash mid-append. Stop replay at the last
            // complete record — the next append resumes cleanly.
            return Ok(());
        }
        match variant {
            VAR_INSERT => {
                if let Ok(block) = crate::canonical::from_canonical(&payload) {
                    store.insert(session_id, block);
                }
            }
            VAR_REFERENCE => {
                if payload.len() == 32 {
                    let mut id = [0u8; 32];
                    id.copy_from_slice(&payload);
                    let _ = store.reference(session_id, id);
                }
            }
            _ => { /* unknown variant from a future version: skip */ }
        }
    }
}

/// Read exactly `buf.len()` bytes. Returns `true` on a full read, `false` on
/// EOF before any byte was read. A short read (some bytes then EOF) is treated
/// as a truncated record: returns `false` and leaves `buf` partially filled.
fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            return Ok(got > 0 && got == buf.len());
        }
        got += n;
    }
    Ok(true)
}
