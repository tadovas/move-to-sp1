//! BLS12-381 natives (`sui::bls12381`, `sui::group_ops`), with the semantics and encodings of
//! Sui's (fastcrypto / blst): points in the ZCash compressed format (G1: 48 bytes, G2: 96),
//! uncompressed G1 in the ZCash uncompressed format (96 bytes), scalars as 32 big-endian bytes
//! (canonical, < r). Invalid inputs abort `group_ops::EInvalidInput`; signature checks return
//! false. GT and pairings are not implemented (blst's GT encoding is not reproduced here): they
//! abort as unsupported natives.
//!
//! Runs on SP1's patched `bls12_381` (BLS12-381 precompiles).

use alloc::vec::Vec;
use core::mem::MaybeUninit;

use bls12_381::{
    G1Affine, G1Projective, G2Affine, G2Prepared, G2Projective, Scalar,
    hash_to_curve::{ExpandMsgXmd, HashToCurve},
    multi_miller_loop,
};
use group::{Curve, Group};
use move_rt_core::{abort_in, desc::VecHeader};

const GROUP_OPS: &str =
    "0000000000000000000000000000000000000000000000000000000000000002::group_ops";
const E_INVALID_INPUT: u64 = 1;

const SCALAR: u8 = 0;
const G1: u8 = 1;
const G2: u8 = 2;
const GT: u8 = 3;
const UNCOMPRESSED_G1: u8 = 4;

const DST_G1: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_";
const DST_G2: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

type Xmd = ExpandMsgXmd<sha2_09::Sha256>;

fn invalid() -> ! {
    abort_in(GROUP_OPS, E_INVALID_INPUT)
}

fn unsupported(op: &str) -> ! {
    let loc = alloc::format!("move2sp1::unsupported::group_ops::{op}");
    move_rt_core::abort_in(&loc, 0)
}

fn g1(b: &[u8]) -> Option<G1Affine> {
    Option::from(G1Affine::from_compressed(b.try_into().ok()?))
}
fn g1_uncompressed(b: &[u8]) -> Option<G1Affine> {
    Option::from(G1Affine::from_uncompressed(b.try_into().ok()?))
}
fn g2(b: &[u8]) -> Option<G2Affine> {
    Option::from(G2Affine::from_compressed(b.try_into().ok()?))
}
fn scalar(b: &[u8]) -> Option<Scalar> {
    let mut le: [u8; 32] = b.try_into().ok()?;
    le.reverse();
    Option::from(Scalar::from_bytes(&le))
}
fn scalar_bytes(s: &Scalar) -> [u8; 32] {
    let mut be = s.to_bytes();
    be.reverse();
    be
}

/// A group element of type `ty`, for arithmetic.
enum El {
    S(Scalar),
    P1(G1Projective),
    P2(G2Projective),
}

fn element(ty: u8, b: &[u8]) -> El {
    match ty {
        SCALAR => El::S(scalar(b).unwrap_or_else(|| invalid())),
        G1 => El::P1(g1(b).unwrap_or_else(|| invalid()).into()),
        G2 => El::P2(g2(b).unwrap_or_else(|| invalid()).into()),
        GT => unsupported("gt"),
        _ => invalid(),
    }
}

fn encode(e: El) -> Vec<u8> {
    match e {
        El::S(s) => scalar_bytes(&s).to_vec(),
        El::P1(p) => p.to_affine().to_compressed().to_vec(),
        El::P2(p) => p.to_affine().to_compressed().to_vec(),
    }
}

fn out_bytes(out: &mut MaybeUninit<VecHeader>, b: &[u8]) {
    out.write(VecHeader::from_bytes(b));
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_validate(ty: u8, e: &VecHeader) -> bool {
    let b = e.bytes();
    match ty {
        SCALAR => scalar(b).is_some(),
        G1 => g1(b).is_some(),
        G2 => g2(b).is_some(),
        UNCOMPRESSED_G1 => g1_uncompressed(b).is_some(),
        GT => unsupported("gt"),
        _ => false,
    }
}

/// `internal_add` / `internal_sub` (`sub` true).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_add(
    ty: u8,
    a: &VecHeader,
    b: &VecHeader,
    sub: bool,
    out: &mut MaybeUninit<VecHeader>,
) {
    let r = match (element(ty, a.bytes()), element(ty, b.bytes())) {
        (El::S(x), El::S(y)) => El::S(if sub { x - y } else { x + y }),
        (El::P1(x), El::P1(y)) => El::P1(if sub { x - y } else { x + y }),
        (El::P2(x), El::P2(y)) => El::P2(if sub { x - y } else { x + y }),
        _ => invalid(),
    };
    out_bytes(out, &encode(r));
}

