//! The program ABI (`.move2sp1.abi` section, JSON) and conversion between JSON values and
//! BCS, driven by the type schema the compiler records.
//!
//! JSON conventions: integers as numbers (or decimal/hex strings; u128/u256 are printed as
//! strings), addresses and `vector<u8>` as `"0x…"` hex (a non-hex string is taken as UTF-8
//! bytes), structs as objects, enums as `"Variant"` or `{"Variant": {fields}}`. A struct
//! whose only field is a `vector<u8>` (e.g. `string::String`) is a plain JSON string.
//! Outputs are built as [`Out`] so struct fields keep their declaration order.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use num_bigint::BigUint;
use object::{Object, ObjectSection};
use serde_json::Value;

use crate::out::Out;

pub struct Abi {
    pub entry: String,
    pub params: Vec<String>,
    pub returns: Vec<String>,
    /// Types that may appear in effects (events, transferred objects).
    pub effects: Vec<String>,
    types: HashMap<String, TypeDef>,
}

enum TypeDef {
    Struct(Vec<(String, String)>),
    Enum(Vec<(String, Vec<(String, String)>)>),
}

fn fields(v: &Value) -> Result<Vec<(String, String)>> {
    v.as_array()
        .ok_or_else(|| anyhow!("bad field list"))?
        .iter()
        .map(|f| {
            Ok((
                f[0].as_str().context("field name")?.into(),
                f[1].as_str().context("field type")?.into(),
            ))
        })
        .collect()
}

