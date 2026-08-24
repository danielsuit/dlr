//! Random Linear Network Coding (RLNC) — multipath + multicast (DESIGN §6.5).
//!
//! Code packets as random linear combinations over GF(2^q):
//!   y_j = sum_i g_{ji} * x_i   (arithmetic in GF(2^q))
//! Each coded packet carries its coefficient vector g_j; a receiver inverts
//! any K linearly-independent y_j by Gaussian elimination.
//!
//! Capacity-optimal by the network coding theorem (Ahlswede-Cai-Li-Yeung 2000;
//! linear codes suffice, Li-Yeung-Cai 2003; random linear achieve capacity w.h.p.
//! Ho et al. 2006). Practically: fan the same context to N agents or spread over
//! multiple paths without coordinating which packet went where — any K
//! independent combinations reconstruct.
//!
//! We use dense GF(2^8) coefficients per generation (the multicast advantage is
//! the point, not the coefficient density). For very large generations switch
//! to sparse/structured coefficients behind the same API.

use crate::fountain::{self, FountainError};
use crate::gf256;

use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum RlncError {
    #[error("insufficient independent packets: have {have} need {need}")]
    Insufficient { have: usize, need: usize },
    #[error("singular generation after {have} packets")]
    Singular { have: usize },
    #[error("size mismatch: expected {exp} got {got}")]
    SizeMismatch { exp: usize, got: usize },
}

/// A coded packet: coefficient vector (length K) + payload (length S).
#[derive(Debug, Clone)]
pub struct CodedPacket {
    pub coeffs: Vec<u8>,
    pub payload: Vec<u8>,
}

/// Per-generation RLNC encoder.
pub struct RlncEncoder {
    k: usize,
    symbol_size: usize,
    source: Vec<Arc<[u8]>>,
    rng: Lcg,
}

impl RlncEncoder {
    pub fn new(source: Vec<Vec<u8>>, symbol_size: usize, seed: u64) -> Self {
        let k = source.len();
        let mut src = Vec::with_capacity(k);
        for mut s in source {
            s.resize(symbol_size, 0);
            src.push(Arc::from(s));
        }
        Self {
            k,
            symbol_size,
            source: src,
            rng: Lcg::new(seed),
        }
    }

    /// Build an encoder over already-`symbol_size` sources shared via `Arc<[u8]>`
    /// — a refcount bump per source, no byte clone. Used by the hierarchical
    /// outer/inner split (`HierEncoder::into_groups`), where every group
    /// RLNC-codes a disjoint subset of the *same* outer symbols and cloning
    /// each outer symbol per group would duplicate the (k+m)·symbol_size
    /// context once per group.
    pub fn new_shared(source: Vec<Arc<[u8]>>, symbol_size: usize, seed: u64) -> Self {
        let k = source.len();
        Self {
            k,
            symbol_size,
            source,
            rng: Lcg::new(seed),
        }
    }

    pub fn k(&self) -> usize {
        self.k
    }
    pub fn symbol_size(&self) -> usize {
        self.symbol_size
    }

    /// Produce one random linear combination of the K source symbols.
    pub fn code(&mut self) -> CodedPacket {
        let mut coeffs = vec![0u8; self.k];
        // draw dense random GF(2^8) coeffs; resample to avoid the all-zero row
        loop {
            let mut nz = 0;
            for c in coeffs.iter_mut() {
                let v = self.rng.next_u8();
                *c = v;
                if v != 0 {
                    nz += 1;
                }
            }
            if nz > 0 {
                break;
            }
        }
        let mut payload = vec![0u8; self.symbol_size];
        for (i, &c) in coeffs.iter().enumerate() {
            if c != 0 {
                gf256::axpy(&mut payload, c, &self.source[i]);
            }
        }
        CodedPacket { coeffs, payload }
    }

    /// Code `n` packets in one batch (the multicast fan-out path: one generation
    /// -> N coded packets, each independently useful to any receiver).
    pub fn code_batch(&mut self, n: usize) -> Vec<CodedPacket> {
        (0..n).map(|_| self.code()).collect()
    }

