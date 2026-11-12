//! movevm: execute move2sp1-compiled programs in the SP1 zkVM and decode their outputs.
//!
//! Library API: an [`Executor`] (the SP1 client, built once) runs a program's ELF on
//! [`Inputs`] (input state + BCS-encoded arguments) and returns an [`Execution`]: how it
//! ended, its decoded results / effects / output state, and its cycle accounting.
//! The `movevm` binary is a thin CLI over these.

pub mod abi;
pub mod out;
pub mod ptb;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use sha2::Digest;
use sp1_sdk::{Elf, LightProver, ProverClient, SP1Stdin, prelude::*};

pub use abi::Abi;
pub use out::Out;

/// Failure record committed by the runtime (`runtime/core/src/lib.rs`, `ABORT_MAGIC`).
const ABORT_MAGIC: &[u8; 8] = b"MOVEABRT";

/// Measured cost per call of each fidelity point's fast (`--fast`) variant, used to project
/// what an exact build would cost with `--fast`. Calibrated on the DeepBook v3 demo
/// (`examples/deepbook-v3-demo`, 165 ID derivations); real costs vary a little with input size
/// (projection vs measurement there: 552,733 vs 557,181 cycles, -0.8%).
const FAST_COST_PER_CALL: &[(&str, u64)] = &[("ids", 1_627)];

/// What a program is run on: the input state snapshot and one BCS buffer per argument
/// (see `compiler/src/translate/entry.rs` for the protocol).
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub state: Vec<u8>,
    pub args: Vec<Vec<u8>>,
}

impl Inputs {
    /// Encode command-line style arguments (JSON, bare words as strings) for `abi`'s entry.
    /// Object references (`&T`) take an object ID, or `_` for the one object of that type in
    /// `state`; `&Clock` takes no input (its argument is ignored).
    pub fn from_json(abi: &Abi, state: Vec<u8>, args: &[String]) -> Result<Inputs> {
        ensure!(
            args.len() == abi.params.len(),
            "{} expects {} argument(s) ({}), got {}",
            abi.entry,
            abi.params.len(),
            abi.params.join(", "),
            args.len()
        );
        let state_objects = abi.state_objects(&state)?;
        let mut encoded = vec![];
        for (ty, a) in abi.params.iter().zip(args) {
            let (ty, a) = match ty.strip_prefix('&') {
                Some("clock::Clock") => continue,
                Some(inner) if inner == "tx_context::TxContext" => (inner, a.clone()),
                Some(inner) if a == "_" => {
                    let found: Vec<_> = state_objects.iter().filter(|(_, t)| t == inner).collect();
                    match found.as_slice() {
                        [(id, _)] => ("address", id.clone()),
                        [] => bail!("no object of type {inner} in the input state"),
                        _ => {
                            bail!("several objects of type {inner} in the input state: give its ID")
                        }
                    }
                }
                Some(_) => ("address", a.clone()),
                None => (ty.as_str(), a.clone()),
            };
            let v: Value = serde_json::from_str(&a).unwrap_or_else(|_| Value::String(a.clone()));
            let mut buf = vec![];
            abi.encode(ty, &v, &mut buf)
                .with_context(|| format!("argument {a} ({ty})"))?;
            encoded.push(buf);
        }
        Ok(Inputs {
            state,
            args: encoded,
        })
    }

    /// For a program image: the transaction context (JSON) and the PTB (JSON, see `ptb`).
    pub fn for_image(abi: &Abi, state: Vec<u8>, tx_context: &str, ptb: &Value) -> Result<Inputs> {
        let mut inputs = Inputs::from_json(abi, state, &[tx_context.to_string()])?;
        inputs.args.push(ptb::encode(abi, ptb)?);
        Ok(inputs)
    }
}

/// How an execution ended.
pub enum Outcome {
    /// Halted normally.
    Success(Output),
    /// Move `abort` with this code, in this module (canonical id).
    Abort { code: u64, location: String },
    /// Move VM error with this status code (e.g. 4017 ARITHMETIC_ERROR), in this module.
    VmError { status: u64, location: String },
    /// Any other failure (exit code).
    Failed(u64),
}

/// The decoded public values of a successful run.
pub struct Output {
    /// One value per entry return value.
    pub results: Vec<Out>,
    /// Events and object transfers / shares / freezes.
    pub effects: Out,
    /// SHA-256 of the input state, as committed by the program.
    pub state_in_hash: [u8; 32],
    /// The output state snapshot (input of the next run).
    pub state_out: Vec<u8>,
}

