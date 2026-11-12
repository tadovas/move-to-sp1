//! `movevm`: CLI over the `movevm` library (see `lib.rs`): run programs, run compiled unit
//! tests, show a program's signature.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use movevm::{Abi, Executor, Inputs, Out, Outcome, sha256};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "movevm", about = "Run move2sp1 programs in the SP1 zkVM")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Execute the program (no proof) and print its results and cycle count.
    Run {
        elf: PathBuf,
        /// Entry function arguments as JSON (bare words are taken as strings), e.g.
        /// `40 2`, `0xCAFE`, `'{"x": 1, "y": 2}'`, `'[1, 2, 3]'`. Object references take an
        /// object ID, or `_` for the one object of that type in the input state.
        args: Vec<String>,
        /// Print results as single-line JSON.
        #[arg(long)]
        compact: bool,
        /// Input state snapshot (objects from earlier runs); default: empty.
        #[arg(long)]
        state_in: Option<PathBuf>,
        /// Write the output state snapshot here (input for the next run).
        #[arg(long)]
        state_out: Option<PathBuf>,
        /// For a program image: the transaction (PTB JSON file); the only argument is then
        /// the transaction context.
        #[arg(long)]
        ptb: Option<PathBuf>,
    },
    /// Prove a run on the local CPU prover and verify the proof; print the timings.
    Prove {
        elf: PathBuf,
        /// As for `run`.
        args: Vec<String>,
        #[arg(long)]
        state_in: Option<PathBuf>,
        #[arg(long)]
        ptb: Option<PathBuf>,
        /// A compressed (constant-size) proof instead of a core proof.
        #[arg(long)]
        compressed: bool,
        /// Write the proof here.
        #[arg(long)]
        save: Option<PathBuf>,
    },
    /// One proof for a batch of runs: each run directory (as `sui-replay` leaves them:
    /// `image.elf`, `ctx.json`, `ptb.json`, `state_in.bin`) is proven with a compressed proof
    /// (cached as `<dir>/proof-compressed.bin`), then an aggregation program verifies them all
    /// inside SP1 and commits to them.
    Aggregate {
        /// Committed with the batch (e.g. `sui:checkpoint:150000003`).
        #[arg(long, default_value = "batch")]
        label: String,
        dirs: Vec<PathBuf>,
    },
    /// Print the program's entry signature.
    Info { elf: PathBuf },
    /// Run the unit tests compiled by `move2sp1 test` (a directory with `tests.json`).
    Test { dir: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var_os("RUST_LOG").is_some() {
        sp1_sdk::utils::setup_logger();
    }
    match Cli::parse().cmd {
        Cmd::Info { elf } => {
            let abi = Abi::from_elf(&std::fs::read(&elf)?)?;
            println!(
                "{}({}) -> ({})",
                abi.entry,
                abi.params.join(", "),
                abi.returns.join(", ")
            );
            if !abi.effects.is_empty() {
                println!("effects may include:\n  {}", abi.effects.join("\n  "));
            }
        }
        Cmd::Run {
            elf,
            args,
            compact,
            state_in,
            state_out,
            ptb,
        } => run(elf, args, compact, state_in, state_out, ptb).await?,
        Cmd::Test { dir } => test(dir).await?,
        Cmd::Aggregate { label, dirs } => aggregate(&label, &dirs).await?,
        Cmd::Prove {
            elf,
            args,
            state_in,
            ptb,
            compressed,
            save,
        } => {
            let bytes =
                std::fs::read(&elf).with_context(|| format!("reading {}", elf.display()))?;
            let abi = Abi::from_elf(&bytes)?;
            let inputs = inputs(&abi, &args, state_in.as_ref(), ptb.as_ref())?;
            let cycles = Executor::new().await.execute(&bytes, &inputs).await?.cycles;
            println!(
                "proving {} ({cycles} cycles, {} proof) ...",
                elf.display(),
                if compressed { "compressed" } else { "core" }
            );
            let r = movevm::prove(&bytes, &inputs, compressed, save.as_deref()).await?;
            println!("verifying key: {}", r.vkey);
            println!(
                "setup {:.2?}, prove {:.2?} ({:.0} kHz), verify {:.2?}; proof {} KiB",
                r.setup,
                r.prove,
                cycles as f64 / r.prove.as_secs_f64() / 1e3,
                r.verify,
                r.proof_bytes / 1024
            );
            println!(
                "public values: {} bytes (sha256 {})",
                r.public_values.len(),
                short_hex(&sha256(&r.public_values))
            );
        }
    }
    Ok(())
}

