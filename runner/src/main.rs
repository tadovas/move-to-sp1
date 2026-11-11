//! movevm: run move2sp1-compiled ELFs in the SP1 zkVM.
//!
//! Arguments are given as JSON on the command line, BCS-encoded per the program's ABI
//! (`abi.rs`) and passed as SP1 stdin; the program is executed with SP1's executor (no
//! proof) and its public values are decoded back into JSON.

mod abi;
mod out;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use serde_json::Value;
use sp1_sdk::{Elf, LightProver, ProverClient, SP1Stdin, prelude::*};

use abi::Abi;

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
        /// `40 2`, `0xCAFE`, `'{"x": 1, "y": 2}'`, `'[1, 2, 3]'`.
        args: Vec<String>,
        /// Print results as single-line JSON.
        #[arg(long)]
        compact: bool,
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
        Cmd::Run { elf, args, compact } => run(elf, args, compact).await?,
        Cmd::Test { dir } => test(dir).await?,
    }
    Ok(())
}

/// How an execution ended.
enum Outcome {
    /// Halted normally; the public values hold the results (and effects).
    Success(Vec<u8>),
    /// Move `abort` with this code.
    Abort(u64),
    /// Move VM error with this status code (e.g. 4017 ARITHMETIC_ERROR).
    VmError(u64),
    /// Any other failure (exit code).
    Failed(u64),
}

/// Failure record committed by the runtime (`runtime/core/src/lib.rs`, `ABORT_MAGIC`).
const ABORT_MAGIC: &[u8; 8] = b"MOVEABRT";

/// Execute an ELF in SP1 (no proof). Returns the outcome and the cycle count.
async fn execute(client: &LightProver, elf: &[u8], stdin: SP1Stdin) -> Result<(Outcome, u64)> {
    let (public_values, report) = client
        .execute(Elf::Dynamic(Arc::from(elf)), stdin)
        .await
        .map_err(|e| anyhow!("execution failed: {e}"))?;
    let pv = public_values.to_vec();
    let outcome = match (report.exit_code, pv.strip_prefix(ABORT_MAGIC.as_slice())) {
        (0, _) => Outcome::Success(pv),
        (_, Some([kind, code @ ..])) if code.len() == 8 => {
            let code = u64::from_le_bytes(code.try_into().unwrap());
            if *kind == 0 {
                Outcome::Abort(code)
            } else {
                Outcome::VmError(code)
            }
        }
        (exit, _) => Outcome::Failed(exit),
    };
    Ok((outcome, report.total_instruction_count()))
}

async fn run(elf: PathBuf, args: Vec<String>, compact: bool) -> Result<()> {
    let bytes = std::fs::read(&elf).with_context(|| format!("reading {}", elf.display()))?;
    let abi = Abi::from_elf(&bytes)?;
    ensure!(
        args.len() == abi.params.len(),
        "{} expects {} argument(s) ({}), got {}",
        abi.entry,
        abi.params.len(),
        abi.params.join(", "),
        args.len()
    );

    let mut stdin = SP1Stdin::new();
    for (ty, a) in abi.params.iter().zip(&args) {
        let v: Value = serde_json::from_str(a).unwrap_or_else(|_| Value::String(a.clone()));
        let mut buf = vec![];
        abi.encode(ty, &v, &mut buf)
            .with_context(|| format!("argument {a} ({ty})"))?;
        stdin.write_vec(buf);
    }

    // Light prover: executes (and verifies) without building proving keys.
    let client = ProverClient::builder().light().build().await;
    let t = std::time::Instant::now();
    let (outcome, cycles) = execute(&client, &bytes, stdin).await?;
    let elapsed = t.elapsed();
    let public_values = match outcome {
        Outcome::Success(pv) => pv,
        Outcome::Abort(code) => bail!("{} aborted with code {code} ({cycles} cycles)", abi.entry),
        Outcome::VmError(status) => bail!(
            "{} failed with VM error {status} ({cycles} cycles)",
            abi.entry
        ),
        Outcome::Failed(exit) => bail!("program exited with code {exit}"),
    };

    let mut out = public_values.as_slice();
    let mut results: Vec<out::Out> = abi
        .returns
        .iter()
        .map(|ty| abi.decode(ty, &mut out))
        .collect::<Result<_>>()?;
    let shown = match results.len() {
        1 => results.pop().unwrap(),
        _ => out::Out::Array(results),
    };
    let text = if !compact && shown.is_compound() {
        shown.pretty()
    } else {
        shown.compact()
    };
    println!("{}({}) = {text}", abi.entry, args.join(", "));
    if !out.is_empty() {
        let effects = abi.decode_effects(&mut out)?;
        if let out::Out::Array(list) = &effects
            && !list.is_empty()
        {
            let text = if compact {
                effects.compact()
            } else {
                effects.pretty()
            };
            println!("effects: {text}");
        }
    }
    println!("cycles: {cycles}  (executed in {elapsed:.2?})");
    Ok(())
}

/// `movevm test <dir>`: run the unit tests compiled by `move2sp1 test` and compare each
/// outcome with its `#[expected_failure]` expectation.
async fn test(dir: PathBuf) -> Result<()> {
    let manifest: Vec<Value> = serde_json::from_slice(&std::fs::read(dir.join("tests.json"))?)?;
    let client = ProverClient::builder().light().build().await;
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
        let bytes = std::fs::read(dir.join(elf))?;
        let (outcome, cycles) = execute(&client, &bytes, SP1Stdin::new()).await?;
        total_cycles += cycles;
        let (ok, got) = match &outcome {
            Outcome::Success(_) => (expected == "success", "success".to_string()),
            // `{"abort": null}` accepts any abort code
            Outcome::Abort(code) => {
                let ok = expected
                    .get("abort")
                    .is_some_and(|want| want.is_null() || want.as_u64() == Some(*code));
                (ok, format!("abort {code}"))
            }
            Outcome::VmError(status) => (
                expected["vm_error"].as_u64() == Some(*status),
                format!("VM error {status}"),
            ),
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
