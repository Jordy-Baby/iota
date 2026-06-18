// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: build a staking transaction with the transaction builder and
//! dry-run it locally against objects pre-fetched over GraphQL.
//!
//! The transaction is never signed: the builder resolves it, its input objects
//! (plus the system-state dynamic fields staking reads) are pulled into a
//! [`GraphqlStore`], and the run is a local [`ExecutionMode::DryRun`] — nothing
//! is submitted to the network.
//!
//! Usage:
//!   cargo run -p iota-vm-sdk --features graphql --example stake_graphql -- \
//!     <graphql-url> <sender-address> <validator-address> [amount]
//!
//! `<sender-address>` must own at least one IOTA coin on the target network.

use anyhow::{Context, Result};
use iota_sdk_graphql_client::Client;
use iota_sdk_transaction_builder::TransactionBuilder;
use iota_sdk_types::Address;
use iota_vm_sdk::{ExecuteOptions, LocalVm, TransactionData, graphql::GraphqlStore};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let graphql_url = args.next().context("missing <graphql-url>")?;
    let sender: Address = args.next().context("missing <sender-address>")?.parse()?;
    let validator: Address = args
        .next()
        .context("missing <validator-address>")?
        .parse()?;
    let amount: u64 = match args.next() {
        Some(a) => a.parse().context("amount must be a u64")?,
        None => 1_000_000_000,
    };

    // Build the staking transaction with the transaction builder over GraphQL.
    // `stake` splits the stake amount off the gas coin, so the sender only needs
    // IOTA coins to pay for gas; `finish` selects them, estimates the gas
    // budget, and returns the transaction unsigned.
    let client = Client::new(&graphql_url).context("connect GraphQL client")?;
    let mut builder = TransactionBuilder::new(sender).with_client(client);
    builder.stake(amount, validator);
    let tx: TransactionData = builder.finish().await.context("resolve staking tx")?;

    // Pull everything the local VM needs over GraphQL: the chain context, the
    // transaction's input objects, and the system-state dynamic fields the
    // staking call reads.
    let mut store = GraphqlStore::connect(graphql_url).context("connect GraphQL store")?;
    let ctx = store
        .fetch_chain_context()
        .await
        .context("fetch chain context")?;
    store
        .prefetch(&tx)
        .await
        .context("prefetch input objects")?;
    store
        .prefetch_dynamic_fields()
        .await
        .context("prefetch dynamic fields")?;

    // Dry-run locally — no signature, no submission.
    let mut vm = LocalVm::new(ctx, store).context("build LocalVm")?;
    let result = vm
        .execute(tx, ExecuteOptions::dry_run())
        .context("local dry-run")?;

    println!("Staking dry-run status: {:?}", result.status);
    println!("Gas summary:            {:?}", result.gas_summary);
    Ok(())
}
