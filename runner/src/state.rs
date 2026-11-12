//! State snapshots as data: parse and build the runtime's snapshot encoding
//! (`runtime/sui/src/state.rs`), e.g. to assemble an input state from chain objects or to
//! compare an output state with chain effects.

use anyhow::{Result, bail, ensure};

/// How an object is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Owned by an account (`owner`).
    Owned,
    Shared,
    /// A dynamic-field child of the object `owner`.
    Child,
}

/// One object of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateObject {
    pub kind: Kind,
    pub id: [u8; 32],
    /// Owner account or parent object (zero for shared objects).
    pub owner: [u8; 32],
    /// Canonical type name (64-hex addresses without `0x`, no spaces).
    pub type_name: String,
    /// BCS of the Move value.
    pub bcs: Vec<u8>,
}

/// Parse a snapshot (without a partial-state trailer).
pub fn parse(snapshot: &[u8]) -> Result<Vec<StateObject>> {
    let mut input = snapshot;
    if input.is_empty() {
        return Ok(vec![]);
    }
    let n = crate::abi::read_uleb(&mut input)?;
    let mut objects = vec![];
    for _ in 0..n {
        let kind = match take(&mut input, 1)?[0] {
            0 => Kind::Owned,
            1 => Kind::Shared,
            2 => Kind::Child,
            k => bail!("bad object kind {k}"),
        };
        let id = take(&mut input, 32)?.try_into()?;
        let owner = take(&mut input, 32)?.try_into()?;
        let type_name = String::from_utf8(crate::abi::read_bytes(&mut input)?.to_vec())?;
        let bcs = crate::abi::read_bytes(&mut input)?.to_vec();
        objects.push(StateObject {
            kind,
            id,
            owner,
            type_name,
            bcs,
        });
    }
    ensure!(input.is_empty(), "trailing bytes after snapshot");
    Ok(objects)
}

/// Encode a snapshot (sorted by ID, as the runtime does). `absent`: a *partial* state, whose
/// dynamic-field children not in `objects` are fetched on demand, except these IDs, known
/// not to exist (see `runtime/sui/src/store.rs`, partial mode).
pub fn encode(objects: &[StateObject], absent: Option<&[[u8; 32]]>) -> Vec<u8> {
    let mut sorted: Vec<&StateObject> = objects.iter().collect();
    sorted.sort_by_key(|o| o.id);
    let mut out = vec![];
    uleb(sorted.len() as u64, &mut out);
    for o in sorted {
        out.push(match o.kind {
            Kind::Owned => 0,
            Kind::Shared => 1,
            Kind::Child => 2,
        });
        out.extend(o.id);
        out.extend(o.owner);
        uleb(o.type_name.len() as u64, &mut out);
        out.extend(o.type_name.as_bytes());
        uleb(o.bcs.len() as u64, &mut out);
        out.extend(&o.bcs);
    }
    if let Some(absent) = absent {
        out.push(1);
        uleb(absent.len() as u64, &mut out);
        for id in absent {
            out.extend(id);
        }
    }
    out
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    ensure!(input.len() >= n, "snapshot too short");
    let (h, t) = input.split_at(n);
    *input = t;
    Ok(h)
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
