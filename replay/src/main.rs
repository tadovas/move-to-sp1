//! `sui-replay`: re-execute a Sui mainnet transaction in the SP1 zkVM and check the result
//! byte for byte against the chain's certified effects.
//!
//! 1. Fetch the transaction (its PTB) and effects through GraphQL.
//! 2. Fetch the bytecode of the called packages, their linkage, and the packages of the type
//!    arguments; compile a program image of exactly the function instances the PTB calls.
//! 3. Assemble the input state: every object the effects say the transaction read or wrote, at
//!    its input version. Objects it created are known absent.
//! 4. Execute. A dynamic-field child the effects do not list (a lookup of a child that may or
//!    may not exist) stops the run with `move2sp1::missing::<id>`; it is fetched as of the
//!    transaction's root version and the run repeated ("discovery" rounds, partial state).
//! 5. Run once more on the complete state (no partial mode) — the run that would be proven —
//!    and compare its written objects and events with the chain's.

mod chain;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result, bail, ensure};
use chain::{Chain, ChainObject, Owner, hex};
use clap::Parser;
use move_core_types::account_address::AccountAddress;
use movevm::{
    Executor, Inputs, Outcome,
    state::{Kind, StateObject},
};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    name = "sui-replay",
    about = "Replay a Sui mainnet transaction in the SP1 zkVM and compare with chain"
)]
struct Cli {
    /// Transaction digest (base58).
    digest: String,
    /// GraphQL endpoint.
    #[arg(long, default_value = chain::MAINNET)]
    url: String,
    /// Output directory (state, PTB, ELF, report go to `<out>/<digest>/`; responses are cached
    /// in `<out>/cache/`, shared by all replays).
    #[arg(long, default_value = "out/replay")]
    out: PathBuf,
    /// Give up after this many discovery rounds.
    #[arg(long, default_value_t = 64)]
    max_rounds: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut chain = Chain::new(&cli.url, cli.out.join("cache"))?;
    let dir = cli.out.join(&cli.digest);
    std::fs::create_dir_all(&dir)?;

    // 1. the transaction
    let tx = chain.transaction(&cli.digest).await?;
    let data = &tx["transactionJson"];
    let effects = &tx["effects"];
    let fx = &effects["effectsJson"];
    let pt = &data["kind"]["programmableTransaction"];
    ensure!(
        data["kind"]["kind"] == "PROGRAMMABLE_TRANSACTION",
        "not a programmable transaction ({})",
        data["kind"]["kind"]
    );
    ensure!(
        fx["status"]["success"] == true,
        "the transaction failed on chain; replaying failures is not supported yet"
    );
    let lamport = num(&effects["lamportVersion"]).context("lamportVersion")?;
    println!(
        "tx {} (checkpoint {}, {}), sender {}",
        cli.digest,
        effects["checkpoint"]["sequenceNumber"],
        effects["timestamp"].as_str().unwrap_or("?"),
        short(tx["sender"]["address"].as_str().unwrap_or("?")),
    );