/// `internal_mul` / `internal_div` (`div` true): `e` times `s` (or times `1/s`).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_mul(
    ty: u8,
    s: &VecHeader,
    e: &VecHeader,
    div: bool,
    out: &mut MaybeUninit<VecHeader>,
) {
    let mut k = scalar(s.bytes()).unwrap_or_else(|| invalid());
    if div {
        k = Option::from(k.invert()).unwrap_or_else(|| invalid());
    }
    let r = match element(ty, e.bytes()) {
        El::S(x) => El::S(if div { x * k } else { k * x }),
        El::P1(p) => El::P1(p * k),
        El::P2(p) => El::P2(p * k),
    };
    out_bytes(out, &encode(r));
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_hash_to(
    ty: u8,
    m: &VecHeader,
    out: &mut MaybeUninit<VecHeader>,
) {
    let m = m.bytes();
    if m.is_empty() {
        invalid();
    }
    let r = match ty {
        G1 => El::P1(<G1Projective as HashToCurve<Xmd>>::hash_to_curve(m, DST_G1)),
        G2 => El::P2(<G2Projective as HashToCurve<Xmd>>::hash_to_curve(m, DST_G2)),
        _ => invalid(),
    };
    out_bytes(out, &encode(r));
}

/// `internal_multi_scalar_mul(type, scalars, elements)` (concatenated encodings).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_msm(
    ty: u8,
    scalars: &VecHeader,
    elements: &VecHeader,
    out: &mut MaybeUninit<VecHeader>,
) {
    let size = match ty {
        G1 => 48,
        G2 => 96,
        _ => invalid(),
    };
    let (s, e) = (scalars.bytes(), elements.bytes());
    if s.is_empty() || s.len() % 32 != 0 || e.len() % size != 0 || s.len() / 32 != e.len() / size {
        invalid();
    }
    let ks: Vec<Scalar> = s
        .chunks(32)
        .map(|c| scalar(c).unwrap_or_else(|| invalid()))
        .collect();
    let r = match ty {
        G1 => El::P1(
            e.chunks(48)
                .zip(&ks)
                .map(|(p, k)| G1Projective::from(g1(p).unwrap_or_else(|| invalid())) * k)
                .sum(),
        ),
        _ => El::P2(
            e.chunks(96)
                .zip(&ks)
                .map(|(p, k)| G2Projective::from(g2(p).unwrap_or_else(|| invalid())) * k)
                .sum(),
        ),
    };
    out_bytes(out, &encode(r));
}

/// `internal_convert(from, to, e)`: G1 <-> uncompressed G1.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_convert(
    from: u8,
    to: u8,
    e: &VecHeader,
    out: &mut MaybeUninit<VecHeader>,
) {
    match (from, to) {
        (UNCOMPRESSED_G1, G1) => {
            let p = g1_uncompressed(e.bytes()).unwrap_or_else(|| invalid());
            out_bytes(out, &p.to_compressed());
        }
        (G1, UNCOMPRESSED_G1) => {
            let p = g1(e.bytes()).unwrap_or_else(|| invalid());
            out_bytes(out, &p.to_uncompressed());
        }
        _ => invalid(),
    }
}

/// `internal_sum(type, terms)`: only uncompressed G1.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_group_sum(
    ty: u8,
    terms: &VecHeader,
    out: &mut MaybeUninit<VecHeader>,
) {
    if ty != UNCOMPRESSED_G1 {
        invalid();
    }
    let sum: G1Projective = terms
        .headers()
        .iter()
        .map(|t| G1Projective::from(g1_uncompressed(t.bytes()).unwrap_or_else(|| invalid())))
        .sum();
    out_bytes(out, &sum.to_affine().to_uncompressed());
}

/// `bls12381_min_sig_verify(sig: G1, pk: G2, msg)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_bls12381_min_sig_verify(
    sig: &VecHeader,
    pk: &VecHeader,
    msg: &VecHeader,
) -> bool {
    let (Some(s), Some(p)) = (g1(sig.bytes()), g2(pk.bytes())) else {
        return false;
    };
    if bool::from(s.is_identity()) || bool::from(p.is_identity()) {
        return false;
    }
    let h = <G1Projective as HashToCurve<Xmd>>::hash_to_curve(msg.bytes(), DST_G1).to_affine();
    // e(sig, g2) == e(H(m), pk)  <=>  e(sig, -g2) * e(H(m), pk) == 1
    let neg_g2 = G2Prepared::from(-G2Affine::generator());
    let pk = G2Prepared::from(p);
    bool::from(
        multi_miller_loop(&[(&s, &neg_g2), (&h, &pk)])
            .final_exponentiation()
            .is_identity(),
    )
}

/// `bls12381_min_pk_verify(sig: G2, pk: G1, msg)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_bls12381_min_pk_verify(
    sig: &VecHeader,
    pk: &VecHeader,
    msg: &VecHeader,
) -> bool {
    let (Some(s), Some(p)) = (g2(sig.bytes()), g1(pk.bytes())) else {
        return false;
    };
    if bool::from(s.is_identity()) || bool::from(p.is_identity()) {
        return false;
    }
    let h = <G2Projective as HashToCurve<Xmd>>::hash_to_curve(msg.bytes(), DST_G2).to_affine();
    // e(pk, H(m)) == e(g1, sig)  <=>  e(-g1, sig) * e(pk, H(m)) == 1
    let neg_g1 = -G1Affine::generator();
    let (s, h) = (G2Prepared::from(s), G2Prepared::from(h));
    bool::from(
        multi_miller_loop(&[(&neg_g1, &s), (&p, &h)])
            .final_exponentiation()
            .is_identity(),
    )
}
