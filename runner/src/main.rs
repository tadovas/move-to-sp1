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
    },
    /// Print the program's entry signature.
    Info { elf: PathBuf },
    /// Run the unit tests compiled by `move2sp1 test` (a directory with `tests.json`).
    Test { dir: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
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
        } => run(elf, args, compact, state_in, state_out).await?,
        Cmd::Test { dir } => test(dir).await?,
    }
    Ok(())
}

async fn run(
    elf: PathBuf,
    args: Vec<String>,
    compact: bool,
    state_in: Option<PathBuf>,
    state_out: Option<PathBuf>,
) -> Result<()> {
    let bytes = std::fs::read(&elf).with_context(|| format!("reading {}", elf.display()))?;
    let abi = Abi::from_elf(&bytes)?;
    let state = match &state_in {
        Some(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        None => vec![],
    };
    let in_hash = sha256(&state);
    let inputs = Inputs::from_json(&abi, state, &args)?;

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