    // 2. packages and the image
    let t = std::time::Instant::now();
    let commands = pt["commands"].as_array().context("commands")?;
    let mut calls = vec![]; // (storage id, module, function, type args)
    for c in commands {
        let Some(m) = c.get("moveCall") else {
            bail!("unsupported command {c} (only MoveCall so far)")
        };
        let targs: Vec<String> = m["typeArguments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        calls.push((
            norm(m["package"].as_str().context("package")?),
            m["module"].as_str().context("module")?.to_string(),
            m["function"].as_str().context("function")?.to_string(),
            targs,
        ));
    }
    let packages = Packages::fetch(&mut chain, &calls).await?;
    let specs: Vec<String> = calls
        .iter()
        .map(|(pkg, module, function, targs)| {
            let original = &packages.original_of[pkg];
            let targs = if targs.is_empty() {
                String::new()
            } else {
                format!("<{}>", targs.join(","))
            };
            format!("{original}::{module}::{function}{targs}")
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let compiled = move2sp1::load_bytecode(&packages.roots, &packages.deps)?;
    let program = move2sp1::build(
        &compiled,
        move2sp1::Entry::Image(&specs),
        &move2sp1::Options::default(),
    )
    .context("compiling the image")?;
    let elf = program.elf;
    std::fs::write(dir.join("image.elf"), &elf)?;
    println!(
        "image: {} function instance(s) from {} package(s) ({} modules) -> {} KiB ELF in {:.1?}",
        specs.len(),
        packages.roots.len() + packages.deps.len(),
        compiled.targets.len() + compiled.deps.len(),
        elf.len() / 1024,
        t.elapsed()
    );
    for s in &specs {
        println!("  {}", short_function(s));
    }
    let abi = movevm::Abi::from_elf(&elf)?;

    // 3. input state: what the effects say was read or written, at input versions
    let mut keys: Vec<(String, u64)> = vec![];
    let mut absent: Vec<[u8; 32]> = vec![];
    for c in fx["changedObjects"].as_array().context("changedObjects")? {
        let id = c["objectId"].as_str().context("objectId")?;
        match (c["inputState"].as_str(), c["idOperation"].as_str()) {
            (Some("INPUT_OBJECT_STATE_EXISTS"), _) => {
                keys.push((id.into(), num(&c["inputVersion"]).context("inputVersion")?))
            }
            (_, Some("CREATED")) => absent.push(addr(id)?),
            _ => {} // e.g. address-balance (accumulator) writes: gas, not modelled
        }
    }
    for list in ["unchangedConsensusObjects", "unchangedLoadedRuntimeObjects"] {
        for o in fx[list].as_array().into_iter().flatten() {
            if let (Some(id), Some(v)) = (o["objectId"].as_str(), num(&o["version"])) {
                keys.push((id.into(), v));
            }
        }
    }
    for i in pt["inputs"].as_array().context("inputs")? {
        match i["kind"].as_str() {
            Some("PURE" | "SHARED") => {}
            Some("IMMUTABLE_OR_OWNED") => keys.push((
                i["objectId"].as_str().context("objectId")?.into(),
                num(&i["version"]).context("version")?,
            )),
            k => bail!("unsupported input kind {k:?}"),
        }
    }
    keys.sort();
    keys.dedup();
    let mut objects = vec![];
    for (o, (id, v)) in chain.objects(&keys).await?.into_iter().zip(&keys) {
        objects.push(state_object(
            &o.with_context(|| format!("{id} v{v} not found"))?,
        )?);
    }
    println!(
        "state: {} object(s) read or written (from effects), {} created",
        objects.len(),
        absent.len()
    );

    // the transaction context and the PTB
    let epoch = &effects["epoch"];
    let ctx = json!({
        "sender": tx["sender"]["address"],
        "tx_hash": format!("0x{}", hex(&bs58::decode(&cli.digest).into_vec()?)),
        "epoch": epoch["epochId"],
        "epoch_timestamp_ms": iso_ms(epoch["startTimestamp"].as_str().context("epoch start")?)?,
        "ids_created": 0,
    })
    .to_string();
    let ptb = ptb_json(pt, &data["gasPayment"], epoch, &calls, &packages)?;
    std::fs::write(dir.join("ctx.json"), &ctx)?;
    std::fs::write(dir.join("ptb.json"), serde_json::to_string_pretty(&ptb)?)?;

    // 4. discovery: run on the partial state until nothing is missing
    let executor = Executor::new().await;
    let mut rounds = 0;
    loop {
        rounds += 1;
        ensure!(
            rounds <= cli.max_rounds,
            "still missing objects after {} rounds",
            cli.max_rounds
        );
        let state = movevm::state::encode(&objects, Some(&absent));
        let inputs = Inputs::for_image(&abi, state, &ctx, &ptb)?;
        let run = executor.execute(&elf, &inputs).await?;
        let missing = match &run.outcome {
            Outcome::Abort { location, .. } => location.strip_prefix("move2sp1::missing::"),
            _ => None,
        };
        let Some(id) = missing.map(|h| format!("0x{h}")) else {
            break;
        };
        match chain.child_at(&id, lamport - 1).await? {
            Some(o) => {
                println!("  round {rounds}: fetched {} (v{})", short(&id), o.version);
                objects.push(state_object(&o)?);
            }
            None => {
                println!("  round {rounds}: {} does not exist", short(&id));
                absent.push(addr(&id)?);
            }
        }
    }
    println!(
        "discovery: {rounds} run(s), {} object(s) in the final input state; {} GraphQL request(s) (rest cached)",
        objects.len(),
        chain.fetched
    );

    // 5. the run to prove: complete state, no partial mode
    let state_in = movevm::state::encode(&objects, None);
    std::fs::write(dir.join("state_in.bin"), &state_in)?;
    let inputs = Inputs::for_image(&abi, state_in.clone(), &ctx, &ptb)?;
    let t = std::time::Instant::now();
    let run = executor.execute(&elf, &inputs).await?;
    let elapsed = t.elapsed();
    let output = match run.outcome {
        Outcome::Success(o) => o,
        Outcome::Abort { code, location } => bail!("aborted with {code} in {location}"),
        Outcome::VmError { status, location } => bail!("VM error {status} in {location}"),
        Outcome::Failed(e) => bail!("program exited with {e}"),
    };
    std::fs::write(dir.join("state_out.bin"), &output.state_out)?;
    println!(
        "executed in SP1: {} cycles ({elapsed:.2?}); input state sha256 {}",
        run.cycles,
        hex(&movevm::sha256(&state_in))
    );

    let ok = compare(&objects, &output, effects)?;
    let gas = &fx["gasUsed"];
    println!(
        "gas (from chain, not computed by the replay): computation {}, storage {}, rebate {}",
        gas["computationCost"], gas["storageCost"], gas["storageRebate"]
    );
    ensure!(ok, "replay differs from chain");
    println!("MATCH: every written object and event is byte-identical to mainnet");
    Ok(())
}

/// Compare the replay's written objects and events with the chain's.
fn compare(input: &[StateObject], output: &movevm::Output, effects: &Value) -> Result<bool> {
    let ours: BTreeMap<[u8; 32], StateObject> = movevm::state::parse(&output.state_out)?
        .into_iter()
        .map(|o| (o.id, o))
        .collect();
    let before: BTreeMap<[u8; 32], &StateObject> = input.iter().map(|o| (o.id, o)).collect();
    let mut ok = true;
    let mut changed = BTreeSet::new();
    println!("objects written (chain vs replay):");
    for c in effects["objectChanges"]["nodes"]
        .as_array()
        .context("objectChanges")?
    {
        let id = addr(c["address"].as_str().context("address")?)?;
        changed.insert(id);
        let out = &c["outputState"];
        let (verdict, what) = if out.is_null() || c["idDeleted"] == true {
            match ours.get(&id) {
                None => ("ok  ", "deleted".to_string()),
                Some(_) => ("DIFF", "deleted on chain, still present in replay".into()),
            }
        } else {
            let want = state_object(&chain_object(out)?.context("output object")?)?;
            match ours.get(&id) {
                None => ("DIFF", "missing in replay".into()),
                Some(got) if *got == want => (
                    "ok  ",
                    format!("{} bytes, {}", want.bcs.len(), short_type(&want.type_name)),
                ),
                Some(got) => ("DIFF", describe_diff(&want, got)),
            }
        };
        ok &= verdict.starts_with("ok");
        let created = if c["idCreated"] == true {
            " (created)"
        } else {
            ""
        };
        println!(
            "  {verdict} {}{created}  {what}",
            short(&format!("0x{}", hex(&id)))
        );
    }
    // nothing else may change
    let mut untouched = 0;
    for (id, o) in &ours {
        if changed.contains(id) {
            continue;
        }
        match before.get(id) {
            Some(b) if *b == o => untouched += 1,
            _ => {
                ok = false;
                println!("  DIFF 0x{}: written by the replay, not on chain", hex(id));
            }
        }
    }
    println!("  {untouched} object(s) only read: unchanged in the replay too");

    println!("events (chain vs replay):");
    let want: Vec<(String, Vec<u8>)> = effects["events"]["nodes"]
        .as_array()
        .context("events")?
        .iter()
        .map(|e| {
            Ok((
                canonical_type(e["contents"]["type"]["repr"].as_str().context("type")?)?,
                chain::b64(e["contents"]["bcs"].as_str().context("bcs")?)?,
            ))
        })
        .collect::<Result<_>>()?;
    let got: Vec<(String, Vec<u8>)> = output
        .effect_records
        .iter()
        .filter(|r| r.tag == 0)
        .map(|r| (r.type_name.clone(), r.bcs.clone()))
        .collect();
    for i in 0..want.len().max(got.len()) {
        let line = match (want.get(i), got.get(i)) {
            (Some(w), Some(g)) if w == g => {
                format!("ok   {} ({} bytes)", short_type(&w.0), w.1.len())
            }
            (Some(w), Some(g)) if w.0 == g.0 => {
                format!("DIFF {}: bytes differ", short_type(&w.0))
            }
            (Some(w), g) => format!(
                "DIFF chain {} vs replay {}",
                short_type(&w.0),
                g.map(|g| short_type(&g.0)).unwrap_or("nothing".into())
            ),
            (None, Some(g)) => format!("DIFF extra event in replay: {}", short_type(&g.0)),
            (None, None) => unreachable!(),
        };
        ok &= line.starts_with("ok");
        println!("  {line}");
    }
    Ok(ok)
}

fn describe_diff(want: &StateObject, got: &StateObject) -> String {
    if want.type_name != got.type_name {
        return format!("type {} vs {}", want.type_name, got.type_name);
    }
    if (want.kind, want.owner) != (got.kind, got.owner) {
        return format!(
            "owner {:?} {} vs {:?} {}",
            want.kind,
            hex(&want.owner),
            got.kind,
            hex(&got.owner)
        );
    }
    let at = want
        .bcs
        .iter()
        .zip(&got.bcs)
        .position(|(a, b)| a != b)
        .unwrap_or(want.bcs.len().min(got.bcs.len()));
    format!(
        "value differs at byte {at} ({} vs {} bytes), {}",
        want.bcs.len(),
        got.bcs.len(),
        short_type(&want.type_name)
    )
}

fn chain_object(v: &Value) -> Result<Option<ChainObject>> {
    // objectChanges' outputState has the same shape as objects fetched directly
    chain::parse_object(v)
}

/// A chain object as a state-snapshot entry.
fn state_object(o: &ChainObject) -> Result<StateObject> {
    let (kind, owner) = match &o.owner {
        Owner::Address(a) => (Kind::Owned, addr(a)?),
        Owner::Object(p) => (Kind::Child, addr(p)?),
        // immutable objects are read-only shared ones as far as the runtime is concerned
        Owner::Shared | Owner::Immutable => (Kind::Shared, [0; 32]),
    };
    Ok(StateObject {
        kind,
        id: addr(&o.id)?,
        owner,
        type_name: canonical_type(&o.type_repr)?,
        bcs: o.bcs.clone(),
    })
}

/// The packages of a transaction.
struct Packages {
    roots: Vec<move2sp1::BytecodePackage>,
    deps: Vec<move2sp1::BytecodePackage>,
    /// storage ID -> original ID (64-hex, no `0x`)
    original_of: BTreeMap<String, String>,
}

impl Packages {
    async fn fetch(
        chain: &mut Chain,
        calls: &[(String, String, String, Vec<String>)],
    ) -> Result<Packages> {
        let mut original_of = BTreeMap::new();
        let mut roots = vec![];
        let mut loaded: BTreeSet<String> = BTreeSet::new(); // originals and defining IDs
        // original -> (storage id, version): the newest any called package links against
        let mut linkage: BTreeMap<String, (String, u64)> = BTreeMap::new();
        let storage: BTreeSet<&String> = calls.iter().map(|c| &c.0).collect();
        for s in storage {
            let p = chain.package(&format!("0x{s}"), None).await?;
            let original = module_address(&p.modules)?;
            original_of.insert(s.clone(), original.clone());
            for l in &p.linkage {
                let e = linkage
                    .entry(norm(&l.original))
                    .or_insert((norm(&l.upgraded), l.version));
                if l.version > e.1 {
                    *e = (norm(&l.upgraded), l.version);
                }
            }
            loaded.insert(original.clone());
            roots.push(bytecode(&p, &original)?);
            loaded.extend(p.type_origins.iter().map(|t| norm(&t.2)));
        }
        let mut deps = vec![];
        for (original, (upgraded, version)) in &linkage {
            if loaded.contains(original) {
                continue;
            }
            // frameworks are upgraded in place: ask for the version
            let v = (original == upgraded).then_some(*version);
            let p = chain.package(&format!("0x{upgraded}"), v).await?;
            loaded.insert(original.clone());
            loaded.extend(p.type_origins.iter().map(|t| norm(&t.2)));
            deps.push(bytecode(&p, original)?);
        }
        // packages that only define type arguments (e.g. coin types)
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        for (.., targs) in calls {
            for t in targs {
                wanted.extend(type_addresses(t));
            }
        }
        // ... and their own dependencies (versions already loaded take precedence)
        let mut queue: Vec<String> = wanted.into_iter().collect();
        while let Some(a) = queue.pop() {
            if loaded.contains(&a) {
                continue;
            }
            let p = chain.package(&format!("0x{a}"), None).await?;
            let original = module_address(&p.modules)?;
            queue.extend(p.linkage.iter().map(|l| norm(&l.upgraded)));
            loaded.insert(a);
            loaded.insert(original.clone());
            loaded.extend(p.type_origins.iter().map(|t| norm(&t.2)));
            deps.push(bytecode(&p, &original)?);
        }
        Ok(Packages {
            roots,
            deps,
            original_of,
        })
    }
}

fn bytecode(p: &chain::Package, original: &str) -> Result<move2sp1::BytecodePackage> {
    let original = AccountAddress::from_hex_literal(&format!("0x{original}"))?;
    Ok(move2sp1::BytecodePackage {
        modules: p.modules.clone(),
        type_origins: p
            .type_origins
            .iter()
            .map(|(module, name, defining)| {
                Ok(move2sp1::TypeOrigin {
                    original,
                    module: module.clone(),
                    name: name.clone(),
                    defining: AccountAddress::from_hex_literal(defining)?,
                })
            })
            .collect::<Result<_>>()?,
    })
}

/// A package's original ID: its modules' self address.
fn module_address(modules: &[Vec<u8>]) -> Result<String> {
    let m = move_binary_format::CompiledModule::deserialize_with_defaults(
        modules.first().context("package without modules")?,
    )?;
    Ok(m.address().to_canonical_string(false))
}

/// The PTB in the runner's JSON format; calls go to original IDs (the image's dispatch keys).
fn ptb_json(
    pt: &Value,
    gas: &Value,
    epoch: &Value,
    calls: &[(String, String, String, Vec<String>)],
    packages: &Packages,
) -> Result<Value> {
    let inputs: Vec<Value> = pt["inputs"]
        .as_array()
        .context("inputs")?
        .iter()
        .map(|i| match i["kind"].as_str() {
            Some("PURE") => {
                Ok(json!({ "pure": format!("base64:{}", i["pure"].as_str().context("pure")?) }))
            }
            _ => Ok(json!({ "object": i["objectId"].as_str().context("objectId")? })),
        })
        .collect::<Result<_>>()?;
    let mut commands = vec![];
    for (c, (pkg, module, function, targs)) in pt["commands"]
        .as_array()
        .context("commands")?
        .iter()
        .zip(calls)
    {
        let args: Vec<Value> = c["moveCall"]["arguments"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| match a["kind"].as_str() {
                Some("INPUT") => Ok(json!({ "Input": a["input"] })),
                Some("RESULT") if a["subresult"].is_null() => Ok(json!({ "Result": a["result"] })),
                Some("RESULT") => Ok(json!({ "NestedResult": [a["result"], a["subresult"]] })),
                Some("GAS_COIN") => Ok(json!("GasCoin")),
                k => bail!("unsupported argument kind {k:?}"),
            })
            .collect::<Result<_>>()?;
        let targs = if targs.is_empty() {
            String::new()
        } else {
            format!("<{}>", targs.join(","))
        };
        commands.push(json!({ "MoveCall": {
            "function": format!("0x{}::{module}::{function}{targs}", packages.original_of[pkg]),
            "args": args,
        }}));
    }
    Ok(json!({
        "gas": {
            "price": num(&gas["price"]).context("gas price")?,
            "rgp": num(&epoch["referenceGasPrice"]).context("reference gas price")?,
            "budget": num(&gas["budget"]).context("gas budget")?,
        },
        "inputs": inputs,
        "commands": commands,
    }))
}

/// Sui's type repr -> the runtime's canonical name (64-hex addresses without `0x`).
fn canonical_type(repr: &str) -> Result<String> {
    movevm::ptb::canonical_function(repr)
}

/// Addresses of the datatypes in a type (64-hex, no `0x`).
fn type_addresses(t: &str) -> Vec<String> {
    t.split(['<', '>', ','])
        .filter_map(|part| part.trim().split_once("::").map(|(a, _)| norm(a)))
        .collect()
}

/// A JSON number, or a number in a string (how GraphQL prints u64s).
fn num(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

fn norm(a: &str) -> String {
    format!("{:0>64}", a.trim_start_matches("0x").to_lowercase())
}

fn addr(a: &str) -> Result<[u8; 32]> {
    let h = norm(a);
    (0..32)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).context("bad address"))
        .collect::<Result<Vec<u8>>>()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad address {a}"))
}

