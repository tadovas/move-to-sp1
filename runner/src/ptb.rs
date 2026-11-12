//! PTBs (programmable transaction blocks) for program images: JSON in, the runtime's binary
//! encoding out (`runtime/sui/src/ptb.rs`).
//!
//! ```json
//! { "gas":      {"price": 1000, "rgp": 1000, "budget": 0, "coins": ["0x…"], "charge": 0, "refund": 0, "owner": "0x…"},
//!   "inputs":   [ {"object": "0x…"}, {"pure": "0x2a00000000000000"}, {"value": 42, "type": "u64"},
//!                 {"reservation": {"coin": "0x2::coin::Coin<0x2::sui::SUI>", "amount": 5}} ],
//!   "commands": [ {"MoveCall": {"function": "0x…::pool::place_market_order<0x…::ns::NS, 0x…::usdc::USDC>",
//!                               "args": [{"Input": 0}, {"Result": 0}, {"NestedResult": [1, 0]}, "GasCoin"]}},
//!                 {"TransferObjects": {"objects": [{"Result": 0}], "address": {"Input": 1}}},
//!                 {"SplitCoins": {"coin": "GasCoin", "amounts": [{"Input": 2}]}},
//!                 {"MergeCoins": {"coin": {"Input": 0}, "sources": [{"Result": 2}]}},
//!                 {"MakeMoveVec": {"type": "u64", "elements": [{"Input": 2}]}} ] }
//! ```
//! `gas` is optional (all 0, as in Sui's unit tests): `price`/`rgp`/`budget` are what
//! `tx_context` reports; gas is not metered — `coins` are smashed into the first, which is
//! debited `charge` and credited `refund` after the commands (amounts supplied by the caller).
//! Pure inputs are raw BCS (`0x` hex or `base64:`), or a JSON value with its type.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::abi::Abi;

