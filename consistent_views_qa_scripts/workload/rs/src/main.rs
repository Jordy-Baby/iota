// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Generate a coverage workload on a running localnet using the in-process
//! iota-sdk. Runs all 8 scenarios concurrently, each on its own dedicated gas
//! coin so they don't contend.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use iota_config::{IOTA_KEYSTORE_FILENAME, iota_config_dir};
use iota_keys::keystore::{AccountKeystore, FileBasedKeystore};
use iota_move_build::BuildConfig;
use iota_sdk::{
    IotaClient, IotaClientBuilder,
    json::IotaJsonValue,
    rpc_types::{
        IotaTransactionBlockEffectsAPI, IotaTransactionBlockResponse,
        IotaTransactionBlockResponseOptions, ObjectChange,
    },
    types::{
        base_types::{IotaAddress, ObjectID},
        quorum_driver_types::ExecuteTransactionRequestType,
        transaction::{Transaction, TransactionData},
    },
};
use iota_sdk_types::crypto::Intent;
use move_compiler::editions::Flavor;
use move_package::BuildConfig as MoveBuildConfig;
use serde::Serialize;
use serde_json::json;

const GAS_BUDGET: u64 = 100_000_000;
const NUM_PARALLEL_COINS: usize = 8;

#[derive(Parser, Debug)]
#[command(about = "Generate a coverage workload for consistent-views QA tests")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9000")]
    rpc_url: String,
    #[arg(long, default_value = "http://localhost:9123/gas")]
    faucet_url: String,
    /// Path to the Move package directory.
    #[arg(long)]
    package_path: PathBuf,
    /// Reuse an already-published package; skip publish step.
    #[arg(long)]
    skip_publish: Option<ObjectID>,
    /// Write the manifest JSON here as well as stdout.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Override the sender address. Default: first address in the keystore.
    #[arg(long)]
    sender: Option<IotaAddress>,
}

#[derive(Serialize)]
struct Manifest {
    package_id: ObjectID,
    sender: IotaAddress,
    final_checkpoint: u64,
    scenarios: Scenarios,
}

#[derive(Serialize, Default)]
struct Scenarios {
    parents_with_dfs: Vec<ObjectID>,
    parents_with_dofs: Vec<ObjectID>,
    df_lifecycle_parents: Vec<ObjectID>,
    transfer_chain: Vec<TransferRecord>,
    wrapped_then_unwrapped: Vec<ObjectID>,
    wrapped_then_deleted: Vec<ObjectID>,
    deleted: Vec<ObjectID>,
    borrow_mut_pairs: Vec<BorrowMutPair>,
}

#[derive(Serialize)]
struct TransferRecord {
    object: ObjectID,
    to: IotaAddress,
}

#[derive(Serialize)]
struct BorrowMutPair {
    parent: ObjectID,
    dof_child: ObjectID,
}

/// All the context a scenario needs to do its work.
#[derive(Clone)]
struct Ctx {
    client: IotaClient,
    keystore: Arc<FileBasedKeystore>,
    sender: IotaAddress,
    pkg: ObjectID,
    gas: ObjectID,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let client = IotaClientBuilder::default().build(&args.rpc_url).await?;
    let keystore_path = iota_config_dir()?.join(IOTA_KEYSTORE_FILENAME);
    let keystore = Arc::new(FileBasedKeystore::new(&keystore_path)?);
    let sender = match args.sender {
        Some(s) => s,
        None => *keystore
            .addresses()
            .first()
            .ok_or_else(|| anyhow!("no addresses in keystore at {keystore_path:?}"))?,
    };
    eprintln!("[workload] sender: {sender}");

    let coins = ensure_gas_coins(&args.faucet_url, sender, &client, NUM_PARALLEL_COINS).await?;
    eprintln!("[workload] {} gas coins available; reserving the first for one-off ops",
              coins.len());

    let pkg = match args.skip_publish {
        Some(id) => {
            eprintln!("[workload] reusing package: {id}");
            id
        }
        None => publish_package(&client, &keystore, sender, &args.package_path, coins[0]).await?,
    };

