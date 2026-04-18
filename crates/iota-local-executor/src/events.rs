// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Decode the raw BCS-encoded Move events attached to a simulation result
//! into fully-annotated [`MoveValue`]s.
//!
//! `SimulateTransactionResult::events` carries a
//! [`TransactionEvents`](iota_types::effects::TransactionEvents) whose payload
//! is a `Vec<u8>` BCS blob. The struct-tag is available per-event but the
//! field layout isn't — that requires loading the containing Move package
//! and running type-layout resolution. This module does that work and
//! returns one [`DecodedEvent`] per event.
//!
//! Use [`OfflineExecutor::decode_events`] etc. for the common case, or
//! [`decode_events`] directly if you already hold a `LayoutResolver`.

use anyhow::{Result, anyhow};
use iota_types::{
    base_types::{IotaAddress, ObjectID},
    effects::TransactionEvents,
    event::Event,
    layout_resolver::LayoutResolver,
};
use move_core_types::{
    annotated_value::{MoveDatatypeLayout, MoveValue},
    identifier::Identifier,
    language_storage::StructTag,
};

/// One decoded Move event.
#[derive(Debug)]
pub struct DecodedEvent {
    /// Package that emitted the event (via `event::emit`).
    pub package_id: ObjectID,
    /// Module inside that package that emitted the event.
    pub transaction_module: Identifier,
    /// Address of the transaction sender.
    pub sender: IotaAddress,
    /// The event struct's type, e.g. `0x2::coin::CoinEvent`.
    pub type_: StructTag,
    /// Decoded event payload with every field named and typed.
    pub value: MoveValue,
    /// Raw BCS bytes of the event, kept for callers that want to re-encode
    /// or hash.
    pub contents_bcs: Vec<u8>,
}

/// Decode every event in `events` against the supplied [`LayoutResolver`].
///
/// Returns one `Result` per event so that a single malformed event doesn't
/// mask the rest.
pub fn decode_events(
    events: &TransactionEvents,
    resolver: &mut dyn LayoutResolver,
) -> Vec<Result<DecodedEvent>> {
    events
        .data
        .iter()
        .map(|ev| decode_one(ev, resolver))
        .collect()
}

fn decode_one(event: &Event, resolver: &mut dyn LayoutResolver) -> Result<DecodedEvent> {
    let layout = resolver
        .get_annotated_layout(&event.type_)
        .map_err(|e| anyhow!("failed to resolve layout for {}: {e}", event.type_))?;
    let value = match layout {
        MoveDatatypeLayout::Struct(s) => MoveValue::simple_deserialize(
            &event.contents,
            &move_core_types::annotated_value::MoveTypeLayout::Struct(s),
        )
        .map_err(|e| anyhow!("BCS deserialise of {}: {e}", event.type_))?,
        MoveDatatypeLayout::Enum(e_layout) => MoveValue::simple_deserialize(
            &event.contents,
            &move_core_types::annotated_value::MoveTypeLayout::Enum(e_layout),
        )
        .map_err(|e| anyhow!("BCS deserialise of {}: {e}", event.type_))?,
    };
    Ok(DecodedEvent {
        package_id: event.package_id,
        transaction_module: event.transaction_module.clone(),
        sender: event.sender,
        type_: event.type_.clone(),
        value,
        contents_bcs: event.contents.clone(),
    })
}
