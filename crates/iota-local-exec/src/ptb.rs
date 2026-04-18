// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Build the single-`MoveCall` `ProgrammableTransaction` + [`TransactionData`]
//! from pre-parsed arguments and a resolved call target.

use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use iota_local_executor::InMemoryStore;
use iota_types::{
    base_types::{IotaAddress, ObjectID, ObjectRef},
    object::Owner,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{Argument, ObjectArg, TransactionData},
};
use move_core_types::{identifier::Identifier, language_storage::TypeTag};

use crate::{
    args::{ParsedValue, parse_value},
    call::{FunctionInfo, ResolvedCall},
};

/// Build a single-`MoveCall` [`TransactionData`] from the resolved call
/// target, raw CLI argument strings, type arguments, sender, and gas config.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    call: &ResolvedCall,
    fn_info: &FunctionInfo,
    raw_args: &[String],
    raw_type_args: &[String],
    store: &InMemoryStore,
    sender: IotaAddress,
    gas_coin: Option<&str>,
    gas_budget: u64,
    gas_price: u64,
) -> Result<TransactionData> {
    if raw_args.len() != fn_info.required_arg_count() {
        bail!(
            "function `{}::{}::{}` expects {} arg(s), got {}",
            call.package_id,
            call.module,
            call.function,
            fn_info.required_arg_count(),
            raw_args.len()
        );
    }

    let mut builder = ProgrammableTransactionBuilder::new();
    let type_args = parse_type_args(raw_type_args)?;
    let mut lowered_args: Vec<Argument> = Vec::with_capacity(raw_args.len());
    for (idx, raw) in raw_args.iter().enumerate() {
        let parsed = parse_value(raw)?;
        let arg = lower_into_builder(&mut builder, store, parsed, idx, fn_info)?;
        lowered_args.push(arg);
    }

    builder.programmable_move_call(
        call.package_id,
        Identifier::new(call.module.clone())?,
        Identifier::new(call.function.clone())?,
        type_args,
        lowered_args,
    );
    let pt = builder.finish();

    let gas_coins = resolve_gas_coins(gas_coin, store)?;
    Ok(TransactionData::new_programmable(
        sender, gas_coins, pt, gas_budget, gas_price,
    ))
}

fn parse_type_args(specs: &[String]) -> Result<Vec<TypeTag>> {
    specs
        .iter()
        .map(|s| TypeTag::from_str(s).map_err(|e| anyhow!("parsing --type-arg `{s}`: {e}")))
        .collect()
}

fn resolve_gas_coins(gas_coin: Option<&str>, store: &InMemoryStore) -> Result<Vec<ObjectRef>> {
    match gas_coin {
        Some(s) => {
            let id = ObjectID::from_hex_literal(s)
                .with_context(|| format!("parsing --gas-coin `{s}`"))?;
            let obj = store
                .get_object(&id)
                .ok_or_else(|| anyhow!("--gas-coin {id} not found in store"))?;
            Ok(vec![obj.compute_object_reference()])
        }
        None => Ok(Vec::new()),
    }
}

/// Lower a [`ParsedValue`] into a [`ProgrammableTransactionBuilder`] argument,
/// consulting the store to classify object args as shared vs owned.
fn lower_into_builder(
    builder: &mut ProgrammableTransactionBuilder,
    store: &InMemoryStore,
    value: ParsedValue,
    idx: usize,
    fn_info: &FunctionInfo,
) -> Result<Argument> {
    match value {
        ParsedValue::Pure(bytes) => {
            if fn_info.is_object_like(idx) {
                bail!(
                    "--arg #{idx} is a pure literal but parameter {idx} is an object-typed parameter — \
                     use object(0xID)"
                );
            }
            Ok(builder.pure_bytes(bytes, false))
        }
        ParsedValue::Object(id) => {
            let obj = store.get_object(&id).ok_or_else(|| {
                anyhow!("object {id} not found in store (pass --package or --remote-object)")
            })?;
            let obj_arg = match obj.owner {
                Owner::Shared {
                    initial_shared_version,
                } => ObjectArg::SharedObject {
                    id,
                    initial_shared_version,
                    // Default to immutable reference; explicit mutable is a v2
                    // feature since the current grammar has no marker for it.
                    mutable: false,
                },
                _ => ObjectArg::ImmOrOwnedObject(obj.compute_object_reference()),
            };
            builder.obj(obj_arg)
        }
    }
}