async fn run(
    elf: PathBuf,
    args: Vec<String>,
    compact: bool,
    state_in: Option<PathBuf>,
    state_out: Option<PathBuf>,
    ptb: Option<PathBuf>,
) -> Result<()> {
    let bytes = std::fs::read(&elf).with_context(|| format!("reading {}", elf.display()))?;
    let abi = Abi::from_elf(&bytes)?;
    let inputs = inputs(&abi, &args, state_in.as_ref(), ptb.as_ref())?;
    let in_hash = sha256(&inputs.state);

    let executor = Executor::new().await;
    let t = std::time::Instant::now();
    let execution = executor.execute(&bytes, &inputs).await?;
    let elapsed = t.elapsed();
    let cycles = execution.cycles;
    let output = match &execution.outcome {
        Outcome::Success(output) => output,
        Outcome::Abort { code, location } => {
            bail!(
                "{} aborted with code {code} in {location} ({cycles} cycles)",
                abi.entry
            )
        }
        Outcome::VmError { status, location } => {
            bail!(
                "{} failed with VM error {status} in {location} ({cycles} cycles)",
                abi.entry
            )
        }
        Outcome::Failed(exit) => bail!("program exited with code {exit}"),
    };

    let show = |v: &Out| {
        if !compact && v.is_compound() {
            v.pretty()
        } else {
            v.compact()
        }
    };
    let shown = match output.results.as_slice() {
        [single] => show(single),
        many => show(&Out::Array(many.iter().map(Out::clone).collect())),
    };
    println!("{}({}) = {shown}", abi.entry, args.join(", "));
    if let Out::Array(list) = &output.effects
        && !list.is_empty()
    {
        println!("effects: {}", show(&output.effects));
    }
    ensure!(
        output.state_in_hash == in_hash,
        "program committed a different input state hash"
    );
    let summary = abi.summarize_state(&output.state_out)?;
    println!(
        "state: in {} -> out {}  {summary}",
        short_hex(&in_hash),
        short_hex(&sha256(&output.state_out))
    );
    if let Some(p) = &state_out {
        std::fs::write(p, &output.state_out)?;
        println!("  output state written to {}", p.display());
    }

    println!("cycles: {cycles}  (executed in {elapsed:.2?})");
    for f in &execution.fidelity {
        let mode = if abi.fast.contains(&f.point) {
            "fast"
        } else {
            "exact"
        };
        println!(
            "  fidelity `{}` ({mode}): {} cycles in {} calls ({:.1}%, {} cycles/call)",
            f.point,
            f.cycles,
            f.calls,
            100.0 * f.cycles as f64 / cycles.max(1) as f64,
            f.cycles / f.calls.max(1),
        );
    }
    if let Some(projected) = execution.projected_fast_cycles(&abi) {
        println!("  projected with --fast: ~{projected} cycles (estimate, not measured)");
    }
    Ok(())
}

