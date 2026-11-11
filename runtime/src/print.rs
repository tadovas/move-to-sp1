//! Human-readable formatting of Move values for `std::debug::print`.

use alloc::string::String;
use core::fmt::Write;

use crate::desc::{self, TypeDesc, VecHeader};

pub unsafe fn format(d: &TypeDesc, p: *const u8, out: &mut String) {
    let le = |n: usize| unsafe { core::slice::from_raw_parts(p, n) };
    match d.kind {
        desc::BOOL => out.push_str(if unsafe { *p } != 0 { "true" } else { "false" }),
        desc::U8 => {
            let _ = write!(out, "{}", unsafe { *p });
        }
        desc::U16 => {
            let _ = write!(out, "{}", u16::from_le_bytes(le(2).try_into().unwrap()));
        }
        desc::U32 => {
            let _ = write!(out, "{}", u32::from_le_bytes(le(4).try_into().unwrap()));
        }
        desc::U64 => {
            let _ = write!(out, "{}", u64::from_le_bytes(le(8).try_into().unwrap()));
        }
        desc::U128 => {
            let _ = write!(out, "{}", u128::from_le_bytes(le(16).try_into().unwrap()));
        }
        desc::U256 => {
            out.push_str("0x");
            le(32).iter().rev().for_each(|b| {
                let _ = write!(out, "{b:02x}");
            });
        }
        desc::ADDRESS => {
            out.push_str("@0x");
            le(32).iter().for_each(|b| {
                let _ = write!(out, "{b:02x}");
            });
        }
        desc::VECTOR if d.elem().kind == desc::U8 => {
            // vector<u8> as hex, like the Move CLI
            out.push_str("0x");
            unsafe { VecHeader::bytes(p as *const VecHeader) }
                .iter()
                .for_each(|b| {
                    let _ = write!(out, "{b:02x}");
                });
        }
        desc::VECTOR => {
            let v = unsafe { &*(p as *const VecHeader) };
            let e = d.elem();
            out.push('[');
            for i in 0..v.len {
                if i > 0 {
                    out.push_str(", ");
                }
                unsafe { format(e, v.data.add((i * e.size) as usize), out) };
            }
            out.push(']');
        }
        desc::STRUCT if is_string(d) => {
            // String-like wrapper (`string::String`, `ascii::String`): print as text
            let b = unsafe { VecHeader::bytes(p as *const VecHeader) };
            match core::str::from_utf8(b) {
                Ok(s) => {
                    let _ = write!(out, "{s:?}");
                }
                Err(_) => unsafe { format(d.fields()[0].ty(), p, out) },
            }
        }
        desc::STRUCT => {
            out.push_str(d.name());
            unsafe { fields(d.fields(), p, out) };
        }
        desc::ENUM => {
            let tag = unsafe { *(p as *const u16) } as usize;
            let v = &d.variants()[tag];
            let _ = write!(out, "{}::{}", d.name(), v.name());
            unsafe { fields(v.fields(), p.add(d.payload_offset as usize), out) };
        }
        _ => out.push_str("<?>"),
    }
}

unsafe fn fields(fs: &[desc::FieldDesc], base: *const u8, out: &mut String) {
    if fs.is_empty() {
        return;
    }
    out.push_str(" { ");
    for (i, f) in fs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{}: ", f.name());
        unsafe { format(f.ty(), base.add(f.offset as usize), out) };
    }
    out.push_str(" }");
}

/// A struct whose only field is a `vector<u8>` (at offset 0).
fn is_string(d: &TypeDesc) -> bool {
    matches!(d.fields(), [f] if f.offset == 0 && f.ty().kind == desc::VECTOR && f.ty().elem().kind == desc::U8)
}
