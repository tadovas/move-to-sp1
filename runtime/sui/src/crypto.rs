//! Signature natives (`sui::ecdsa_k1`, `sui::ecdsa_r1`) and `sui::hmac`, with Sui's semantics
//! (`sui-move-natives/src/crypto`): invalid inputs give `false` from `*_verify`, and abort
//! `ecrecover` / `decompress_pubkey` with the module's error codes.
//!
//! secp256k1, secp256r1 and ed25519 use SP1's patched `k256` / `p256` / `curve25519-dalek`, which run on its elliptic-curve
//! precompiles; Keccak-256 and SHA-256 use its hash precompiles.

use core::mem::MaybeUninit;

use move_rt_core::{abort_in, desc::VecHeader};
use sha2::Digest;

const ECDSA_K1: &str = "0000000000000000000000000000000000000000000000000000000000000002::ecdsa_k1";
const ECDSA_R1: &str = "0000000000000000000000000000000000000000000000000000000000000002::ecdsa_r1";
const E_FAIL_TO_RECOVER_PUBKEY: u64 = 0;
const E_INVALID_SIGNATURE: u64 = 1;
const E_INVALID_PUBKEY: u64 = 2;

const KECCAK256: u8 = 0;
const SHA256: u8 = 1;

/// The message digest named by `hash`, if it is one of the supported functions.
fn digest(msg: &[u8], hash: u8) -> Option<[u8; 32]> {
    match hash {
        KECCAK256 => {
            use tiny_keccak::{Hasher, Keccak};
            let mut h = Keccak::v256();
            h.update(msg);
            let mut d = [0u8; 32];
            h.finalize(&mut d);
            Some(d)
        }
        SHA256 => Some(sha2::Sha256::digest(msg).into()),
        _ => None,
    }
}

/// `ecdsa_k1::secp256k1_ecrecover(signature, msg, hash)`: the signer's compressed public key.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_secp256k1_ecrecover(
    sig: &VecHeader,
    msg: &VecHeader,
    hash: u8,
    out: &mut MaybeUninit<VecHeader>,
) {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let sig = sig.bytes();
    let parsed = (sig.len() == 65)
        .then(|| {
            Some((
                Signature::from_slice(&sig[..64]).ok()?,
                RecoveryId::from_byte(sig[64])?,
            ))
        })
        .flatten();
    let Some((s, id)) = parsed else {
        abort_in(ECDSA_K1, E_INVALID_SIGNATURE)
    };
    let pk = digest(msg.bytes(), hash)
        .and_then(|d| VerifyingKey::recover_from_prehash(&d, &s, id).ok())
        .unwrap_or_else(|| abort_in(ECDSA_K1, E_FAIL_TO_RECOVER_PUBKEY));
    out.write(VecHeader::from_bytes(pk.to_encoded_point(true).as_bytes()));
}

/// `ecdsa_k1::decompress_pubkey(pubkey)`: the 65-byte uncompressed form.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_secp256k1_decompress_pubkey(
    pk: &VecHeader,
    out: &mut MaybeUninit<VecHeader>,
) {
    use k256::elliptic_curve::sec1::ToEncodedPoint;
    let pk = k256::PublicKey::from_sec1_bytes(pk.bytes())
        .unwrap_or_else(|_| abort_in(ECDSA_K1, E_INVALID_PUBKEY));
    out.write(VecHeader::from_bytes(pk.to_encoded_point(false).as_bytes()));
}

/// `ecdsa_k1::secp256k1_verify(signature, public_key, msg, hash)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_secp256k1_verify(
    sig: &VecHeader,
    pk: &VecHeader,
    msg: &VecHeader,
    hash: u8,
) -> bool {
    use k256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};
    (|| {
        let s = Signature::from_slice(sig.bytes()).ok()?;
        let vk = VerifyingKey::from_sec1_bytes(pk.bytes()).ok()?;
        let d = digest(msg.bytes(), hash)?;
        vk.verify_prehash(&d, &s).ok()
    })()
    .is_some()
}

/// `ecdsa_r1::secp256r1_ecrecover(signature, msg, hash)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_secp256r1_ecrecover(
    sig: &VecHeader,
    msg: &VecHeader,
    hash: u8,
    out: &mut MaybeUninit<VecHeader>,
) {
    use p256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let sig = sig.bytes();
    let parsed = (sig.len() == 65)
        .then(|| {
            Some((
                Signature::from_slice(&sig[..64]).ok()?,
                RecoveryId::from_byte(sig[64])?,
            ))
        })
        .flatten();
    let Some((s, id)) = parsed else {
        abort_in(ECDSA_R1, E_INVALID_SIGNATURE)
    };
    let pk = digest(msg.bytes(), hash)
        .and_then(|d| VerifyingKey::recover_from_prehash(&d, &s, id).ok())
        .unwrap_or_else(|| abort_in(ECDSA_R1, E_FAIL_TO_RECOVER_PUBKEY));
    out.write(VecHeader::from_bytes(pk.to_encoded_point(true).as_bytes()));
}

/// `ecdsa_r1::secp256r1_verify(signature, public_key, msg, hash)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_secp256r1_verify(
    sig: &VecHeader,
    pk: &VecHeader,
    msg: &VecHeader,
    hash: u8,
) -> bool {
    use p256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};
    (|| {
        let s = Signature::from_slice(sig.bytes()).ok()?;
        let vk = VerifyingKey::from_sec1_bytes(pk.bytes()).ok()?;
        let d = digest(msg.bytes(), hash)?;
        vk.verify_prehash(&d, &s).ok()
    })()
    .is_some()
}

/// `ed25519::ed25519_verify(signature, public_key, msg)`. Sui verifies with ZIP-215 rules;
/// this uses `ed25519-dalek`'s (cofactorless) verification, which agrees on all honestly
/// generated signatures and may differ on adversarial edge cases (small-order keys,
/// non-canonical encodings).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_ed25519_verify(
    sig: &VecHeader,
    pk: &VecHeader,
    msg: &VecHeader,
) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    (|| {
        let s = Signature::from_slice(sig.bytes()).ok()?;
        let vk = VerifyingKey::from_bytes(pk.bytes().try_into().ok()?).ok()?;
        vk.verify(msg.bytes(), &s).ok()
    })()
    .is_some()
}

/// `hmac::hmac_sha3_256(key, msg)` (RFC 2104 with SHA3-256, block size 136).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_hmac_sha3_256(
    key: &VecHeader,
    msg: &VecHeader,
    out: &mut MaybeUninit<VecHeader>,
) {
    use tiny_keccak::{Hasher, Sha3};
    const BLOCK: usize = 136;
    let sha3 = |parts: &[&[u8]]| {
        let mut h = Sha3::v256();
        for p in parts {
            h.update(p);
        }
        let mut d = [0u8; 32];
        h.finalize(&mut d);
        d
    };
    let mut k = [0u8; BLOCK];
    let key = key.bytes();
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&sha3(&[key]));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.map(|x| x ^ b);
    let inner = sha3(&[&pad(0x36), msg.bytes()]);
    out.write(VecHeader::from_bytes(&sha3(&[&pad(0x5c), &inner])));
}
