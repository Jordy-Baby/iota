// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! `--call` / `--authenticator` parsing and function signature lookup.
//!
//! Resolves `PKG::MODULE::FN` strings against the set of loaded
//! [`LocalPackage`]s and validates argument counts against the compiled
//! bytecode.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use iota_local_executor::LocalPackage;
use iota_types::base_types::ObjectID;
use move_binary_format::{CompiledModule, file_format::SignatureToken};

/// Fully resolved target of a `--call` / `--authenticator` flag.
pub(crate) struct ResolvedCall {
    pub(crate) package_id: ObjectID,
    pub(crate) module: String,
    pub(crate) function: String,
}

/// Parse `--call PKG::MODULE::FN` and resolve `PKG` against the loaded
/// packages. `PKG` may be the package name (e.g. `hello`), a hex ID
/// (`0x42`), or elided when exactly one package is loaded.
pub(crate) fn resolve_call_spec(
    spec: &str,
    pkgs: &[LocalPackage],
    aliases: &BTreeMap<String, ObjectID>,
) -> Result<ResolvedCall> {
    let parts: Vec<&str> = spec.split("::").collect();
    match parts.as_slice() {
        [module, function] => {
            if pkgs.len() != 1 {
                bail!(
                    "--call `{spec}` elides package, but {} packages are loaded — \
                     use PKG::MODULE::FN form",
                    pkgs.len()
                );
            }
            Ok(ResolvedCall {
                package_id: pkgs[0].id,
                module: (*module).to_string(),
                function: (*function).to_string(),
            })
        }
        [pkg, module, function] => {
            let id = if pkg.starts_with("0x") {
                ObjectID::from_hex_literal(pkg)
                    .map_err(|e| anyhow!("parsing package ID in --call `{spec}`: {e}"))?
            } else {
                *aliases.get(*pkg).ok_or_else(|| {
                    anyhow!(
                        "--call `{spec}` references package `{pkg}` — \
                         no --package was supplied with that name"
                    )
                })?
            };
            Ok(ResolvedCall {
                package_id: id,
                module: (*module).to_string(),
                function: (*function).to_string(),
            })
        }
        _ => bail!("--call `{spec}` must be in PKG::MODULE::FN or MODULE::FN form"),
    }
}

/// Locate a module by name inside a compiled package.
pub(crate) fn find_module_in_package<'a>(
    pkg: &'a LocalPackage,
    module: &str,
) -> Result<&'a CompiledModule> {
    pkg.compiled
        .package
        .root_compiled_units
        .iter()
        .map(|u| &u.unit.module)
        .find(|m| m.self_id().name().as_str() == module)
        .ok_or_else(|| {
            anyhow!(
                "module `{module}` not found in package {} — available: [{}]",
                pkg.id,
                pkg.compiled
                    .package
                    .root_compiled_units
                    .iter()
                    .map(|u| u.unit.module.self_id().name().as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Resolved function signature — parameter count plus per-parameter
/// object-like classification for diagnostics.
pub(crate) struct FunctionInfo {
    params: Vec<SignatureToken>,
}

impl FunctionInfo {
    /// Number of parameters that must be supplied as CLI args. Move currently
    /// doesn't treat any parameter implicitly, so this is just `params.len()`.
    pub(crate) fn required_arg_count(&self) -> usize {
        self.params.len()
    }

    /// Whether parameter at `idx` is object-shaped (by-value struct, by-ref,
    /// or by-mut-ref). Used only for diagnostics — final object-vs-pure is
    /// decided by the literal grammar in [`crate::args::parse_value`].
    pub(crate) fn is_object_like(&self, idx: usize) -> bool {
        matches!(
            self.params.get(idx),
            Some(
                SignatureToken::Datatype(_)
                    | SignatureToken::DatatypeInstantiation(_)
                    | SignatureToken::Reference(_)
                    | SignatureToken::MutableReference(_)
            )
        )
    }
}

/// Find a function definition in a module and return its parameter signature.
pub(crate) fn resolve_function(module: &CompiledModule, function: &str) -> Result<FunctionInfo> {
    let (_idx, def) = module.find_function_def_by_name(function).ok_or_else(|| {
        anyhow!(
            "function `{function}` not found in module {} — available: [{}]",
            module.self_id(),
            available_function_names(module).join(", ")
        )
    })?;
    let handle = module.function_handle_at(def.function);
    let params = module.signature_at(handle.parameters).0.clone();
    Ok(FunctionInfo { params })
}

fn available_function_names(module: &CompiledModule) -> Vec<String> {
    module
        .function_defs()
        .iter()
        .map(|def| {
            module
                .identifier_at(module.function_handle_at(def.function).name)
                .as_str()
                .to_string()
        })
        .collect()
}
