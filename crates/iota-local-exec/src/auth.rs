// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! `--authenticator` / `--auth-arg` handling: wrap a [`TransactionData`] in
//! a [`SenderSignedData`] carrying a `MoveAuthenticator`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use iota_local_executor::{InMemoryStore, LocalPackage};
use iota_types::{
    base_types::{ObjectID, TypeTag},
    object::Owner,
    transaction::{CallArg, SenderSignedData, SharedObjectRef, TransactionData},
};

use crate::{
    args::{ParsedValue, parse_value},
    call::{
        FunctionInfo, ResolvedCall, find_module_in_package, resolve_call_spec, resolve_function,
    },
};

/// Build a [`SenderSignedData`] that wraps `tx` in a `MoveAuthenticator`.
pub(crate) fn wrap(
    tx: TransactionData,
    auth_spec: &str,
    raw_auth_args: &[String],
    account_object: &Option<String>,
    pkgs: &[LocalPackage],
    aliases: &BTreeMap<String, ObjectID>,
    store: &InMemoryStore,
) -> Result<SenderSignedData> {
    use iota_types::{move_authenticator::MoveAuthenticator, signature::GenericSignature};

    // Resolve the authenticator function's existence (same as --call) — this
    // catches typos before we kick off the VM.
    let call = resolve_call_spec(auth_spec, pkgs, aliases)?;
    let pkg = pkgs
        .iter()
        .find(|p| p.id() == call.package_id)
        .ok_or_else(|| {
            anyhow!(
                "--authenticator references package {} but no such --package was supplied",
                call.package_id
            )
        })?;
    let module = find_module_in_package(pkg, &call.module)?;
    let fn_info = resolve_function(module, &call.function)?;
    ensure_auth_arg_count(&call, &fn_info, raw_auth_args)?;

    // Lower auth args into CallArg values (not PTB inputs — MoveAuthenticator
    // carries them verbatim).
    let mut call_args: Vec<CallArg> = Vec::with_capacity(raw_auth_args.len());
    for raw in raw_auth_args {
        let parsed = parse_value(raw)?;
        call_args.push(parsed_to_call_arg(parsed, store)?);
    }

    // object_to_authenticate is the account object itself (typically shared).
    let self_call_arg = account_object_call_arg(account_object.as_ref(), store)?;

    let type_arguments: Vec<TypeTag> = Vec::new();
    let authenticator = MoveAuthenticator::new_v1(call_args, type_arguments, self_call_arg);
    let generic = GenericSignature::MoveAuthenticator(authenticator);
    Ok(SenderSignedData::new(tx, vec![generic]))
}

/// Assert that the number of `--auth-arg`s matches the authenticator
/// function's parameter count.
fn ensure_auth_arg_count(
    call: &ResolvedCall,
    fn_info: &FunctionInfo,
    raw_args: &[String],
) -> Result<()> {
    if raw_args.len() != fn_info.required_arg_count() {
        bail!(
            "--authenticator {}::{}::{} expects {} arg(s), got {}",
            call.package_id,
            call.module,
            call.function,
            fn_info.required_arg_count(),
            raw_args.len()
        );
    }
    Ok(())
}

/// Wrap a [`ParsedValue`] into a [`CallArg`]. The authenticator path carries
/// [`CallArg`]s verbatim rather than running them through a
/// [`iota_types::programmable_transaction_builder::ProgrammableTransactionBuilder`].
fn parsed_to_call_arg(value: ParsedValue, store: &InMemoryStore) -> Result<CallArg> {
    Ok(match value {
        ParsedValue::Pure(bytes) => CallArg::Pure(bytes),
        ParsedValue::Object(id) => {
            let obj = store
                .get_object(&id)
                .ok_or_else(|| anyhow!("object {id} for --auth-arg not found in store"))?;
            match obj.owner {
                Owner::Shared(initial_shared_version) => CallArg::Shared(SharedObjectRef {
                    object_id: id,
                    initial_shared_version,
                    mutable: false,
                }),
                _ => CallArg::ImmutableOrOwned(obj.compute_object_reference()),
            }
        }
    })
}

/// Turn an `--account-object 0xID` flag into a self-call [`CallArg`] suitable
/// for a `MoveAuthenticator`.
fn account_object_call_arg(
    account_object: Option<&String>,
    store: &InMemoryStore,
) -> Result<CallArg> {
    let account_id_s = account_object.ok_or_else(|| {
        anyhow!(
            "--authenticator requires --account-object 0xID (auto-materialisation is \
             not supported in v1)"
        )
    })?;
    let account_id = ObjectID::from_prefixed_short_hex(account_id_s)
        .with_context(|| format!("parsing --account-object `{account_id_s}`"))?;
    let account_obj = store
        .get_object(&account_id)
        .ok_or_else(|| anyhow!("--account-object {account_id} not found in store"))?;
    Ok(match account_obj.owner {
        Owner::Shared(initial_shared_version) => CallArg::Shared(SharedObjectRef {
            object_id: account_id,
            initial_shared_version,
            mutable: false,
        }),
        _ => CallArg::ImmutableOrOwned(account_obj.compute_object_reference()),
    })
}