    // Each scenario gets a dedicated gas coin so they don't fight over object
    // versions. Coins[0] was used for publish; reuse for scenario 0 — the
    // publish tx already updated the coin in place, the SDK will re-resolve.
    eprintln!("[workload] launching {} scenarios in parallel", NUM_PARALLEL_COINS);
    let mk_ctx = |i: usize| Ctx {
        client: client.clone(),
        keystore: keystore.clone(),
        sender,
        pkg,
        gas: coins[i],
    };

    let (r1, r2, r3, r4, r5, r6, r7, r8) = tokio::try_join!(
        scenario_parents_with_dfs(mk_ctx(0)),
        scenario_parents_with_dofs(mk_ctx(1)),
        scenario_df_lifecycle(mk_ctx(2)),
        scenario_transfer_chain(mk_ctx(3)),
        scenario_wrap_unwrap(mk_ctx(4)),
        scenario_wrap_delete(mk_ctx(5)),
        scenario_plain_delete(mk_ctx(6)),
        scenario_borrow_mut_quirk(mk_ctx(7)),
    )?;

    let scenarios = Scenarios {
        parents_with_dfs: r1,
        parents_with_dofs: r2,
        df_lifecycle_parents: r3,
        transfer_chain: r4,
        wrapped_then_unwrapped: r5,
        wrapped_then_deleted: r6,
        deleted: r7,
        borrow_mut_pairs: r8,
    };

    let final_cp = client.read_api().get_latest_checkpoint_sequence_number().await?;
    eprintln!("[workload] final checkpoint: {final_cp}");