pub fn encode(abi: &Abi, ptb: &Value) -> Result<Vec<u8>> {
    let mut out = vec![];
    let gas = &ptb["gas"];
    for k in ["price", "rgp", "budget"] {
        out.extend(gas[k].as_u64().unwrap_or(0).to_le_bytes());
    }
    let coins = gas["coins"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    uleb(coins.len() as u64, &mut out);
    for c in coins {
        out.extend(address(c.as_str().context("gas coin")?)?);
    }
    for k in ["charge", "refund"] {
        out.extend(gas[k].as_u64().unwrap_or(0).to_le_bytes());
    }
    out.extend(match gas["owner"].as_str() {
        Some(o) => address(o)?,
        None => vec![0; 32],
    });
    let inputs = ptb["inputs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    uleb(inputs.len() as u64, &mut out);
    for input in inputs {
        if let Some(id) = input["object"].as_str() {
            out.push(0);
            out.extend(address(id)?);
        } else if let Some(r) = input.get("reservation") {
            out.push(2);
            string(
                &canonical_function(r["coin"].as_str().context("reservation coin")?)?,
                &mut out,
            );
            out.extend(
                r["amount"]
                    .as_u64()
                    .context("reservation amount")?
                    .to_le_bytes(),
            );
        } else {
            out.push(1);
            let bytes = pure(abi, input).with_context(|| format!("pure input {input}"))?;
            uleb(bytes.len() as u64, &mut out);
            out.extend(bytes);
        }
    }
    let commands = ptb["commands"].as_array().context("PTB has no commands")?;
    uleb(commands.len() as u64, &mut out);
    for c in commands {
        if let Some(call) = c.get("MoveCall") {
            out.push(0);
            let key = canonical_function(call["function"].as_str().context("MoveCall.function")?)?;
            string(&key, &mut out);
            args(&call["args"], &mut out)?;
        } else if let Some(t) = c.get("TransferObjects") {
            out.push(1);
            args(&t["objects"], &mut out)?;
            arg(&t["address"], &mut out)?;
        } else if let Some(s) = c.get("SplitCoins") {
            out.push(2);
            arg(&s["coin"], &mut out)?;
            args(&s["amounts"], &mut out)?;
        } else if let Some(m) = c.get("MergeCoins") {
            out.push(3);
            arg(&m["coin"], &mut out)?;
            args(&m["sources"], &mut out)?;
        } else if let Some(v) = c.get("MakeMoveVec") {
            out.push(4);
            match v["type"].as_str() {
                Some(t) => {
                    out.push(1);
                    string(&canonical_function(t)?, &mut out);
                }
                None => out.push(0),
            }
            args(&v["elements"], &mut out)?;
        } else {
            bail!("unsupported command {c}");
        }
    }
    Ok(out)
}

fn string(s: &str, out: &mut Vec<u8>) {
    uleb(s.len() as u64, out);
    out.extend(s.as_bytes());
}

fn args(list: &Value, out: &mut Vec<u8>) -> Result<()> {
    let list = list.as_array().map(Vec::as_slice).unwrap_or_default();
    uleb(list.len() as u64, out);
    for a in list {
        arg(a, out)?;
    }
    Ok(())
}

fn arg(a: &Value, out: &mut Vec<u8>) -> Result<()> {
    if a == "GasCoin" {
        out.push(0);
    } else if let Some(i) = a["Input"].as_u64() {
        out.push(1);
        out.extend((i as u16).to_le_bytes());
    } else if let Some(i) = a["Result"].as_u64() {
        out.push(2);
        out.extend((i as u16).to_le_bytes());
    } else if let Some([c, i]) = a["NestedResult"].as_array().map(Vec::as_slice) {
        out.push(3);
        out.extend((c.as_u64().context("NestedResult")? as u16).to_le_bytes());
        out.extend((i.as_u64().context("NestedResult")? as u16).to_le_bytes());
    } else {
        bail!("bad argument {a}");
    }
    Ok(())
}

fn pure(abi: &Abi, input: &Value) -> Result<Vec<u8>> {
    if let Some(s) = input["pure"].as_str() {
        return match s.strip_prefix("base64:") {
            Some(b) => base64_decode(b),
            None => hex(s.trim_start_matches("0x")),
        };
    }
    let ty = input["type"]
        .as_str()
        .context("pure input needs \"pure\" bytes or \"value\" + \"type\"")?;
    let mut out = vec![];
    abi.encode(ty, &input["value"], &mut out)?;
    Ok(out)
}

/// `0x2::m::f<0x2::sui::SUI, u64>` -> 64-hex addresses without `0x`, no spaces (the
/// compiler's dispatch keys).
pub fn canonical_function(f: &str) -> Result<String> {
    let mut out = String::new();
    for token in f.split_inclusive(['<', '>', ',']) {
        let (body, sep) = match token.chars().last() {
            Some(c @ ('<' | '>' | ',')) => (&token[..token.len() - 1], Some(c)),
            _ => (token, None),
        };
        let body = body.trim();
        let body = match body.split_once("::") {
            Some((addr, rest)) if addr.starts_with("0x") || addr.len() == 64 => {
                format!("{:0>64}::{rest}", addr.trim_start_matches("0x"))
            }
            _ => body.to_string(),
        };
        out.push_str(&body);
        if let Some(c) = sep {
            out.push(c);
        }
    }
    Ok(out)
}

fn address(s: &str) -> Result<Vec<u8>> {
    let h = s.trim_start_matches("0x");
    hex(&format!("{h:0>64}"))
}

fn hex(h: &str) -> Result<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        bail!("odd-length hex {h}");
    }
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(Into::into))
        .collect()
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut acc, mut bits) = (vec![], 0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=') {
        let v = A
            .iter()
            .position(|&a| a == c)
            .ok_or_else(|| anyhow!("bad base64"))? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

fn uleb(mut v: u64, out: &mut Vec<u8>) {
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
