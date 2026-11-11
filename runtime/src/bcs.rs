//! BCS (Binary Canonical Serialization) of in-memory Move values, driven by type
//! descriptors. Used for entry arguments/results and `std::bcs::to_bytes`.
//!
//! bool: 1 byte; integers: little-endian; address: 32 bytes; vector: ULEB128 length +
//! elements; struct: fields in order; enum: ULEB128 variant tag + variant fields.

use alloc::vec::Vec;

use crate::desc::{self, TypeDesc, VecHeader};

pub unsafe fn encode(d: &TypeDesc, p: *const u8, out: &mut Vec<u8>) {
    match d.kind {
        desc::VECTOR => {
            let v = unsafe { &*(p as *const VecHeader) };
            uleb128(v.len, out);
            let e = d.elem();
            for i in 0..v.len {
                unsafe { encode(e, v.data.add((i * e.size) as usize), out) };
            }
        }
        desc::STRUCT => {
            for f in d.fields() {
                unsafe { encode(f.ty(), p.add(f.offset as usize), out) };
            }
        }
        desc::ENUM => {
            let tag = unsafe { *(p as *const u16) };
            uleb128(tag as u64, out);
            let payload = unsafe { p.add(d.payload_offset as usize) };
            for f in d.variants()[tag as usize].fields() {
                unsafe { encode(f.ty(), payload.add(f.offset as usize), out) };
            }
        }
        _ => out.extend_from_slice(unsafe { core::slice::from_raw_parts(p, wire_size(d)) }),
    }
}

pub unsafe fn decode(d: &TypeDesc, input: &mut &[u8], p: *mut u8) -> Result<(), &'static str> {
    match d.kind {
        desc::VECTOR => {
            let n = read_uleb128(input)?;
            let e = d.elem();
            let data = desc::alloc((n * e.size) as usize);
            for i in 0..n {
                unsafe { decode(e, input, data.add((i * e.size) as usize))? };
            }
            unsafe {
                (p as *mut VecHeader).write(VecHeader {
                    data,
                    len: n,
                    cap: n,
                })
            };
        }
        desc::STRUCT => {
            for f in d.fields() {
                unsafe { decode(f.ty(), input, p.add(f.offset as usize))? };
            }
        }
        desc::ENUM => {
            let tag = read_uleb128(input)?;
            let variant = d
                .variants()
                .get(tag as usize)
                .ok_or("invalid enum variant tag")?;
            unsafe { (p as *mut u16).write(tag as u16) };
            let payload = unsafe { p.add(d.payload_offset as usize) };
            for f in variant.fields() {
                unsafe { decode(f.ty(), input, payload.add(f.offset as usize))? };
            }
        }
        _ => {
            let n = wire_size(d);
            let bytes = take(input, n)?;
            if d.kind == desc::BOOL && bytes[0] > 1 {
                return Err("invalid bool");
            }
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n) };
        }
    }
    Ok(())
}

/// Encoded size of a primitive.
fn wire_size(d: &TypeDesc) -> usize {
    match d.kind {
        desc::BOOL | desc::U8 => 1,
        desc::U16 => 2,
        desc::U32 => 4,
        desc::U64 => 8,
        desc::U128 => 16,
        desc::U256 | desc::ADDRESS => 32,
        _ => unreachable!("not a primitive"),
    }
}

fn uleb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn read_uleb128(input: &mut &[u8]) -> Result<u64, &'static str> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = take(input, 1)?[0];
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("invalid ULEB128")
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], &'static str> {
    if input.len() < n {
        return Err("unexpected end of input");
    }
    let (h, t) = input.split_at(n);
    *input = t;
    Ok(h)
}

/// `VecHeader` of the BCS bytes of a value (for `std::bcs::to_bytes`).
pub unsafe fn to_bytes(d: &TypeDesc, p: *const u8) -> VecHeader {
    let mut out = Vec::new();
    unsafe { encode(d, p, &mut out) };
    VecHeader::from_bytes(&out)
}
