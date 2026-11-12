//! Reading Sui mainnet through its public GraphQL API: the transaction and its certified
//! effects, Move packages, and objects at given versions.
//!
//! Every response is cached on disk (keyed by the query and its variables), so a replay can
//! be rerun offline and its inputs inspected.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use serde_json::{Value, json};
use sha2::Digest;

pub const MAINNET: &str = "https://graphql.mainnet.sui.io/graphql";

pub struct Chain {
    http: reqwest::Client,
    url: String,
    cache: PathBuf,
    /// Requests that went to the network (not served from the cache).
    pub fetched: usize,
}

impl Chain {
    pub fn new(url: &str, cache: PathBuf) -> Result<Chain> {
        std::fs::create_dir_all(&cache)?;
        Ok(Chain {
            http: reqwest::Client::builder()
                .user_agent("sui-replay")
                .build()?,
            url: url.into(),
            cache,
            fetched: 0,
        })
    }

    async fn query(&mut self, query: &str, vars: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": vars });
        let key = hex(&sha2::Sha256::digest(body.to_string().as_bytes())[..12]);
        let path = self.cache.join(format!("{key}.json"));
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        let resp: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .context("GraphQL request")?
            .error_for_status()?
            .json()
            .await?;
        if let Some(e) = resp.get("errors") {
            bail!("GraphQL error: {e}");
        }
        let data = resp["data"].clone();
        self.fetched += 1;
        std::fs::write(&path, serde_json::to_vec_pretty(&data)?)?;
        Ok(data)
    }

