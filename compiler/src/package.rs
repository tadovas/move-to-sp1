//! Move packages: `Move.toml` manifests and their dependency graph.
//!
//! Supported: `[package]` name/edition, `[addresses]` (+ `[dev-addresses]` in test mode),
//! and `[dependencies]` with `local = "..."` paths, resolved recursively. The Sui framework
//! packages (`Sui`, `MoveStdlib`, `SuiSystem`) — local or git — map to the embedded copies.
//! Other git dependencies are not fetched (vendor them and use `local`).
//!
//! Unpublished packages declare `name = "0x0"` (or `"_"`); those get a deterministic
//! placeholder address unless overridden with `-a name=0x...`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use move_compiler::editions::Edition;
use serde::Deserialize;

/// Dependencies provided by the embedded frameworks (see `frontend.rs`).
const FRAMEWORK_PACKAGES: &[&str] = &["Sui", "MoveStdlib", "SuiSystem", "sui", "std"];

/// First placeholder address for unpublished packages (`0x0`).
const PLACEHOLDER_BASE: u64 = 0xa0000;

/// A package to compile: its sources and Move edition.
pub struct SourcePackage {
    pub name: String,
    pub edition: Edition,
    pub sources: Vec<PathBuf>,
}

/// A loaded package graph: the root package first, then its dependencies.
pub struct PackageGraph {
    pub packages: Vec<SourcePackage>,
    /// Named addresses of all packages (placeholders assigned).
    pub addresses: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct Manifest {
    package: PackageSection,
    #[serde(default)]
    addresses: BTreeMap<String, String>,
    #[serde(default, rename = "dev-addresses")]
    dev_addresses: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
struct PackageSection {
    name: String,
    edition: Option<String>,
}

/// Load the package at `root` and its local dependencies. In test mode the root package's
/// `tests/` sources and `[dev-addresses]` are included.
pub fn load(root: &Path, test_mode: bool) -> Result<PackageGraph> {
    let mut packages = vec![];
    let mut addresses: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), true)];
    while let Some((dir, is_root)) = stack.pop() {
        let path = dir.join("Move.toml");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let m: Manifest =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if packages
            .iter()
            .any(|p: &SourcePackage| p.name == m.package.name)
        {
            continue;
        }

        let mut named = m.addresses.clone();
        if is_root && test_mode {
            named.extend(m.dev_addresses.clone());
        }
        for (name, value) in named {
            let value = (value != "0x0" && value != "_").then_some(value);
            match addresses.get(&name) {
                Some(Some(prev)) if value.as_ref().is_some_and(|v| v != prev) => {
                    bail!(
                        "named address `{name}` has conflicting values {prev} and {}",
                        value.unwrap()
                    )
                }
                Some(Some(_)) => {}
                _ => {
                    addresses.insert(name, value);
                }
            }
        }

        for (dep, spec) in &m.dependencies {
            if FRAMEWORK_PACKAGES.contains(&dep.as_str()) {
                continue; // embedded
            }
            match spec.get("local").and_then(|v| v.as_str()) {
                Some(rel) => stack.push((dir.join(rel), false)),
                None => bail!(
                    "{}: dependency `{dep}` is not a local path (vendor it and use `local = \"...\"`)",
                    path.display()
                ),
            }
        }

        let mut sources = vec![];
        collect_move_files(&dir.join("sources"), &mut sources)?;
        if is_root && test_mode && dir.join("tests").is_dir() {
            collect_move_files(&dir.join("tests"), &mut sources)?;
        }
        sources.sort();
        let edition = match &m.package.edition {
            Some(e) => e
                .parse()
                .with_context(|| format!("{}: edition {e}", path.display()))?,
            None => Edition::LEGACY,
        };
        packages.push(SourcePackage {
            name: m.package.name,
            edition,
            sources,
        });
    }

    // Unassigned (`0x0`) addresses get deterministic placeholders, in name order.
    let mut next = PLACEHOLDER_BASE;
    let addresses = addresses
        .into_iter()
        .map(|(name, value)| {
            let value = value.unwrap_or_else(|| {
                next += 1;
                format!("{:#x}", next - 1)
            });
            (name, value)
        })
        .collect();
    Ok(PackageGraph {
        packages,
        addresses,
    })
}

fn collect_move_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            collect_move_files(&p, out)?;
        } else if p.extension().is_some_and(|e| e == "move") {
            out.push(p);
        }
    }
    Ok(())
}