/// A run's inputs: the state snapshot and the arguments (or, for an image, the transaction
/// context and the PTB).
fn inputs(
    abi: &Abi,
    args: &[String],
    state_in: Option<&PathBuf>,
    ptb: Option<&PathBuf>,
) -> Result<Inputs> {
    let state = match state_in {
        Some(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        None => vec![],
    };
    match ptb {
        Some(p) => {
            let ptb: Value = serde_json::from_slice(
                &std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
            )?;
            let [ctx] = args else {
                bail!("with --ptb, the only argument is the transaction context")
            };
            Inputs::for_image(abi, state, ctx, &ptb)
        }
        None => Inputs::from_json(abi, state, args),
    }
}

/// `movevm aggregate`: prove each run (compressed, cached), then the batch.
async fn aggregate(label: &str, dirs: &[PathBuf]) -> Result<()> {
    use std::time::Instant;
    let started = Instant::now();
    let mut runs = vec![];
    let mut total_cycles = 0u64;
    for (i, dir) in dirs.iter().enumerate() {
        let elf = std::fs::read(dir.join("image.elf"))
            .with_context(|| format!("reading {}/image.elf", dir.display()))?;
        let abi = Abi::from_elf(&elf)?;
        let ctx = std::fs::read_to_string(dir.join("ctx.json"))?;
        let inputs = inputs(
            &abi,
            &[ctx],
            Some(&dir.join("state_in.bin")),
            Some(&dir.join("ptb.json")),
        )?;
        let cycles = Executor::new().await.execute(&elf, &inputs).await?.cycles;
        total_cycles += cycles;
        let cached = dir.join("proof-compressed.bin");
        let t = Instant::now();
        let (run, how) = match sp1_sdk::SP1ProofWithPublicValues::load(&cached) {
            Ok(proof) => {
                let vk = movevm::verifying_key(&elf).await?;
                (
                    movevm::RunProof { proof, vk },
                    "cached compressed proof, key setup in",
                )
            }
            Err(_) => {
                let r = movevm::prove_compressed(&elf, &inputs).await?;
                r.proof
                    .save(&cached)
                    .map_err(|e| anyhow::anyhow!("saving the proof: {e}"))?;
                (r, "compressed proof in")
            }
        };
        println!(
            "[{}/{}] {}: {cycles} cycles, {how} {:.1?}",
            i + 1,
            dirs.len(),
            dir.file_name().unwrap_or_default().to_string_lossy(),
            t.elapsed()
        );
        runs.push(run);
    }
    println!("aggregating {} proof(s) ...", runs.len());
    let r = movevm::aggregate(label.as_bytes(), &runs).await?;
    println!(
        "batch proof: {} runs, {total_cycles} cycles in total; aggregation proved in {:.1?}, verified in {:.0?}; proof {} KiB",
        runs.len(),
        r.prove,
        r.verify,
        r.proof_bytes / 1024
    );
    println!("aggregator verifying key: {}", r.vkey);
    println!(
        "committed: label {label:?}, {} (verifying key, public-values digest) pairs; sha256 {}",
        runs.len(),
        short_hex(&sha256(&r.public_values))
    );
    println!("total wall time: {:.1?}", started.elapsed());
    Ok(())
}

/// `movevm test <dir>`: run the unit tests compiled by `move2sp1 test` and compare each
/// outcome with its `#[expected_failure]` expectation.
async fn test(dir: PathBuf) -> Result<()> {
    let manifest: Vec<Value> = serde_json::from_slice(&std::fs::read(dir.join("tests.json"))?)?;
    let executor = Executor::new().await;
    let (mut passed, mut failed, mut total_cycles) = (0, 0, 0u64);
    for t in &manifest {
        let name = t["name"].as_str().unwrap_or("?");
        let expected = &t["expected"];
        let Some(elf) = t["elf"].as_str() else {
            println!(
                "FAIL {name}: not compiled ({})",
                t["compile_error"].as_str().unwrap_or("?")
            );
            failed += 1;
            continue;
        };
        let execution = executor
            .execute(&std::fs::read(dir.join(elf))?, &Inputs::default())
            .await?;
        let cycles = execution.cycles;
        total_cycles += cycles;
        let (ok, got) = match &execution.outcome {
            Outcome::Success(_) => (expected == "success", "success".to_string()),
            // `"abort": null` accepts any code, `"location": null` any module
            Outcome::Abort { code, location } => {
                let ok = expected
                    .get("abort")
                    .is_some_and(|want| want.is_null() || want.as_u64() == Some(*code))
                    && location_ok(&expected["location"], location);
                (ok, format!("abort {code} in {location}"))
            }
            Outcome::VmError { status, location } => {
                let ok = expected["vm_error"].as_u64() == Some(*status)
                    && location_ok(&expected["location"], location);
                (ok, format!("VM error {status} in {location}"))
            }
            Outcome::Failed(exit) => (false, format!("exit code {exit}")),
        };
        if ok {
            passed += 1;
            println!("ok   {name}  [{cycles} cycles]");
        } else {
            failed += 1;
            println!("FAIL {name}: expected {expected}, got {got}  [{cycles} cycles]");
        }
    }
    println!("{passed} passed, {failed} failed; {total_cycles} cycles total");
    ensure!(failed == 0, "{failed} test(s) failed");
    Ok(())
}

/// An expected abort location (canonical module id) matches, or none was expected.
fn location_ok(expected: &Value, actual: &str) -> bool {
    expected.as_str().is_none_or(|want| want == actual)
}

fn short_hex(b: &[u8]) -> String {
    b[..6]
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<String>()
        + "…"
}
