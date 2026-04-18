// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! `--package` / `--named-address` handling: compile local Move packages and
//! assemble the [`InMemoryStore`] used by the executor.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use iota_framework::BuiltInFramework;
use iota_local_executor::{InMemoryStore, LocalPackage};
use iota_protocol_config::ProtocolConfig;
use iota_types::base_types::ObjectID;

use crate::CompileFail;

const DEFAULT_PACKAGE_START_ID: u64 = 0x42;

/// Outcome of compiling every `--package` flag: the compiled packages
/// themselves, the seeded in-memory store, and the alias table used to resolve
/// `--call PKG::M::F` where `PKG` is a name rather than a hex ID.
pub(crate) struct LoadedPackages {
    pub(crate) packages: Vec<LocalPackage>,
    pub(crate) store: InMemoryStore,
    pub(crate) aliases: BTreeMap<String, ObjectID>,
}

/// Compile all `--package` flags and install them into a fresh in-memory
/// store seeded with the built-in framework genesis objects.
pub(crate) fn load(
    raw_packages: &[String],
    raw_named_addresses: &[String],
    protocol_config: &ProtocolConfig,
) -> Result<LoadedPackages> {
    let named_address_overrides = parse_named_address_overrides(raw_named_addresses)?;

    let mut packages: Vec<LocalPackage> = Vec::new();
    let mut aliases: BTreeMap<String, ObjectID> = BTreeMap::new();
    let mut next_auto_id = DEFAULT_PACKAGE_START_ID;
    for spec in raw_packages {
        let (path, explicit_id) = parse_package_spec(spec)?;
        let id = match explicit_id {
            Some(id) => id,
            None => {
                let id = ObjectID::from_hex_literal(&format!("0x{next_auto_id:x}"))?;
                next_auto_id += 1;
                id
            }
        };
        let named_address = infer_named_address(&path, &named_address_overrides)?;

        let pkg = LocalPackage::compile(&path, id, &named_address, protocol_config)
            .map_err(|e| anyhow::Error::new(CompileFail(e)))?;

        // Register the package name (module-0 namespace) as an alias for the
        // elided --call form.
        if let Some(first) = pkg.compiled.package.root_compiled_units.first() {
            let mod_name = first.unit.module.self_id().name().as_str().to_string();
            aliases.entry(mod_name).or_insert(id);
        }
        aliases.entry(named_address.clone()).or_insert(id);

        packages.push(pkg);
    }

    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    for pkg in &packages {
        pkg.install_into(&mut store);
    }

    Ok(LoadedPackages {
        packages,
        store,
        aliases,
    })
}

fn parse_package_spec(spec: &str) -> Result<(PathBuf, Option<ObjectID>)> {
    match spec.split_once('=') {
        Some((path, id)) => {
            let id = ObjectID::from_hex_literal(id.trim())
                .with_context(|| format!("parsing package ID in `{spec}`"))?;
            Ok((PathBuf::from(path.trim()), Some(id)))
        }
        None => Ok((PathBuf::from(spec.trim()), None)),
    }
}

fn parse_named_address_overrides(specs: &[String]) -> Result<BTreeMap<String, ObjectID>> {
    let mut out = BTreeMap::new();
    for spec in specs {
        let (key, id) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("--named-address `{spec}` is not in KEY=ID form"))?;
        let id = ObjectID::from_hex_literal(id.trim())
            .with_context(|| format!("parsing address in --named-address `{spec}`"))?;
        out.insert(key.trim().to_string(), id);
    }
    Ok(out)
}

/// Pick the name to pass to `LocalPackage::compile` as the `named_address`.
///
/// Strategy:
/// 1. If exactly one `--named-address KEY=...` was supplied, use `KEY`.
/// 2. Otherwise parse the Move.toml's first `[addresses]` entry whose value is
///    `"_"` or `"0x0"`, and use that name.
fn infer_named_address(path: &Path, overrides: &BTreeMap<String, ObjectID>) -> Result<String> {
    if overrides.len() == 1 {
        return Ok(overrides.keys().next().unwrap().clone());
    }
    let move_toml = std::fs::read_to_string(path.join("Move.toml"))
        .with_context(|| format!("reading Move.toml at {}", path.display()))?;
    infer_named_address_from_toml(&move_toml).ok_or_else(|| {
        anyhow!(
            "could not infer named-address for package at {} — \
             declare one `[addresses]` entry as `NAME = \"_\"` or pass --named-address KEY=ID",
            path.display()
        )
    })
}

fn infer_named_address_from_toml(toml: &str) -> Option<String> {
    // Tiny hand-parser: find the `[addresses]` section, then walk lines until
    // the next `[` or EOF. Skip reserved names like `iota`, `std`.
    let mut in_section = false;
    for raw in toml.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_section = line == "[addresses]";
            continue;
        }
        if !in_section {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key = key.trim().to_string();
        let value = value.trim().trim_matches('"');
        if matches!(key.as_str(), "iota" | "std" | "move_stdlib") {
            continue;
        }
        if value == "_" || value == "0x0" {
            return Some(key);
        }
    }
    None
}