    /// Produce a **sparse** coded packet covering exactly `degree` random
    /// sources (low-density RLNC). Dense RLNC (§6.5) is capacity-optimal but
    /// decodes by O(K³) Gaussian elimination; for large multicast generations a
    /// sparse, low-degree packet lets the receiver use the **peeling decoder**
    /// (`sparse_decode`) for ~linear decode, trading a sliver of capacity for
    /// far cheaper reconstruction at large N.
    pub fn code_sparse(&mut self, degree: usize) -> CodedPacket {
        let degree = degree.max(1).min(self.k);
        let mut coeffs = vec![0u8; self.k];
        // pick `degree` distinct source indices with random nonzero coefficients
        let mut chosen: Vec<usize> = Vec::with_capacity(degree);
        let mut guard = 0;
        while chosen.len() < degree && guard < 8 * self.k {
            // Use the **high** bits of the stream (`>> 40`) to pick the index.
            // An LCG's low bits have a far shorter period than the full state,
            // so `(next_u64() % k)` for `k` sharing factors with 2^64 (notably
            // the powers of two RLNC uses, K=16/32/64/128) cycles through only
            // `k` distinct low-bit values and makes consecutive sparse packets
            // pick *identical* supports — collapsing 70 degree-4 packets to 8
            // distinct supports and rank 32. The high bits carry the
            // full-period avalanche, so `>> 40) % k` spreads.
            let idx = ((self.rng.next_u64() >> 40) as usize) % self.k;
            if coeffs[idx] == 0 {
                let c = self.rng.next_u8() | 1; // nonzero
                coeffs[idx] = c;
                chosen.push(idx);
            }
            guard += 1;
        }
        let mut payload = vec![0u8; self.symbol_size];
        for &i in &chosen {
            gf256::axpy(&mut payload, coeffs[i], &self.source[i]);
        }
        CodedPacket { coeffs, payload }
    }
}

/// Decode sparse-RLNC packets via the fountain **peeling decoder**. Sparse
/// packets have low degree so peeling is ~linear and the residual Gaussian is
/// tiny. This is the multicast fast path for large generations.
pub fn sparse_decode(
    packets: &[CodedPacket],
    k: usize,
    s: usize,
) -> Result<Vec<Vec<u8>>, RlncError> {
    let mut rows: Vec<Vec<u8>> = packets
        .iter()
        .filter(|p| p.coeffs.iter().any(|&c| c != 0))
        .map(|p| {
            let mut row = vec![0u8; k + s];
            row[..k].copy_from_slice(&p.coeffs);
            row[k..].copy_from_slice(&p.payload);
            row
        })
        .collect();
    fountain::peel_decode(&mut rows, k, s).map_err(|e| match e {
        FountainError::Underdetermined { have, need } => RlncError::Insufficient { have, need },
        _ => RlncError::Singular { have: k },
    })
}

/// Per-generation RLNC decoder. Collects packets, Gaussian-eliminates once it
/// has K independent ones.
pub struct RlncDecoder {
    k: usize,
    symbol_size: usize,
    /// Augmented rows [coeffs | payload], kept in reduced form.
    rows: Vec<Vec<u8>>,
    /// `pivots[i]` is the pivot column of `rows[i]`. Rows are kept in RREF, so a
    /// row's pivot never changes once it's added — caching it avoids an O(k)
    /// `first_nonzero` scan of every existing row on every incoming packet (the
    /// old form re-scanned all k columns of every row per `add`).
    pivots: Vec<usize>,
}

impl RlncDecoder {
    pub fn new(k: usize, symbol_size: usize) -> Self {
        Self {
            k,
            symbol_size,
            rows: Vec::with_capacity(k),
            pivots: Vec::with_capacity(k),
        }
    }

    pub fn rank(&self) -> usize {
        self.rows.len()
    }
    pub fn k(&self) -> usize {
        self.k
    }

