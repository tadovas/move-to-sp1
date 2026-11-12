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
    let kind = data["kind"]["kind"].as_str().unwrap_or("?");
    ensure!(
        kind == "PROGRAMMABLE_TRANSACTION",
        "not supported: {kind} (a system transaction; only programmable transactions are replayed)"
    );
    let chain_error = (fx["status"]["success"] != true).then(|| &fx["status"]["error"]);
    let lamport = num(&effects["lamportVersion"]).context("lamportVersion")?;
    let checkpoint = num(&effects["checkpoint"]["sequenceNumber"]).context("checkpoint")?;
    println!(
        "tx {} (checkpoint {}, {}), sender {}",
        cli.digest,
        effects["checkpoint"]["sequenceNumber"],
        effects["timestamp"].as_str().unwrap_or("?"),
        short(tx["sender"]["address"].as_str().unwrap_or("?")),
    );

    // what the transaction does: its commands (Move calls name the functions to compile)
    let commands = pt["commands"].as_array().context("commands")?;
    let mut calls = vec![]; // (storage id, module, function, type args)
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    // gas coins, real or virtual (gas reservations), are `Coin<SUI>`
    let mut extra_types: Vec<String> = vec!["0x2::coin::Coin<0x2::sui::SUI>".into()];
    for c in commands {
        let (k, body) = c
            .as_object()
            .and_then(|o| o.iter().next())
            .context("bad command")?;
        *kinds.entry(k.as_str()).or_default() += 1;
        match k.as_str() {
            "moveCall" => {
                let targs: Vec<String> = body["typeArguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect();
                calls.push((
                    norm(body["package"].as_str().context("package")?),
                    body["module"].as_str().context("module")?.to_string(),
                    body["function"].as_str().context("function")?.to_string(),
                    targs,
                ));
            }
            "makeMoveVector" => extra_types.extend(body["elementType"].as_str().map(String::from)),
            "transferObjects" | "splitCoins" | "mergeCoins" => {}
            "publish" | "upgrade" => bail!("not supported: the transaction contains a {k} command"),
            _ => bail!("not supported: unknown command {k}"),
        }
    }
    for i in pt["inputs"].as_array().context("inputs")? {
        match i["kind"].as_str() {
            Some("PURE" | "SHARED" | "IMMUTABLE_OR_OWNED") => {}
            Some("FUNDS_WITHDRAWAL") => ensure!(
                i["fundsWithdrawal"]["source"] == "SENDER",
                "not supported yet: a FUNDS_WITHDRAWAL input from {}",
                i["fundsWithdrawal"]["source"]
            ),
            k => bail!("not supported yet: input kind {}", k.unwrap_or("?")),
        }
    }
    println!(
        "  commands: {}",
        kinds
            .iter()
            .map(|(k, n)| format!("{n}× {k}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // coin reservations: object references standing for funds from the sender's address balance
    let sender = tx["sender"]["address"].as_str().context("sender")?;
    let mut reservations: BTreeMap<usize, (String, u64)> = BTreeMap::new();
    for (n, i) in pt["inputs"]
        .as_array()
        .context("inputs")?
        .iter()
        .enumerate()
    {
        if i["kind"] != "IMMUTABLE_OR_OWNED" {
            continue;
        }
        if let Some((amount, coin)) =
            coin_reservation(&mut chain, i, sender, &calls, &extra_types).await?
        {
            println!(
                "  input {n}: coin reservation of {amount} {} from the sender's address balance",
                short_type(&canonical_type(&coin)?)
            );
            extra_types.push(format!("0x2::coin::Coin<{coin}>"));
            reservations.insert(n, (coin, amount));
        }
    }

    // input state: what the effects say was read or written, at input versions
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
    for (n, i) in pt["inputs"]
        .as_array()
        .context("inputs")?
        .iter()
        .enumerate()
    {
        if i["kind"] == "IMMUTABLE_OR_OWNED" && !reservations.contains_key(&n) {
            keys.push((
                i["objectId"].as_str().context("objectId")?.into(),
                num(&i["version"]).context("version")?,
            ));
        }
    }
    let gas_coins: Vec<&Value> = data["gasPayment"]["objects"]
        .as_array()
        .map(|v| v.iter().collect())
        .unwrap_or_default();
    for g in &gas_coins {
        if num(&g["version"]) == Some(0) {
            continue; // a reservation of the address balance, not an object
        }
        keys.push((
            g["objectId"].as_str().context("gas objectId")?.into(),
            num(&g["version"]).context("gas version")?,
        ));
    }
    keys.sort();
    keys.dedup();
    let mut objects = vec![];
    // the digests the effects certify for the input objects (written, and shared read-only)
    let mut certified: BTreeMap<String, &str> = BTreeMap::new();
    for c in fx["changedObjects"].as_array().into_iter().flatten() {
        if let (Some(id), Some(d)) = (c["objectId"].as_str(), c["inputDigest"].as_str()) {
            certified.insert(norm(id), d);
        }
    }
    for o in fx["unchangedConsensusObjects"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let (Some(id), Some(d)) = (o["objectId"].as_str(), o["digest"].as_str()) {
            certified.insert(norm(id), d);
        }
    }
    let mut verified = 0;
    for (o, (id, v)) in chain.objects(&keys).await?.into_iter().zip(&keys) {
        let o = o.with_context(|| {
            format!("not available: input object {id} v{v} is not served by the GraphQL endpoint (history not retained)")
        })?;
        if let Some(d) = certified.get(&norm(id)) {
            ensure!(
                o.verify(d),
                "input object {id} v{v} does not match the digest certified in the effects"
            );
            verified += 1;
        }
        extra_types.push(o.type_repr.clone());
        objects.push(state_object(&o)?);
    }
    println!(
        "state: {} object(s) read or written (from effects), {} created; {verified} verified against digests in the effects{}",
        objects.len(),
        absent.len(),
        match objects.len() - verified {
            0 => String::new(),
            n => format!(" ({n} only read: the effects record their version, not a digest)"),
        }
    );

    // packages and the image
    let t = std::time::Instant::now();
    let packages = Packages::fetch(&mut chain, checkpoint, &calls, &extra_types).await?;
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
        move2sp1::Entry::Image {
            functions: &specs,
            types: &extra_types,
        },
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
    let sender = tx["sender"]["address"].as_str().context("sender")?;
    let ptb = ptb_json(
        pt,
        sender,
        &data["gasPayment"],
        epoch,
        &fx["gasUsed"],
        &packages,
        &reservations,
    )?;
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
    println!(
        "executed in SP1: {} cycles ({elapsed:.2?}); input state sha256 {}",
        run.cycles,
        hex(&movevm::sha256(&state_in))
    );
    if let Some(err) = chain_error {
        // on chain, everything but gas was reverted: the failure itself is what to compare
        return compare_failure(err, &run.outcome, &packages);
    }
    let output = match run.outcome {
        Outcome::Success(o) => o,
        Outcome::Abort { code, location } => {
            bail!("DIFF: succeeded on chain, aborted with {code} in {location} in the replay")
        }
        Outcome::VmError { status, location } => {
            bail!("DIFF: succeeded on chain, VM error {status} in {location} in the replay")
        }
        Outcome::Failed(e) => bail!("program exited with {e}"),
    };
    std::fs::write(dir.join("state_out.bin"), &output.state_out)?;

    let ok = compare(&objects, &output, effects)?;
    let ok = compare_balances(fx, &data["gasPayment"], &output)? && ok;
    let gas = &fx["gasUsed"];
    println!(
        "gas (from chain, not computed by the replay): computation {}, storage {}, rebate {}",
        gas["computationCost"], gas["storageCost"], gas["storageRebate"]
    );
    ensure!(ok, "replay differs from chain");
    println!("MATCH: every written object and event is byte-identical to mainnet");
    Ok(())
}

/// A transaction that failed on chain must fail the same way: same abort code in the same
/// module (Move aborts), or a VM error in the same module.
fn compare_failure(err: &Value, outcome: &Outcome, packages: &Packages) -> Result<()> {
    let kind = err["kind"].as_str().unwrap_or("?");
    let chain_module = || -> Option<String> {
        let l = if kind == "MOVE_ABORT" {
            &err["abort"]["location"]
        } else {
            &err["location"]
        };
        let pkg = norm(l["package"].as_str()?);
        let pkg = packages.original_of.get(&pkg).cloned().unwrap_or(pkg);
        Some(format!("{pkg}::{}", l["module"].as_str()?))
    };
    println!(
        "chain: failed in command {} — {}",
        err["command"].as_str().unwrap_or("?"),
        err["description"].as_str().unwrap_or(kind)
    );
    let (ok, got) = match outcome {
        // failures of the PTB interpreter itself (runtime/sui/src/ptb.rs error codes)
        Outcome::Abort { code, location } if location == "move2sp1::ptb" => {
            let ours = match code {
                6 => "INSUFFICIENT_COIN_BALANCE",
                7 => "COIN_BALANCE_OVERFLOW",
                3 | 4 => "COMMAND_ARGUMENT_ERROR",
                10 => "INSUFFICIENT_GAS",
                _ => "?",
            };
            (kind == ours, format!("PTB error {ours} (code {code})"))
        }
        Outcome::Abort { code, location } => (
            kind == "MOVE_ABORT"
                && num(&err["abort"]["abortCode"]) == Some(*code)
                && chain_module().as_deref() == Some(location),
            format!("abort {code} in {}", short_type(location)),
        ),
        Outcome::VmError { status, location } => (
            kind == "MOVE_PRIMITIVE_RUNTIME_ERROR" && chain_module().as_deref() == Some(location),
            format!("VM error {status} in {}", short_type(location)),
        ),
        Outcome::Success(_) => (false, "success".into()),
        Outcome::Failed(e) => (false, format!("exit code {e}")),
    };
    println!("replay: {got}");
    ensure!(ok, "DIFF: the replay does not fail like the chain");
    println!("MATCH: fails exactly as on mainnet (state reverted there; replay output discarded)");
    Ok(())
}

/// Address balances: the replay's net deposits/withdrawals per (owner, balance type) against
/// the chain's accumulator writes. Gas paid from an address balance comes from the chain's gas
/// figures (gas is not metered).
fn compare_balances(fx: &Value, gas: &Value, output: &movevm::Output) -> Result<bool> {
    let mut chain_net: BTreeMap<(String, String), i128> = BTreeMap::new();
    for c in fx["changedObjects"].as_array().into_iter().flatten() {
        let w = &c["accumulatorWrite"];
        if w.is_null() {
            continue;
        }
        let amount = w["integerValue"]
            .as_str()
            .and_then(|v| v.parse::<i128>().ok())
            .context("accumulator amount")?;
        let signed = match w["operation"].as_str() {
            Some("MERGE") => amount,
            Some("SPLIT") => -amount,
            op => bail!("unknown accumulator operation {op:?}"),
        };
        let key = (
            norm(w["address"].as_str().context("address")?),
            canonical_type(w["accumulatorType"].as_str().context("type")?)?,
        );
        *chain_net.entry(key).or_default() += signed;
    }
    let mut ours: BTreeMap<(String, String), i128> = BTreeMap::new();
    for r in output.effect_records.iter().filter(|r| r.tag == 4) {
        let amount = u64::from_le_bytes(r.bcs[1..9].try_into()?) as i128;
        let owner = hex(&r.recipient.context("accumulator owner")?);
        *ours.entry((owner, r.type_name.clone())).or_default() +=
            if r.bcs[0] == 0 { amount } else { -amount };
    }
    // gas from the address balance (no gas coins, or a reservation absorbing them)
    let coins: Vec<&Value> = gas["objects"].as_array().into_iter().flatten().collect();
    let reservation = coins.first().is_some_and(|c| num(&c["version"]) == Some(0));
    if coins.is_empty() || reservation {
        let owner = norm(gas["owner"].as_str().context("gas owner")?);
        let sui = canonical_type("0x2::balance::Balance<0x2::sui::SUI>")?;
        let cost = |k: &str| num(&fx["gasUsed"][k]).map(|v| v as i128).unwrap_or(0);
        // (the replay itself records what the PTB took from / left in the balance)
        let net = cost("storageRebate") - cost("computationCost") - cost("storageCost");
        *ours.entry((owner, sui)).or_default() += net;
    }
    ours.retain(|_, v| *v != 0);
    chain_net.retain(|_, v| *v != 0);
    if chain_net.is_empty() && ours.is_empty() {
        return Ok(true);
    }
    println!("address balances (net change, chain vs replay; gas part from chain):");
    let keys: BTreeSet<&(String, String)> = chain_net.keys().chain(ours.keys()).collect();
    let mut ok = true;
    for k in keys {
        let (c, o) = (chain_net.get(k), ours.get(k));
        let verdict = if c == o { "ok  " } else { "DIFF" };
        ok &= c == o;
        println!(
            "  {verdict} {} {}: {:+} vs {:+}",
            short(&k.0),
            short_type(&k.1),
            c.copied().unwrap_or(0),
            o.copied().unwrap_or(0)
        );
    }
    Ok(ok)
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
            let chain_out = chain_object(out)?.context("output object")?;
            let certified = effects["effectsJson"]["changedObjects"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["objectId"].as_str().map(norm) == Some(hex(&id)))
                .and_then(|c| c["outputDigest"].as_str());
            ensure!(
                certified.is_some_and(|d| chain_out.verify(d)),
                "chain output object 0x{} does not match the digest certified in the effects",
                hex(&id)
            );
            let want = state_object(&chain_out)?;
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
    /// The called packages and their linkage, plus the packages defining `types` (and
    /// the type arguments of the calls).
    async fn fetch(
        chain: &mut Chain,
        checkpoint: u64,
        calls: &[(String, String, String, Vec<String>)],
        types: &[String],
    ) -> Result<Packages> {
        // a package by storage ID; system packages as of the transaction's checkpoint
        async fn get(chain: &mut Chain, a: &str, checkpoint: u64) -> Result<chain::Package> {
            if is_system(a) {
                chain.system_package(&format!("0x{a}"), checkpoint).await
            } else {
                chain.package(&format!("0x{a}"), None).await
            }
        }
        let mut original_of = BTreeMap::new();
        let mut roots = vec![];
        let mut loaded: BTreeSet<String> = BTreeSet::new(); // originals and defining IDs
        // original -> (storage id, version): the newest any called package links against
        let mut linkage: BTreeMap<String, (String, u64)> = BTreeMap::new();
        let storage: BTreeSet<&String> = calls.iter().map(|c| &c.0).collect();
        for s in storage {
            let p = get(chain, s, checkpoint).await?;
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
        // original -> (version, index in deps)
        let mut dep_at: BTreeMap<String, (u64, usize)> = BTreeMap::new();
        for (original, (upgraded, version)) in &linkage {
            if loaded.contains(original) {
                continue;
            }
            let p = if is_system(original) {
                get(chain, original, checkpoint).await?
            } else {
                chain
                    .package(&format!("0x{upgraded}"), Some(*version))
                    .await?
            };
            original_of.insert(upgraded.clone(), original.clone());
            loaded.insert(original.clone());
            loaded.extend(p.type_origins.iter().map(|t| norm(&t.2)));
            dep_at.insert(original.clone(), (p.version, deps.len()));
            deps.push(bytecode(&p, original)?);
        }
        // packages that only define type arguments (e.g. coin types)
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        for (.., targs) in calls {
            for t in targs {
                wanted.extend(type_addresses(t));
            }
        }
        for t in types {
            wanted.extend(type_addresses(t));
        }
        // ... and their own dependencies (versions already loaded take precedence)
        let mut queue: Vec<String> = wanted.into_iter().collect();
        while let Some(a) = queue.pop() {
            if loaded.contains(&a) {
                continue;
            }
            let p = get(chain, &a, checkpoint).await?;
            let original = module_address(&p.modules)?;
            let have = loaded.contains(&original);
            original_of.insert(a.clone(), original.clone());
            loaded.insert(a);
            if have {
                // another version of a package we have: keep the newer (it has every type),
                // unless the one we have is a called package (its version is the one run)
                if let Some((v, i)) = dep_at.get(&original).copied()
                    && p.version > v
                {
                    loaded.extend(p.type_origins.iter().map(|t| norm(&t.2)));
                    deps[i] = bytecode(&p, &original)?;
                    dep_at.insert(original, (p.version, i));
                }
                continue;
            }
            queue.extend(p.linkage.iter().map(|l| norm(&l.upgraded)));
            loaded.insert(original.clone());
            loaded.extend(p.type_origins.iter().map(|t| norm(&t.2)));
            dep_at.insert(original.clone(), (p.version, deps.len()));
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
/// Gas coins are charged the chain's net gas cost (not computed here).
fn ptb_json(
    pt: &Value,
    sender: &str,
    gas: &Value,
    epoch: &Value,
    gas_used: &Value,
    packages: &Packages,
    reservations: &BTreeMap<usize, (String, u64)>,
) -> Result<Value> {
    let inputs: Vec<Value> = pt["inputs"]
        .as_array()
        .context("inputs")?
        .iter()
        .enumerate()
        .map(|(n, i)| match i["kind"].as_str() {
            _ if reservations.contains_key(&n) => {
                let (coin, amount) = &reservations[&n];
                Ok(json!({ "reservation": { "coin": format!("0x2::coin::Coin<{coin}>"), "amount": amount } }))
            }
            Some("PURE") => {
                Ok(json!({ "pure": format!("base64:{}", i["pure"].as_str().context("pure")?) }))
            }
            // `Withdrawal<Balance<T>> { owner: sender, limit: amount as u256 }`
            Some("FUNDS_WITHDRAWAL") => {
                let amount = num(&i["fundsWithdrawal"]["amount"]).context("withdrawal amount")?;
                let mut limit = [0u8; 32];
                limit[..8].copy_from_slice(&amount.to_le_bytes());
                Ok(json!({ "pure": format!("0x{}{}", hex(&addr(sender)?), hex(&limit)) }))
            }
            _ => Ok(json!({ "object": i["objectId"].as_str().context("objectId")? })),
        })
        .collect::<Result<_>>()?;
    let arg = |a: &Value| -> Result<Value> {
        match a["kind"].as_str() {
            Some("INPUT") => Ok(json!({ "Input": a["input"] })),
            Some("RESULT") if a["subresult"].is_null() => Ok(json!({ "Result": a["result"] })),
            Some("RESULT") => Ok(json!({ "NestedResult": [a["result"], a["subresult"]] })),
            Some("GAS" | "GAS_COIN") => Ok(json!("GasCoin")),
            k => bail!("not supported: argument kind {k:?}"),
        }
    };
    let args = |list: &Value| -> Result<Vec<Value>> {
        list.as_array().into_iter().flatten().map(arg).collect()
    };
    let mut commands = vec![];
    for c in pt["commands"].as_array().context("commands")? {
        let (k, b) = c
            .as_object()
            .and_then(|o| o.iter().next())
            .context("bad command")?;
        commands.push(match k.as_str() {
            "moveCall" => {
                let pkg = norm(b["package"].as_str().context("package")?);
                let targs: Vec<&str> = b["typeArguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str())
                    .collect();
                let targs = if targs.is_empty() {
                    String::new()
                } else {
                    format!("<{}>", targs.join(","))
                };
                json!({ "MoveCall": {
                    "function": format!("0x{}::{}::{}{targs}", packages.original_of[&pkg],
                        b["module"].as_str().context("module")?,
                        b["function"].as_str().context("function")?),
                    "args": args(&b["arguments"])?,
                }})
            }
            "transferObjects" => json!({ "TransferObjects": {
                "objects": args(&b["objects"])?, "address": arg(&b["address"])?,
            }}),
            "splitCoins" => json!({ "SplitCoins": {
                "coin": arg(&b["coin"])?, "amounts": args(&b["amounts"])?,
            }}),
            "mergeCoins" => json!({ "MergeCoins": {
                "coin": arg(&b["coin"])?, "sources": args(&b["coinsToMerge"])?,
            }}),
            "makeMoveVector" => json!({ "MakeMoveVec": {
                "type": b["elementType"], "elements": args(&b["elements"])?,
            }}),
            _ => bail!("not supported: command {k}"),
        });
    }
    // a version-0 entry reserves gas from the address balance (zero address in the PTB);
    // no gas objects at all is the same: gas from the address balance, nothing to absorb
    let mut coins: Vec<Value> = gas["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| {
            if num(&o["version"]) == Some(0) {
                json!("0x0")
            } else {
                o["objectId"].clone()
            }
        })
        .collect();
    if coins.is_empty() {
        coins.push(json!("0x0"));
    }
    // net gas cost, as charged to the gas coin by the chain
    let cost = |k: &str| num(&gas_used[k]).with_context(|| format!("gasUsed.{k}"));
    let net =
        (cost("computationCost")? + cost("storageCost")?) as i128 - cost("storageRebate")? as i128;
    Ok(json!({
        "gas": {
            "price": num(&gas["price"]).context("gas price")?,
            "rgp": num(&epoch["referenceGasPrice"]).context("reference gas price")?,
            "budget": num(&gas["budget"]).context("gas budget")?,
            "coins": coins,
            "charge": net.max(0) as u64,
            "refund": (-net).max(0) as u64,
            "owner": gas["owner"],
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

/// System packages (upgraded in place with the protocol).
fn is_system(a: &str) -> bool {
    let a = norm(a);
    let a = a.trim_start_matches('0');
    matches!(a, "1" | "2" | "3" | "b" | "dee9")
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

/// A coin reservation (`IMMUTABLE_OR_OWNED` input whose "digest" is
/// `amount: u64 LE || epoch: u32 LE || 0xac * 20` and whose ID is the sender's balance
/// accumulator ID for `Balance<T>`, XOR the chain identifier): `(amount, T)`. `T` is found
/// among the types the transaction mentions.
async fn coin_reservation(
    chain: &mut Chain,
    input: &Value,
    sender: &str,
    calls: &[(String, String, String, Vec<String>)],
    types: &[String],
) -> Result<Option<(u64, String)>> {
    let digest = bs58::decode(input["digest"].as_str().unwrap_or_default()).into_vec()?;
    if digest.len() != 32 || digest[12..].iter().any(|&b| b != 0xac) {
        return Ok(None);
    }
    let amount = u64::from_le_bytes(digest[..8].try_into()?);
    let chain_id = chain.chain_identifier().await?;
    let masked = addr(input["objectId"].as_str().context("objectId")?)?;
    let unmasked: Vec<u8> = masked.iter().zip(chain_id).map(|(a, b)| a ^ b).collect();
    let mut candidates: BTreeSet<String> = BTreeSet::from(["0x2::sui::SUI".to_string()]);
    for t in calls.iter().flat_map(|c| &c.3).chain(types) {
        candidates.insert(t.clone());
        // non-generic types inside it
        for part in t.split(['<', '>', ',']) {
            if part.contains("::") {
                candidates.insert(part.trim().to_string());
            }
        }
    }
    for t in candidates {
        let Ok(tag) = type_tag_bcs(&t) else { continue };
        // accumulator_address<Balance<T>>(sender) = dynamic field ID of Key<Balance<T>> under 0xacc
        let mut key_tag = vec![7];
        key_tag.extend(addr("0x2")?);
        key_tag.extend(bcs_str("accumulator"));
        key_tag.extend(bcs_str("Key"));
        key_tag.push(1);
        key_tag.push(7);
        key_tag.extend(addr("0x2")?);
        key_tag.extend(bcs_str("balance"));
        key_tag.extend(bcs_str("Balance"));
        key_tag.push(1);
        key_tag.extend(&tag);
        use blake2::{Blake2b, Digest, digest::consts::U32};
        let mut h = Blake2b::<U32>::new();
        h.update([0xf0]);
        h.update(addr("0xacc")?);
        h.update(32u64.to_le_bytes());
        h.update(addr(sender)?);
        h.update(&key_tag);
        if h.finalize()[..] == unmasked[..] {
            return Ok(Some((amount, t)));
        }
    }
    bail!(
        "not supported yet: a coin reservation input of a coin type not among the transaction's types"
    )
}

fn bcs_str(s: &str) -> Vec<u8> {
    let mut v = vec![s.len() as u8];
    v.extend(s.as_bytes());
    v
}

/// BCS of a Move `TypeTag`, from Sui's type syntax.
fn type_tag_bcs(t: &str) -> Result<Vec<u8>> {
    let t = t.trim();
    let prim = |b: u8| Ok(vec![b]);
    match t {
        "bool" => return prim(0),
        "u8" => return prim(1),
        "u64" => return prim(2),
        "u128" => return prim(3),
        "address" => return prim(4),
        "signer" => return prim(5),
        "u16" => return prim(8),
        "u32" => return prim(9),
        "u256" => return prim(10),
        _ => {}
    }
    let (head, args) = match t.find('<') {
        Some(i) => (
            &t[..i],
            move2sp1::translate::split_type_args(t[i + 1..].strip_suffix('>').context("bad type")?),
        ),
        None => (t, vec![]),
    };
    if head == "vector" {
        let mut v = vec![6];
        v.extend(type_tag_bcs(args.first().context("vector type")?)?);
        return Ok(v);
    }
    let parts: Vec<&str> = head.split("::").collect();
    let [a, m, n] = parts.as_slice() else {
        bail!("bad type {t}")
    };
    let mut v = vec![7];
    v.extend(addr(a)?);
    v.extend(bcs_str(m));
    v.extend(bcs_str(n));
    v.push(args.len() as u8);
    for a in &args {
        v.extend(type_tag_bcs(a)?);
    }
    Ok(v)
}
