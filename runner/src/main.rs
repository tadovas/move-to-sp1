//! movevm: run move2sp1-compiled ELFs in the SP1 zkVM.
//!
//! Reads the entry signature from the ELF's `.move2sp1.abi` section, encodes CLI arguments
//! into SP1 stdin (one buffer per argument, little-endian), executes the program with
//! the SP1 executor and decodes the public values back into Move values.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use num_bigint::BigUint;
use object::{Object, ObjectSection};
use sp1_sdk::{Elf, ProverClient, SP1Stdin, prelude::*};

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
        /// Entry function arguments, e.g. `40 2` (decimal, `0x..` hex, `true`/`false`).
        args: Vec<String>,
    },
    /// Print the program's entry signature.
    Info { elf: PathBuf },
}

struct Abi {
    entry: String,
    params: Vec<String>,
    returns: Vec<String>,
}

fn read_abi(elf: &[u8]) -> Result<Abi> {
    let f = object::File::parse(elf)?;
    let sec = f
        .section_by_name(".move2sp1.abi")
        .ok_or_else(|| anyhow!("not a move2sp1 program (no .move2sp1.abi section)"))?;
    let text = std::str::from_utf8(sec.data()?)?;
    let mut abi = Abi {
        entry: String::new(),
        params: vec![],
        returns: vec![],
    };
    for line in text.lines() {
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        let list = || v.split_whitespace().map(String::from).collect::<Vec<_>>();
        match k {
            "entry" => abi.entry = v.to_string(),
            "params" => abi.params = list(),
            "returns" => abi.returns = list(),
            _ => {}
        }
    }
    Ok(abi)
}

fn width(ty: &str) -> Result<usize> {
    Ok(match ty {
        "bool" | "u8" => 1,
        "u16" => 2,
        "u32" => 4,
        "u64" => 8,
        "u128" => 16,
        "u256" | "address" => 32,
        other => bail!("unsupported ABI type {other}"),
    })
}

fn encode(ty: &str, s: &str) -> Result<Vec<u8>> {
    let w = width(ty)?;
    if ty == "bool" {
        return match s {
            "true" => Ok(vec![1]),
            "false" => Ok(vec![0]),
            _ => bail!("expected true/false for bool, got {s}"),
        };
    }
    let n = match s.strip_prefix("0x") {
        Some(h) => BigUint::parse_bytes(h.as_bytes(), 16),
        None => BigUint::parse_bytes(s.as_bytes(), 10),
    }
    .ok_or_else(|| anyhow!("invalid {ty} literal: {s}"))?;
    let mut bytes = n.to_bytes_le();
    ensure!(bytes.len() <= w, "{s} does not fit in {ty}");
    bytes.resize(w, 0);
    Ok(bytes)
}

fn decode(ty: &str, bytes: &[u8]) -> Result<String> {
    Ok(match ty {
        "bool" => (bytes[0] != 0).to_string(),
        "address" => format!(
            "0x{}",
            bytes
                .iter()
                .rev()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
        _ => BigUint::from_bytes_le(bytes).to_string(),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Info { elf } => {
            let bytes = std::fs::read(&elf)?;
            let abi = read_abi(&bytes)?;
            println!(
                "{}({}) -> ({})",
                abi.entry,
                abi.params.join(", "),
                abi.returns.join(", ")
            );
        }
        Cmd::Run { elf, args } => {
            let bytes =
                std::fs::read(&elf).with_context(|| format!("reading {}", elf.display()))?;
            let abi = read_abi(&bytes)?;
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
                stdin.write_vec(encode(ty, a)?);
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
            let out = public_values.as_slice();
            let mut off = 0;
            let mut results = vec![];
            for ty in &abi.returns {
                let w = width(ty)?;
                ensure!(off + w <= out.len(), "public values too short");
                results.push(decode(ty, &out[off..off + w])?);
                off += w;
            }
            println!(
                "{}({}) = {}",
                abi.entry,
                args.join(", "),
                match results.len() {
                    1 => results[0].clone(),
                    _ => format!("({})", results.join(", ")),
                }
            );
            println!(
                "cycles: {}  (executed in {:.2?})",
                report.total_instruction_count(),
                elapsed
            );
        }
    }
    Ok(())
}