/// Cycles and invocation count of one fidelity point in an execution.
pub struct FidelityCost {
    pub point: String,
    pub cycles: u64,
    pub calls: u64,
}

pub struct Execution {
    pub outcome: Outcome,
    pub cycles: u64,
    pub fidelity: Vec<FidelityCost>,
}

impl Execution {
    /// For exact builds: the cycles projected with `--fast` (each exact fidelity point's
    /// cycles replaced by its calibrated fast cost). An estimate, not a measurement.
    pub fn projected_fast_cycles(&self, abi: &Abi) -> Option<u64> {
        let exact: Vec<&FidelityCost> = self
            .fidelity
            .iter()
            .filter(|f| !abi.fast.contains(&f.point))
            .collect();
        if exact.is_empty() {
            return None;
        }
        let mut projected = self.cycles;
        for f in exact {
            if let Some((_, per_call)) = FAST_COST_PER_CALL.iter().find(|(p, _)| *p == f.point) {
                projected = projected - f.cycles + f.calls * per_call;
            }
        }
        Some(projected)
    }
}

/// Runs programs in SP1's executor (no proof).
pub struct Executor {
    client: LightProver,
}

impl Executor {
    /// Light prover: executes (and verifies) without building proving keys.
    pub async fn new() -> Self {
        Executor {
            client: ProverClient::builder().light().build().await,
        }
    }

    pub async fn execute(&self, elf: &[u8], inputs: &Inputs) -> Result<Execution> {
        let abi = Abi::from_elf(elf)?;
        let mut stdin = SP1Stdin::new();
        stdin.write_vec(inputs.state.clone());
        for a in &inputs.args {
            stdin.write_vec(a.clone());
        }
        let (public_values, report) = self
            .client
            .execute(Elf::Dynamic(Arc::from(elf)), stdin)
            .await
            .map_err(|e| anyhow!("execution failed: {e}"))?;
        let pv = public_values.to_vec();
        let outcome = match (
            report.exit_code,
            pv.strip_prefix(ABORT_MAGIC.as_slice())
                .and_then(failure_record),
        ) {
            (0, _) => Outcome::Success(decode_output(&abi, &pv)?),
            (_, Some((0, code, location))) => Outcome::Abort { code, location },
            (_, Some((_, status, location))) => Outcome::VmError { status, location },
            (exit, None) => Outcome::Failed(exit),
        };
        let mut fidelity: Vec<FidelityCost> = report
            .cycle_tracker
            .iter()
            .filter_map(|(name, cycles)| {
                let point = name.strip_prefix("fidelity:")?;
                let calls = report.invocation_tracker.get(name).copied().unwrap_or(0);
                Some(FidelityCost {
                    point: point.to_string(),
                    cycles: *cycles,
                    calls,
                })
            })
            .collect();
        fidelity.sort_by(|a, b| a.point.cmp(&b.point));
        Ok(Execution {
            outcome,
            cycles: report.total_instruction_count(),
            fidelity,
        })
    }
}

/// `kind (u8) || code (u64 LE) || len (u64 LE) || location`
fn failure_record(r: &[u8]) -> Option<(u8, u64, String)> {
    let (&kind, r) = r.split_first()?;
    let code = u64::from_le_bytes(r.get(..8)?.try_into().ok()?);
    let len = u64::from_le_bytes(r.get(8..16)?.try_into().ok()?) as usize;
    Some((
        kind,
        code,
        String::from_utf8_lossy(r.get(16..16 + len)?).into_owned(),
    ))
}

/// Public values of a successful run: results, effects, then the state section.
fn decode_output(abi: &Abi, pv: &[u8]) -> Result<Output> {
    let mut out = pv;
    let results = abi
        .returns
        .iter()
        .map(|ty| abi.decode(ty, &mut out))
        .collect::<Result<_>>()?;
    let effects = abi.decode_effects(&mut out)?;
    let state_in_hash: [u8; 32] = out.get(..32).context("missing state section")?.try_into()?;
    out = &out[32..];
    let state_out = abi::read_bytes(&mut out)?.to_vec();
    Ok(Output {
        results,
        effects,
        state_in_hash,
        state_out,
    })
}

/// SHA-256 (state snapshot hashes).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}