impl Abi {
    pub fn from_elf(elf: &[u8]) -> Result<Abi> {
        let f = object::File::parse(elf)?;
        let sec = f
            .section_by_name(".move2sp1.abi")
            .ok_or_else(|| anyhow!("not a move2sp1 program (no .move2sp1.abi section)"))?;
        let j: Value = serde_json::from_slice(sec.data()?)?;
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        };
        let mut types = HashMap::new();
        for (name, def) in j["types"].as_object().into_iter().flatten() {
            let def = if let Some(fs) = def.get("struct") {
                TypeDef::Struct(fields(fs)?)
            } else {
                let vs = def["enum"].as_array().context("bad enum")?;
                TypeDef::Enum(
                    vs.iter()
                        .map(|v| {
                            Ok((
                                v[0].as_str().context("variant")?.to_string(),
                                fields(&v[1])?,
                            ))
                        })
                        .collect::<Result<_>>()?,
                )
            };
            types.insert(name.clone(), def);
        }
        Ok(Abi {
            entry: j["entry"].as_str().unwrap_or("?").into(),
            params: strings(&j["params"]),
            returns: strings(&j["returns"]),
            effects: strings(&j["effects"]),
            types,
        })
    }

    fn def(&self, ty: &str) -> Result<&TypeDef> {
        self.types
            .get(ty)
            .ok_or_else(|| anyhow!("unknown type {ty}"))
    }

    // ---- JSON -> BCS

    pub fn encode(&self, ty: &str, v: &Value, out: &mut Vec<u8>) -> Result<()> {
        if let Some(width) = int_width(ty) {
            return encode_int(ty, width, v, out);
        }
        match ty {
            "bool" => out.push(
                v.as_bool()
                    .ok_or_else(|| anyhow!("expected bool, got {v}"))? as u8,
            ),
            "address" => out.extend(address_bytes(v)?),
            "vector<u8>" if v.is_string() => {
                let b = bytes_of(v.as_str().unwrap())?;
                uleb128(b.len() as u64, out);
                out.extend(b);
            }
            _ if ty.starts_with(OPTION) => {
                // `null` or the value
                if v.is_null() {
                    out.push(0);
                } else {
                    out.push(1);
                    self.encode(&ty[OPTION.len()..ty.len() - 1], v, out)?;
                }
            }
            _ if ty.starts_with("vector<") => {
                let elem = &ty["vector<".len()..ty.len() - 1];
                let items = v
                    .as_array()
                    .ok_or_else(|| anyhow!("expected array for {ty}"))?;
                uleb128(items.len() as u64, out);
                for item in items {
                    self.encode(elem, item, out)?;
                }
            }
            _ => match self.def(ty)? {
                TypeDef::Struct(fs) => self.encode_fields(ty, fs, v, out)?,
                TypeDef::Enum(vs) => {
                    let (name, body) = match v {
                        Value::String(s) => (s.as_str(), Value::Null),
                        Value::Object(o) if o.len() == 1 => {
                            let (k, b) = o.iter().next().unwrap();
                            (k.as_str(), b.clone())
                        }
                        _ => bail!("expected \"Variant\" or {{\"Variant\": ...}} for {ty}"),
                    };
                    let tag = vs
                        .iter()
                        .position(|(n, _)| n == name)
                        .ok_or_else(|| anyhow!("{ty} has no variant {name}"))?;
                    uleb128(tag as u64, out);
                    self.encode_fields(ty, &vs[tag].1, &body, out)?;
                }
            },
        }
        Ok(())
    }

    fn encode_fields(
        &self,
        ty: &str,
        fs: &[(String, String)],
        v: &Value,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        // A single-field wrapper (`UID`, `Balance`, `String`, ...) may be given as its field's
        // value directly.
        if let [(name, ft)] = fs
            && !matches!(v, Value::Object(o) if o.contains_key(name))
        {
            return self.encode(ft, v, out);
        }
        for (i, (name, fty)) in fs.iter().enumerate() {
            let fv = match v {
                Value::Object(o) => o
                    .get(name)
                    .ok_or_else(|| anyhow!("{ty}: missing field {name}"))?,
                Value::Array(a) => a
                    .get(i)
                    .ok_or_else(|| anyhow!("{ty}: missing field #{i}"))?,
                _ if fs.is_empty() => break,
                _ => bail!("expected object for {ty}, got {v}"),
            };
            self.encode(fty, fv, out)
                .with_context(|| format!("{ty}.{name}"))?;
        }
        Ok(())
    }

    // ---- BCS -> JSON

    pub fn decode(&self, ty: &str, input: &mut &[u8]) -> Result<Out> {
        if let Some(width) = int_width(ty) {
            let n = BigUint::from_bytes_le(take(input, width)?).to_string();
            // u128/u256 exceed what JSON consumers handle as numbers: print as strings
            return Ok(if width <= 8 { Out::Num(n) } else { Out::Str(n) });
        }
        Ok(match ty {
            "bool" => Out::Bool(take(input, 1)?[0] != 0),
            "address" => Out::Str(hex(take(input, 32)?)),
            "vector<u8>" => {
                let n = read_uleb128(input)? as usize;
                Out::Str(hex(take(input, n)?))
            }
            _ if ty.starts_with(OPTION) => match read_uleb128(input)? {
                0 => Out::Null,
                _ => self.decode(&ty[OPTION.len()..ty.len() - 1], input)?,
            },
            _ if ty.starts_with("vector<") => {
                let elem = &ty["vector<".len()..ty.len() - 1];
                let n = read_uleb128(input)?;
                Out::Array(
                    (0..n)
                        .map(|_| self.decode(elem, input))
                        .collect::<Result<_>>()?,
                )
            }
            _ => match self.def(ty)? {
                TypeDef::Struct(fs) => self.decode_fields(fs, input)?,
                TypeDef::Enum(vs) => {
                    let tag = read_uleb128(input)? as usize;
                    let (name, fs) = vs
                        .get(tag)
                        .ok_or_else(|| anyhow!("bad tag {tag} for {ty}"))?;
                    if fs.is_empty() {
                        Out::Str(name.clone())
                    } else {
                        Out::Object(vec![(name.clone(), self.decode_fields(fs, input)?)])
                    }
                }
            },
        })
    }

    fn decode_fields(&self, fs: &[(String, String)], input: &mut &[u8]) -> Result<Out> {
        if let [(_, ft)] = fs
            && ft == "vector<u8>"
        {
            // String-like wrapper: show as text when it is valid UTF-8
            let n = read_uleb128(input)? as usize;
            let b = take(input, n)?;
            return Ok(Out::Str(
                std::str::from_utf8(b)
                    .map(String::from)
                    .unwrap_or_else(|_| hex(b)),
            ));
        }
        if let [(_, ft)] = fs {
            return self.decode(ft, input); // single-field wrapper: show its field
        }
        fs.iter()
            .map(|(name, fty)| Ok((name.clone(), self.decode(fty, input)?)))
            .collect::<Result<_>>()
            .map(Out::Object)
    }
}

