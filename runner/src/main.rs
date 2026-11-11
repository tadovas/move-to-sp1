//! movevm: run move2sp1-compiled ELFs in the SP1 zkVM.
//!
//! Arguments are given as JSON on the command line, BCS-encoded per the program's ABI
//! (`abi.rs`) and passed as SP1 stdin; the program is executed with SP1's executor (no
//! proof) and its public values are decoded back into JSON.

mod abi;
mod out;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use serde_json::Value;
use sp1_sdk::{Elf, ProverClient, SP1Stdin, prelude::*};

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
        }
        Cmd::Run { elf, args, compact } => run(elf, args, compact).await?,
    }
    Ok(())
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
    let (public_values, report) = client
        .execute(Elf::Dynamic(Arc::from(bytes.as_slice())), stdin)
        .await
        .map_err(|e| anyhow!("execution failed: {e}"))?;
    let elapsed = t.elapsed();
    ensure!(
        report.exit_code == 0,
        "program exited with code {}",
        report.exit_code
    );

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
    println!(
        "cycles: {}  (executed in {elapsed:.2?})",
        report.total_instruction_count()
    );
    Ok(())
}