    /// The transaction, its effects (JSON), events and object changes.
    pub async fn transaction(&mut self, digest: &str) -> Result<Value> {
        let q = format!(
            r#"query($d: String!) {{ transaction(digest: $d) {{
                digest sender {{ address }} transactionJson
                effects {{
                    lamportVersion timestamp status effectsJson
                    checkpoint {{ sequenceNumber }}
                    epoch {{ epochId referenceGasPrice startTimestamp }}
                    {EVENTS}
                    {CHANGES}
                }}
            }} }}"#
        );
        let d = self.query(&q, json!({ "d": digest })).await?;
        let mut tx = d["transaction"].clone();
        ensure!(!tx.is_null(), "transaction {digest} not found");
        // further pages of events and object changes
        for (field, selection) in [("events", EVENTS), ("objectChanges", CHANGES)] {
            loop {
                let page = &tx["effects"][field]["pageInfo"];
                if page["hasNextPage"] != true {
                    break;
                }
                let after = page["endCursor"].clone();
                let q = format!(
                    r#"query($d: String!, $after: String) {{ transaction(digest: $d) {{ effects {{
                        {} }} }} }}"#,
                    selection.replace("(first: 50)", "(first: 50, after: $after)")
                );
                let d = self
                    .query(&q, json!({ "d": digest, "after": after }))
                    .await?;
                let next = &d["transaction"]["effects"][field];
                let nodes = next["nodes"].as_array().context("page nodes")?.clone();
                let all = tx["effects"][field]["nodes"]
                    .as_array_mut()
                    .context("nodes")?;
                all.extend(nodes);
                tx["effects"][field]["pageInfo"] = next["pageInfo"].clone();
            }
        }
        Ok(tx)
    }

    /// Objects at exact versions; `None` for an object that does not exist.
    pub async fn objects(&mut self, keys: &[(String, u64)]) -> Result<Vec<Option<ChainObject>>> {
        let mut out = vec![];
        for chunk in keys.chunks(40) {
            let keys: Vec<Value> = chunk
                .iter()
                .map(|(a, v)| json!({ "address": a, "version": v }))
                .collect();
            let q =
                format!("query($k: [ObjectKey!]!) {{ multiGetObjects(keys: $k) {{ {OBJECT} }} }}");
            let d = self.query(&q, json!({ "k": keys })).await?;
            for o in d["multiGetObjects"].as_array().context("multiGetObjects")? {
                out.push(ChainObject::parse(o)?);
            }
        }
        Ok(out)
    }

    /// A dynamic-field child as of a root version (the latest version not after it).
    pub async fn child_at(&mut self, id: &str, root_version: u64) -> Result<Option<ChainObject>> {
        let q = format!(
            "query($a: SuiAddress!, $v: UInt53!) {{ object(address: $a, rootVersion: $v) {{ {OBJECT} }} }}"
        );
        let d = self
            .query(&q, json!({ "a": id, "v": root_version }))
            .await?;
        ChainObject::parse(&d["object"])
    }

    /// A package (a given version if `version` is set): its modules, linkage and type origins.
    /// A system package (`0x1`, `0x2`, ...: upgraded in place) as of a checkpoint: transactions
    /// run against the current framework, whatever version a package's linkage names.
    pub async fn system_package(&mut self, address: &str, checkpoint: u64) -> Result<Package> {
        let q = "query($a: SuiAddress!, $c: UInt53) { package(address: $a, atCheckpoint: $c) { version } }";
        let d = self
            .query(q, json!({ "a": address, "c": checkpoint }))
            .await?;
        let v = d["package"]["version"]
            .as_u64()
            .with_context(|| format!("system package {address} not found"))?;
        self.package(address, Some(v)).await
    }

    pub async fn package(&mut self, address: &str, version: Option<u64>) -> Result<Package> {
        // `package(address)` alone resolves to the *latest* version of the package's family;
        // a storage ID names one exact version, which `object(address)` gives
        let version = match version {
            Some(v) => Some(v),
            None => {
                let q = "query($a: SuiAddress!) { object(address: $a) { version } }";
                let d = self.query(q, json!({ "a": address })).await?;
                Some(
                    d["object"]["version"]
                        .as_u64()
                        .with_context(|| format!("package {address} not found"))?,
                )
            }
        };
        let q = r#"query($a: SuiAddress!, $v: UInt53) { package(address: $a, version: $v) {
            linkage { originalId upgradedId version }
            typeOrigins { module struct definingId }
        } }"#;
        let d = self.query(q, json!({ "a": address, "v": version })).await?;
        let p = &d["package"];
        ensure!(!p.is_null(), "package {address} not found");
        let mut modules = vec![];
        let mut after: Option<String> = None;
        loop {
            let q = r#"query($a: SuiAddress!, $v: UInt53, $after: String) {
                package(address: $a, version: $v) {
                    modules(first: 50, after: $after) {
                        pageInfo { hasNextPage endCursor } nodes { bytes }
                    }
                } }"#;
            let d = self
                .query(q, json!({ "a": address, "v": version, "after": after }))
                .await?;
            let page = &d["package"]["modules"];
            for m in page["nodes"].as_array().context("modules")? {
                modules.push(b64(m["bytes"].as_str().context("module bytes")?)?);
            }
            if page["pageInfo"]["hasNextPage"] != true {
                break;
            }
            after = page["pageInfo"]["endCursor"].as_str().map(String::from);
        }
        let linkage = p["linkage"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| {
                Ok(Linkage {
                    original: l["originalId"].as_str().context("originalId")?.into(),
                    upgraded: l["upgradedId"].as_str().context("upgradedId")?.into(),
                    version: l["version"].as_u64().context("version")?,
                })
            })
            .collect::<Result<_>>()?;
        let type_origins = p["typeOrigins"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                Ok((
                    t["module"].as_str().context("module")?.to_string(),
                    t["struct"].as_str().context("struct")?.to_string(),
                    t["definingId"].as_str().context("definingId")?.to_string(),
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Package {
            version: version.unwrap_or(0),
            modules,
            linkage,
            type_origins,
        })
    }
}