fn short(id: &str) -> String {
    let h = id.trim_start_matches("0x");
    format!("0x{}…{}", &h[..6], &h[h.len() - 4..])
}

/// Type name with addresses shortened.
fn short_type(t: &str) -> String {
    let mut out = String::new();
    for part in t.split_inclusive(['<', '>', ',']) {
        match part.split_once("::") {
            Some((a, rest)) if a.len() == 64 => {
                let a = a.trim_start_matches('0');
                out.push_str(&format!("0x{}::{rest}", &a[..a.len().min(4)]));
            }
            _ => out.push_str(part),
        }
    }
    out
}

fn short_function(spec: &str) -> String {
    short_type(&canonical_type(spec).unwrap_or_else(|_| spec.to_string()))
}

/// `2026-11-04T03:10:38.200Z` -> Unix milliseconds.
fn iso_ms(s: &str) -> Result<u64> {
    let bad = || anyhow::anyhow!("bad timestamp {s}");
    let (date, time) = s.trim_end_matches('Z').split_once('T').ok_or_else(bad)?;
    let d: Vec<i64> = date
        .split('-')
        .map(|x| x.parse())
        .collect::<Result<_, _>>()?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let t: Vec<i64> = hms
        .split(':')
        .map(|x| x.parse())
        .collect::<Result<_, _>>()?;
    let [y, m, day] = d[..] else {
        return Err(bad());
    };
    let [hh, mm, ss] = t[..] else {
        return Err(bad());
    };
    let ms: i64 = format!("{frac:0<3}")[..3].parse()?;
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Ok((((days * 24 + hh) * 60 + mm) * 60 + ss) as u64 * 1000 + ms as u64)
}
