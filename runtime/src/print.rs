//! Human-readable formatting of Move values for `std::debug::print`.

use alloc::string::String;
use core::fmt::Write;

use crate::desc::{FieldDesc, Kind, Value};

pub fn format(v: Value, out: &mut String) {
    let b = v.desc().kind().prim_size().map(|_| v.prim_bytes());
    let int = |n: usize| -> [u8; 16] {
        let mut a = [0u8; 16];
        a[..n].copy_from_slice(&b.unwrap()[..n]);
        a
    };
    let _ = match v.desc().kind() {
        Kind::Bool => write!(out, "{}", b.unwrap()[0] != 0),
        Kind::U8 | Kind::U16 | Kind::U32 | Kind::U64 | Kind::U128 => {
            let n = v.desc().kind().prim_size().unwrap();
            write!(out, "{}", u128::from_le_bytes(int(n)))
        }
        Kind::U256 => write!(out, "0x{}", hex(b.unwrap().iter().rev())),
        Kind::Address => write!(out, "@0x{}", hex(b.unwrap().iter())),
        // vector<u8> as hex, like the Move CLI
        Kind::Vector if v.desc().elem().kind() == Kind::U8 => {
            write!(out, "0x{}", hex(v.vec_bytes().iter()))
        }
        Kind::Vector => {
            out.push('[');
            for (i, e) in v.elems().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format(e, out);
            }
            out.push(']');
            Ok(())
        }
        Kind::Struct => match string_like(v) {
            Some(s) => write!(out, "{s:?}"),
            None => {
                out.push_str(v.desc().name());
                fields(v.fields(), out);
                Ok(())
            }
        },
        Kind::Enum => {
            let (_, variant, fs) = v.variant();
            let _ = write!(out, "{}::{}", v.desc().name(), variant.name());
            fields(fs, out);
            Ok(())
        }
    };
}

fn fields<'a>(fs: impl Iterator<Item = (&'a FieldDesc, Value<'a>)>, out: &mut String) {
    let mut fs = fs.peekable();
    if fs.peek().is_none() {
        return;
    }
    out.push_str(" { ");
    for (i, (f, v)) in fs.enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{}: ", f.name());
        format(v, out);
    }
    out.push_str(" }");
}

/// Text of a struct whose only field is a UTF-8 `vector<u8>` (`string::String`, ...).
fn string_like<'a>(v: Value<'a>) -> Option<&'a str> {
    let mut fs = v.fields();
    let (_, only) = fs.next()?;
    let is_bytes = only.desc().kind() == Kind::Vector && only.desc().elem().kind() == Kind::U8;
    if fs.next().is_some() || !is_bytes {
        return None;
    }
    core::str::from_utf8(only.vec_bytes()).ok()
}

fn hex<'a>(bytes: impl Iterator<Item = &'a u8>) -> String {
    let mut s = String::new();
    bytes.for_each(|b| {
        let _ = write!(s, "{b:02x}");
    });
    s
}