const EVENTS: &str = "events(first: 50) { pageInfo { hasNextPage endCursor } nodes { contents { type { repr } bcs } } }";
const CHANGES: &str = concat!(
    "objectChanges(first: 50) { pageInfo { hasNextPage endCursor } nodes { address idCreated idDeleted outputState { ",
    "address version objectBcs owner { __typename ... on AddressOwner { address { address } } ... on ObjectOwner { address { address } } ... on ConsensusAddressOwner { address { address } } } asMoveObject { contents { type { repr } bcs } }",
    " } } }"
);

/// Object fields selected everywhere.
const OBJECT: &str = r#"address version objectBcs
    owner { __typename
        ... on AddressOwner { address { address } }
        ... on ObjectOwner { address { address } }
        ... on ConsensusAddressOwner { address { address } } }
    asMoveObject { contents { type { repr } bcs } }"#;

/// An object as read from chain.
#[derive(Clone, Debug)]
pub struct ChainObject {
    pub id: String,
    pub version: u64,
    pub owner: Owner,
    /// Type as Sui prints it (`0x…::m::T<0x…::n::U>`).
    pub type_repr: String,
    /// BCS of the Move value.
    pub bcs: Vec<u8>,
    /// BCS of the whole object (owner, previous transaction, storage rebate, contents): its
    /// digest is the hash of this.
    pub object_bcs: Vec<u8>,
}

impl ChainObject {
    /// Whether the object is the one with this (certified) digest: Blake2b-256 of
    /// `"Object::" || object BCS` equals it, and the Move value used is the one inside.
    pub fn verify(&self, certified: &str) -> bool {
        use blake2::{Blake2b, Digest, digest::consts::U32};
        let mut h = Blake2b::<U32>::new();
        h.update(b"Object::");
        h.update(&self.object_bcs);
        let digest = bs58::encode(h.finalize()).into_string();
        let mut contents = uleb(self.bcs.len() as u64);
        contents.extend(&self.bcs);
        digest == certified
            && self
                .object_bcs
                .windows(contents.len())
                .any(|w| w == contents.as_slice())
    }
}

fn uleb(mut v: u64) -> Vec<u8> {
    let mut out = vec![];
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    Address(String),
    /// A dynamic-field child (or other object-owned object) of this object.
    Object(String),
    Shared,
    Immutable,
}

impl ChainObject {
    pub fn parse(o: &Value) -> Result<Option<ChainObject>> {
        if o.is_null() {
            return Ok(None);
        }
        let owner_addr = || -> Result<String> {
            Ok(o["owner"]["address"]["address"]
                .as_str()
                .context("owner address")?
                .to_string())
        };
        let owner = match o["owner"]["__typename"].as_str() {
            Some("AddressOwner" | "ConsensusAddressOwner") => Owner::Address(owner_addr()?),
            Some("ObjectOwner") => Owner::Object(owner_addr()?),
            Some("Shared") => Owner::Shared,
            Some("Immutable") => Owner::Immutable,
            t => bail!("unknown owner kind {t:?}"),
        };
        let contents = &o["asMoveObject"]["contents"];
        ensure!(!contents.is_null(), "{} is not a Move object", o["address"]);
        Ok(Some(ChainObject {
            id: o["address"].as_str().context("address")?.into(),
            version: o["version"].as_u64().context("version")?,
            owner,
            type_repr: contents["type"]["repr"].as_str().context("type")?.into(),
            bcs: b64(contents["bcs"].as_str().context("bcs")?)?,
            object_bcs: b64(o["objectBcs"].as_str().unwrap_or_default())?,
        }))
    }
}

pub struct Package {
    pub version: u64,
    pub modules: Vec<Vec<u8>>,
    pub linkage: Vec<Linkage>,
    /// (module, struct, defining ID)
    pub type_origins: Vec<(String, String, String)>,
}

pub struct Linkage {
    pub original: String,
    pub upgraded: String,
    pub version: u64,
}

pub fn b64(s: &str) -> Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An object selected with the common object fields (e.g. an effects' output state).
pub fn parse_object(o: &Value) -> Result<Option<ChainObject>> {
    ChainObject::parse(o)
}