impl Abi {
    /// Decode the effects committed after the results (see `runtime/sui/src/effects.rs`):
    /// `vector<(tag, type name, [recipient], bcs value)>`.
    pub fn decode_effects(&self, input: &mut &[u8]) -> Result<Out> {
        let n = read_uleb128(input)?;
        let mut effects = vec![];
        for _ in 0..n {
            let tag = take(input, 1)?[0];
            let len = read_uleb128(input)? as usize;
            let ty = String::from_utf8(take(input, len)?.to_vec())?;
            let mut e = vec![(
                "kind".to_string(),
                Out::Str(
                    ["event", "transfer", "share", "freeze"]
                        .get(tag as usize)
                        .unwrap_or(&"?")
                        .to_string(),
                ),
            )];
            e.push(("type".into(), Out::Str(ty.clone())));
            if tag == 1 {
                e.push(("recipient".into(), Out::Str(hex(take(input, 32)?))));
            }
            let len = read_uleb128(input)? as usize;
            let mut value = take(input, len)?;
            let decoded = if self.types.contains_key(&ty) {
                self.decode(&ty, &mut value)?
            } else {
                Out::Str(hex(value))
            };
            e.push(("value".into(), decoded));
            effects.push(Out::Object(e));
        }
        Ok(Out::Array(effects))
    }
}

const OPTION: &str = "option::Option<";

fn int_width(ty: &str) -> Option<usize> {
    Some(match ty {
        "u8" => 1,
        "u16" => 2,
        "u32" => 4,
        "u64" => 8,
        "u128" => 16,
        "u256" => 32,
        _ => return None,
    })
}

fn encode_int(ty: &str, width: usize, v: &Value, out: &mut Vec<u8>) -> Result<()> {
    let n = match v {
        Value::Number(n) => n
            .as_u64()
            .map(BigUint::from)
            .ok_or_else(|| anyhow!("{n} is not a {ty}"))?,
        Value::String(s) => match s.strip_prefix("0x") {
            Some(h) => BigUint::parse_bytes(h.as_bytes(), 16),
            None => BigUint::parse_bytes(s.as_bytes(), 10),
        }
        .ok_or_else(|| anyhow!("invalid {ty}: {s}"))?,
        _ => bail!("expected {ty}, got {v}"),
    };
    let mut b = n.to_bytes_le();
    ensure!(b.len() <= width, "{n} does not fit in {ty}");
    b.resize(width, 0);
    out.extend(b);
    Ok(())
}

/// `"0xCAFE"` -> 32 bytes, left-padded (as written, most significant byte first).
fn address_bytes(v: &Value) -> Result<Vec<u8>> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("expected address string, got {v}"))?;
    let h = s.trim_start_matches('@').trim_start_matches("0x");
    ensure!(h.len() <= 64, "address too long: {s}");
    let h = format!("{h:0>64}");
    (0..32)
        .map(|i| {
            u8::from_str_radix(&h[2 * i..2 * i + 2], 16).with_context(|| format!("bad address {s}"))
        })
        .collect()
}

/// `"0x0102"` -> hex bytes; anything else -> its UTF-8 bytes.
fn bytes_of(s: &str) -> Result<Vec<u8>> {
    match s.strip_prefix("0x") {
        Some(h) if h.len() % 2 == 0 && h.bytes().all(|c| c.is_ascii_hexdigit()) => Ok((0..h.len()
            / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect()),
        _ => Ok(s.as_bytes().to_vec()),
    }
}

fn hex(b: &[u8]) -> String {
    format!(
        "0x{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
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

fn read_uleb128(input: &mut &[u8]) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = take(input, 1)?[0];
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("invalid ULEB128")
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    ensure!(input.len() >= n, "public values too short");
    let (h, t) = input.split_at(n);
    *input = t;
    Ok(h)
}