    let manifest = Manifest {
        package_id: pkg,
        sender,
        final_checkpoint: final_cp,
        scenarios,
    };
    let out = serde_json::to_string_pretty(&manifest)?;
    println!("{out}");
    if let Some(path) = args.output {
        std::fs::write(&path, &out)?;
        eprintln!("[workload] manifest written to {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

async fn scenario_parents_with_dfs(ctx: Ctx) -> Result<Vec<ObjectID>> {
    eprintln!("[workload] scenario 1: parents_with_dfs (gas={})", short(&ctx.gas));
    let parents = mint_parents(&ctx, 10, 0).await?;
    for parent in &parents {
        for k in 0..10u64 {
            call_move(&ctx, "add_df", vec![obj_arg(*parent), pure(k)?, pure(k * 7)?]).await?;
        }
    }
    eprintln!("[workload] scenario 1 done");
    Ok(parents)
}

async fn scenario_parents_with_dofs(ctx: Ctx) -> Result<Vec<ObjectID>> {
    eprintln!("[workload] scenario 2: parents_with_dofs (gas={})", short(&ctx.gas));
    let parents = mint_parents(&ctx, 10, 100).await?;
    for parent in &parents {
        for k in 0..10u64 {
            call_move(&ctx, "add_dof_to", vec![obj_arg(*parent), pure(k)?, pure(k * 11)?]).await?;
        }
    }
    eprintln!("[workload] scenario 2 done");
    Ok(parents)
}

async fn scenario_df_lifecycle(ctx: Ctx) -> Result<Vec<ObjectID>> {
    eprintln!("[workload] scenario 3: df_lifecycle (gas={})", short(&ctx.gas));
    let parents = mint_parents(&ctx, 3, 200).await?;
    for parent in &parents {
        call_move(&ctx, "add_df", vec![obj_arg(*parent), pure(42u64)?, pure(1000u64)?]).await?;
    }
    for parent in &parents {
        call_move(&ctx, "remove_df", vec![obj_arg(*parent), pure(42u64)?]).await?;
    }
    for parent in &parents {
        call_move(&ctx, "add_df", vec![obj_arg(*parent), pure(42u64)?, pure(2000u64)?]).await?;
    }
    eprintln!("[workload] scenario 3 done");
    Ok(parents)
}

async fn scenario_transfer_chain(ctx: Ctx) -> Result<Vec<TransferRecord>> {
    eprintln!("[workload] scenario 4: transfer_chain (gas={})", short(&ctx.gas));
    let recipient: IotaAddress = "0xabababababababababababababababababababababababababababababababab"
        .parse()
        .unwrap();
    let parents = mint_parents(&ctx, 10, 300).await?;
    let mut out = vec![];
    for parent in &parents {
        let tx_data = ctx.client.transaction_builder()
            .transfer_object(ctx.sender, *parent, Some(ctx.gas), GAS_BUDGET, recipient)
            .await?;
        execute(&ctx, tx_data).await?;
        out.push(TransferRecord { object: *parent, to: recipient });
    }
    eprintln!("[workload] scenario 4 done");
    Ok(out)
}

async fn scenario_wrap_unwrap(ctx: Ctx) -> Result<Vec<ObjectID>> {
    eprintln!("[workload] scenario 5: wrap -> unwrap (gas={})", short(&ctx.gas));
    let mut out = vec![];
    for i in 0..10u64 {
        let r = call_move(&ctx, "mint_child_to_sender", vec![pure(400 + i)?]).await?;
        let child = first_created_of(&r, "::workload::Child")?;
        let r = call_move(&ctx, "wrap_self_and_send", vec![obj_arg(child)]).await?;
        let wrapper = first_created_of(&r, "::workload::Wrapper")?;
        call_move(&ctx, "unwrap_and_send", vec![obj_arg(wrapper)]).await?;
        out.push(child);
    }
    eprintln!("[workload] scenario 5 done");
    Ok(out)
}

async fn scenario_wrap_delete(ctx: Ctx) -> Result<Vec<ObjectID>> {
    eprintln!("[workload] scenario 6: wrap + delete (gas={})", short(&ctx.gas));
    let mut out = vec![];
    for i in 0..10u64 {
        let r = call_move(&ctx, "mint_child_to_sender", vec![pure(500 + i)?]).await?;
        let child = first_created_of(&r, "::workload::Child")?;
        call_move(&ctx, "wrap_and_delete", vec![obj_arg(child)]).await?;
        out.push(child);
    }
    eprintln!("[workload] scenario 6 done");
    Ok(out)
}

async fn scenario_plain_delete(ctx: Ctx) -> Result<Vec<ObjectID>> {
    eprintln!("[workload] scenario 7: plain delete (gas={})", short(&ctx.gas));
    let mut out = vec![];
    for i in 0..10u64 {
        let r = call_move(&ctx, "mint_parent_to_sender", vec![pure(600 + i)?]).await?;
        let parent = first_created_of(&r, "::workload::Parent")?;
        call_move(&ctx, "delete_parent", vec![obj_arg(parent)]).await?;
        out.push(parent);
    }
    eprintln!("[workload] scenario 7 done");
    Ok(out)
}

async fn scenario_borrow_mut_quirk(ctx: Ctx) -> Result<Vec<BorrowMutPair>> {
    eprintln!("[workload] scenario 8: borrow_mut quirk (gas={})", short(&ctx.gas));
    let mut out = vec![];
    for i in 0..3u64 {
        let r = call_move(&ctx, "mint_parent_to_sender", vec![pure(700 + i)?]).await?;
        let parent = first_created_of(&r, "::workload::Parent")?;
        let r = call_move(&ctx, "add_dof_to", vec![obj_arg(parent), pure(0u64)?, pure(777u64)?]).await?;
        let dof_child = first_created_of(&r, "::workload::Child")?;
        call_move(&ctx, "add_df_to_dof_child",
                  vec![obj_arg(parent), pure(0u64)?, pure(1u64)?, pure(999u64)?]).await?;
        out.push(BorrowMutPair { parent, dof_child });
    }
    eprintln!("[workload] scenario 8 done");
    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wrap a u64 as a JSON string — the RPC won't accept JSON numbers for u64.
fn pure(v: u64) -> Result<IotaJsonValue> {
    IotaJsonValue::new(serde_json::Value::String(v.to_string())).map_err(|e| anyhow!(e))
}

fn obj_arg(id: ObjectID) -> IotaJsonValue {
    IotaJsonValue::new(json!(id.to_string())).expect("object id is always serializable")
}

fn short(id: &ObjectID) -> String {
    let s = id.to_string();
    format!("{}...{}", &s[..6], &s[s.len() - 4..])
}

fn first_created_of(r: &IotaTransactionBlockResponse, type_substr: &str) -> Result<ObjectID> {
    let changes = r.object_changes.as_ref().ok_or_else(|| anyhow!("no object_changes in tx response"))?;
    for c in changes {
        if let ObjectChange::Created { object_id, object_type, .. } = c {
            if object_type.to_string().contains(type_substr) {
                return Ok(*object_id);
            }
        }
    }
    bail!("no created object of type containing {type_substr:?} in tx response")
}

/// Make sure the sender owns at least `n` IOTA coins by hitting the faucet
/// repeatedly. The localnet faucet creates one new coin per request.
async fn ensure_gas_coins(faucet_url: &str, sender: IotaAddress, client: &IotaClient,
                          n: usize) -> Result<Vec<ObjectID>> {
    for attempt in 0..(n * 2) {
        let coins = client.coin_read_api()
            .get_coins(sender, None, None, Some(n + 4))
            .await?;
        if coins.data.len() >= n {
            return Ok(coins.data.into_iter().map(|c| c.coin_object_id).collect());
        }
        let body = json!({"FixedAmountRequest": {"recipient": sender.to_string()}});
        reqwest::Client::new().post(faucet_url).json(&body).send().await
            .with_context(|| format!("faucet POST to {faucet_url}"))?;
        eprintln!("[workload] faucet -> {sender} (have {}/{}, attempt {})",
                  coins.data.len(), n, attempt + 1);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("failed to accumulate {n} gas coins for {sender}")
}

async fn publish_package(
    client: &IotaClient,
    keystore: &FileBasedKeystore,
    sender: IotaAddress,
    package_path: &PathBuf,
    gas: ObjectID,
) -> Result<ObjectID> {
    eprintln!("[workload] publishing package from {}", package_path.display());
    let build_config = BuildConfig {
        config: MoveBuildConfig {
            default_flavor: Some(Flavor::Iota),
            ..MoveBuildConfig::default()
        },
        run_bytecode_verifier: true,
        print_diags_to_stderr: false,
        chain_id: None,
    };
    let module = build_config.build(package_path)?;
    let tx_data = client.transaction_builder()
        .publish(
            sender,
            module.get_package_bytes(false),
            module.get_dependency_storage_package_ids(),
            gas,
            GAS_BUDGET,
        ).await?;
    let signature = keystore.sign_secure(&sender, &tx_data, Intent::iota_transaction())?;
    let r = client.quorum_driver_api()
        .execute_transaction_block(
            Transaction::from_data(tx_data, vec![signature]),
            IotaTransactionBlockResponseOptions::full_content(),
            ExecuteTransactionRequestType::WaitForLocalExecution,
        )
        .await
        .context("publish execute_transaction_block")?;
    if let Some(eff) = &r.effects {
        if !eff.status().is_ok() {
            bail!("publish tx failed: {:?}", eff.status());
        }
    }
    let pkg = r.object_changes.as_ref().unwrap().iter().find_map(|c| {
        if let ObjectChange::Published { package_id, .. } = c {
            Some(*package_id)
        } else { None }
    }).ok_or_else(|| anyhow!("no Published object change in publish response"))?;
    eprintln!("[workload] package published: {pkg}");
    Ok(pkg)
}

async fn mint_parents(ctx: &Ctx, count: u64, start_value: u64) -> Result<Vec<ObjectID>> {
    let mut out = vec![];
    for i in 0..count {
        let r = call_move(ctx, "mint_parent_to_sender", vec![pure(start_value + i)?]).await?;
        out.push(first_created_of(&r, "::workload::Parent")?);
    }
    Ok(out)
}

async fn call_move(ctx: &Ctx, function: &str, call_args: Vec<IotaJsonValue>)
    -> Result<IotaTransactionBlockResponse>
{
    let tx_data = ctx.client.transaction_builder()
        .move_call(ctx.sender, ctx.pkg, "workload", function, vec![], call_args,
                   Some(ctx.gas), GAS_BUDGET, None)
        .await
        .with_context(|| format!("build move_call workload::{function}"))?;
    execute(ctx, tx_data).await
}

async fn execute(ctx: &Ctx, tx_data: TransactionData) -> Result<IotaTransactionBlockResponse> {
    let signature = ctx.keystore.sign_secure(&ctx.sender, &tx_data, Intent::iota_transaction())?;
    let r = ctx.client.quorum_driver_api()
        .execute_transaction_block(
            Transaction::from_data(tx_data, vec![signature]),
            IotaTransactionBlockResponseOptions::full_content(),
            ExecuteTransactionRequestType::WaitForLocalExecution,
        )
        .await
        .context("execute_transaction_block")?;
    if let Some(eff) = &r.effects {
        if !eff.status().is_ok() {
            bail!("tx failed: {:?}", eff.status());
        }
    }
    Ok(r)
}

