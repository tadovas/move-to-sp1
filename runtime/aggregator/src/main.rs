//! Batch aggregation: verifies N SP1 proofs (compressed proofs of move2sp1 runs — e.g. the
//! replayed transactions of a Sui checkpoint) inside SP1, and commits to all of them.
//!
//! Input (stdin): `label: Vec<u8>` (e.g. "sui:checkpoint:150000003", committed as given), `n:
//! u32`, then per proof `vk_digest: [u32; 8]` and its public values (`Vec<u8>`); the proofs
//! themselves are in the proof stream (`SP1Stdin::write_proof`), in the same order.
//!
//! Committed: `len(label) (u32 LE) || label || n (u32 LE) || per proof: vk_digest (32 bytes) || sha256(public values)`.
//! A verifier of this one proof knows that each listed program (by verifying key) ran with
//! the listed public values — for move2sp1 replays: the input state hash, results, effects and
//! output state of each transaction.

#![no_main]
sp1_zkvm::entrypoint!(main);

use sha2::{Digest, Sha256};

fn main() {
    let label: Vec<u8> = sp1_zkvm::io::read_vec();
    let n: u32 = sp1_zkvm::io::read();
    let mut out = Vec::with_capacity(label.len() + 4 + 64 * n as usize);
    out.extend_from_slice(&(label.len() as u32).to_le_bytes());
    out.extend_from_slice(&label);
    out.extend_from_slice(&n.to_le_bytes());
    for _ in 0..n {
        let vk: [u32; 8] = sp1_zkvm::io::read();
        let public_values: Vec<u8> = sp1_zkvm::io::read_vec();
        let digest: [u8; 32] = Sha256::digest(&public_values).into();
        sp1_zkvm::lib::verify::verify_sp1_proof(&vk, &digest);
        for w in vk {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&digest);
    }
    sp1_zkvm::io::commit_slice(&out);
}
