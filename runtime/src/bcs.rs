//! BCS (Binary Canonical Serialization) of Move values, driven by type descriptors.
//! Used for entry arguments/results and `std::bcs::to_bytes`.
//!
//! bool: 1 byte; integers: little-endian; address: 32 bytes; vector: ULEB128 length +
//! elements; struct: fields in order; enum: ULEB128 variant tag + variant fields.

use alloc::vec::Vec;

use crate::desc::{Kind, Slot, Value};

pub fn encode(v: Value, out: &mut Vec<u8>) {
    match v.desc().kind() {
        // fast path: bytes are copied as one slice
        Kind::Vector if v.desc().elem().kind() == Kind::U8 => {
            let bytes = v.vec_bytes();
            uleb128(bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        Kind::Vector => {
            let elems = v.elems();
            uleb128(elems.len() as u64, out);
            elems.for_each(|e| encode(e, out));
        }
        Kind::Struct => v.fields().for_each(|(_, f)| encode(f, out)),
        Kind::Enum => {
            let (tag, _, fields) = v.variant();
            uleb128(tag as u64, out);
            fields.for_each(|(_, f)| encode(f, out));
        }
        _ => out.extend_from_slice(v.prim_bytes()),
    }
}

pub fn decode(slot: Slot, input: &mut &[u8]) -> Result<(), &'static str> {
    match slot.desc().kind() {
        Kind::Vector if slot.desc().elem().kind() == Kind::U8 => {
            let n = read_uleb128(input)? as usize;
            slot.init_bytes(take(input, n)?);
        }
        Kind::Vector => {
            let n = read_uleb128(input)?;
            for e in slot.init_vector(n as usize) {
                decode(e, input)?;
            }
        }
        Kind::Struct => {
            for f in slot.fields() {
                decode(f, input)?;
            }
        }
        Kind::Enum => {
            let tag =
                u16::try_from(read_uleb128(input)?).map_err(|_| "invalid enum variant tag")?;
            for f in slot.init_variant(tag).ok_or("invalid enum variant tag")? {
                decode(f, input)?;
            }
        }
        kind => {
            let bytes = take(input, kind.prim_size().unwrap())?;
            if kind == Kind::Bool && bytes[0] > 1 {
                return Err("invalid bool");
            }
            slot.write_prim(bytes);
        }
    }
    Ok(())
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
