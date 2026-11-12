//! Batch mode: replay many transactions — every user transaction of a range of checkpoints, or
//! the latest ones — each in its own `sui-replay` process, several at a time, and summarize.

use std::{
    collections::BTreeMap,
    process::Command,
    sync::{Arc, Mutex, mpsc},
};

use anyhow::{Context, Result, bail};

use crate::{Cli, chain::Chain};

/// How one replay ended.
struct Verdict {
    digest: String,
    kind: &'static str,
    detail: String,
    cycles: u64,
}

pub async fn run(cli: &Cli) -> Result<()> {
    let mut chain = Chain::new(&cli.url, cli.out.join("cache"))?;
    let digests = if let Some(range) = &cli.checkpoints {
        let (first, last) = match range.split_once('-') {
            Some((a, b)) => (a.parse()?, b.parse()?),
            None => (range.parse()?, range.parse()?),
        };
        let (mut all, mut system) = (vec![], 0);
        for seq in first..=last {
            let (user, sys) = chain.checkpoint_transactions(seq).await?;
            println!(
                "checkpoint {seq}: {} user, {sys} system transaction(s)",
                user.len()
            );
            all.extend(user);
            system += sys;
        }
        println!(
            "replaying {} user transaction(s), {} at a time ({system} system transaction(s) are not replayed)",
            all.len(),
            cli.jobs
        );
        all
    } else if let Some(n) = cli.latest {
        let all = chain.latest_transactions(n).await?;
        println!(
            "replaying the {} latest user transaction(s), {} at a time",
            all.len(),
            cli.jobs
        );
        all
    } else {
        bail!("give a transaction digest, --checkpoints A-B, or --latest N");
    };

    let started = std::time::Instant::now();
    let queue = Arc::new(Mutex::new(digests.clone()));
    let (tx, rx) = mpsc::channel::<Verdict>();
    let exe = std::env::current_exe()?;
    for _ in 0..cli.jobs.max(1) {
        let (queue, tx, exe) = (queue.clone(), tx.clone(), exe.clone());
        let (url, out) = (cli.url.clone(), cli.out.clone());
        std::thread::spawn(move || {
            loop {
                let Some(d) = queue.lock().unwrap().pop() else {
                    return;
                };
                let verdict = replay_one(&exe, &url, &out, &d);
                if tx.send(verdict).is_err() {
                    return;
                }
            }
        });
    }
    drop(tx);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut cycles = 0u64;
    for v in rx {
        *counts.entry(v.kind).or_default() += 1;
        cycles += v.cycles;
        if v.kind == "MATCH" {
            println!("MATCH        {}  {} cycles", v.digest, v.cycles);
        } else {
            println!("{:12} {}  {}", v.kind, v.digest, v.detail);
        }
    }
    let matched = counts.get("MATCH").copied().unwrap_or(0);
    println!(
        "\n{matched} of {} byte-identical to mainnet{}; {:.1?}",
        digests.len(),
        counts
            .iter()
            .filter(|(k, _)| **k != "MATCH")
            .map(|(k, v)| format!(", {v} {k}"))
            .collect::<String>(),
        started.elapsed()
    );
    if matched > 0 {
        println!(
            "cycles: {cycles} in total, {} per transaction on average",
            cycles / matched as u64
        );
    }
    Ok(())
}

/// Run `sui-replay <digest>` and classify how it ended.
fn replay_one(exe: &std::path::Path, url: &str, out: &std::path::Path, digest: &str) -> Verdict {
    let run = Command::new(exe)
        .arg(digest)
        .args(["--url", url])
        .arg("--out")
        .arg(out)
        .output()
        .context("running sui-replay");
    let (ok, text) = match run {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
        ),
        Err(e) => (false, format!("Error: {e:#}")),
    };
    let cycles = text
        .lines()
        .find_map(|l| l.strip_prefix("executed in SP1: "))
        .and_then(|l| l.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let error = text
        .lines()
        .find_map(|l| l.strip_prefix("Error: "))
        .unwrap_or("")
        .to_string();
    let kind = if ok && text.lines().any(|l| l.starts_with("MATCH:")) {
        "MATCH"
    } else if text.contains("DIFF") {
        "DIFF"
    } else if error.contains("not supported") || error.contains("not available") {
        "UNSUPPORTED"
    } else {
        "ERROR"
    };
    Verdict {
        digest: digest.to_string(),
        kind,
        detail: error.chars().take(150).collect(),
        cycles: if kind == "MATCH" { cycles } else { 0 },
    }
}