    /// Add a coded packet. Returns true if it increased rank.
    pub fn add(&mut self, p: CodedPacket) -> Result<bool, RlncError> {
        if p.coeffs.len() != self.k {
            return Err(RlncError::SizeMismatch {
                exp: self.k,
                got: p.coeffs.len(),
            });
        }
        if p.payload.len() != self.symbol_size {
            return Err(RlncError::SizeMismatch {
                exp: self.symbol_size,
                got: p.payload.len(),
            });
        }
        let mut row = vec![0u8; self.k + self.symbol_size];
        row[..self.k].copy_from_slice(&p.coeffs);
        row[self.k..].copy_from_slice(&p.payload);

        // Reduce against existing pivots. Each row's pivot column is cached in
        // `self.pivots`, so this is O(rows) iterations with no per-row O(k) scan.
        for (i, e) in self.rows.iter_mut().enumerate() {
            let piv = self.pivots[i];
            if row[piv] != 0 {
                let c = row[piv];
                gf256::axpy(&mut row, c, e);
            }
        }
        match first_nonzero(&row[..self.k]) {
            Some(piv) => {
                let inv = gf256::inv(row[piv]);
                gf256::scal_inplace(&mut row, inv);
                // Eliminate the new pivot column from every existing row. The new
                // row is now the only one with a nonzero in column `piv` besides
                // what's being cleared here.
                for e in self.rows.iter_mut() {
                    if e[piv] != 0 {
                        let c = e[piv];
                        gf256::axpy(e, c, &row);
                    }
                }
                self.pivots.push(piv);
                self.rows.push(row);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Decode once rank == K. Returns the K recovered source symbols.
    pub fn decode(&self) -> Result<Vec<Vec<u8>>, RlncError> {
        if self.rows.len() < self.k {
            return Err(RlncError::Insufficient {
                have: self.rows.len(),
                need: self.k,
            });
        }
        let mut out = vec![vec![0u8; self.symbol_size]; self.k];
        let mut seen = vec![false; self.k];
        // Cached pivots place each row's payload directly — no per-row scan.
        for (i, r) in self.rows.iter().enumerate() {
            let piv = self.pivots[i];
            out[piv].copy_from_slice(&r[self.k..]);
            seen[piv] = true;
        }
        if seen.iter().any(|s| !*s) {
            return Err(RlncError::Singular {
                have: self.rows.len(),
            });
        }
        Ok(out)
    }
}

#[inline]
fn first_nonzero(row: &[u8]) -> Option<usize> {
    row.iter().position(|&x| x != 0)
}

// shared LCG (same as fountain)
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(0x9E3779B97F4A7C15u64.wrapping_mul(seed ^ 0xD1B54A32D192ED03))
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    #[inline]
    fn next_u8(&mut self) -> u8 {
        (self.next_u64() >> 40) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let k = 32;
        let s = 16;
        let source: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..s).map(|j| ((i * 31 + j) & 0xFF) as u8).collect())
            .collect();
        let mut enc = RlncEncoder::new(source.clone(), s, 0xA5A5);
        // any K+ a few extra coded packets decode, regardless of which arrived
        let mut dec = RlncDecoder::new(k, s);
        let mut produced = 0;
        while dec.rank() < k {
            let p = enc.code();
            produced += 1;
            dec.add(p).unwrap();
            assert!(produced <= k + 4, "too many packets for full rank");
        }
        let rec = dec.decode().unwrap();
        assert_eq!(rec, source);
    }

    #[test]
    fn sparse_roundtrip_peels() {
        let k = 64;
        let s = 16;
        let source: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..s).map(|j| ((i * 13 + j) & 0xFF) as u8).collect())
            .collect();
        let mut enc = RlncEncoder::new(source.clone(), s, 0xBEEF);
        // sparse, degree 4; emit a few extra so the residual is solvable
        let mut pkts = Vec::new();
        for _ in 0..(k + 6) {
            pkts.push(enc.code_sparse(4));
        }
        let rec = sparse_decode(&pkts, k, s).unwrap();
        assert_eq!(rec, source);
    }
}
