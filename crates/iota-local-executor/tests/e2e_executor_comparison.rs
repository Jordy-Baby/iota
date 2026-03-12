// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests that spin up a local test cluster and compare transaction
//! simulation results across the different executor backends (JSON-RPC,
//! gRPC) and against the node's own dry-run API.

use iota_json_rpc_types::IotaTransactionBlockEffectsAPI;
use iota_local_executor::{GrpcExecutor, JsonRpcExecutor, ObjectFetchMode, VmChecks};
use iota_test_transaction_builder::TestTransactionBuilder;
use iota_types::{base_types::IotaAddress, effects::TransactionEffectsAPI, gas::GasCostSummary};
use test_cluster::TestClusterBuilder;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Summary extracted from each executor's result, comparable across backends.
#[derive(Debug)]
struct SimulationSummary {
    success: bool,
    gas: GasCostSummary,
    created_count: usize,
    mutated_count: usize,
    deleted_count: usize,
    events_count: usize,
}

impl SimulationSummary {
    fn assert_matches(&self, other: &Self, left_name: &str, right_name: &str) {
        assert_eq!(
            self.success, other.success,
            "{left_name} success={} but {right_name} success={}",
            self.success, other.success,
        );
        assert_eq!(
            self.gas, other.gas,
            "{left_name} gas != {right_name} gas\n  {left_name}: {:?}\n  {right_name}: {:?}",
            self.gas, other.gas,
        );
        assert_eq!(
            self.created_count, other.created_count,
            "{left_name} created={} but {right_name} created={}",
            self.created_count, other.created_count,
        );
        assert_eq!(
            self.mutated_count, other.mutated_count,
            "{left_name} mutated={} but {right_name} mutated={}",
            self.mutated_count, other.mutated_count,
        );
        assert_eq!(
            self.deleted_count, other.deleted_count,
            "{left_name} deleted={} but {right_name} deleted={}",
            self.deleted_count, other.deleted_count,
        );
        assert_eq!(
            self.events_count, other.events_count,
            "{left_name} events={} but {right_name} events={}",
            self.events_count, other.events_count,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Simulate a simple IOTA transfer via JsonRpcExecutor, GrpcExecutor, and the
/// node's dry-run API, then compare that all three produce the same result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_executors_transfer() {
    // Spin up a local test cluster with gRPC enabled.
    let test_cluster = TestClusterBuilder::new()
        .with_fullnode_enable_grpc_api(true)
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    let (sender, gas) = test_cluster
        .wallet
        .get_one_gas_object()
        .await
        .unwrap()
        .unwrap();
    let rgp = test_cluster.get_reference_gas_price().await;
    let recipient = IotaAddress::random_for_testing_only();

    let tx_data = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_iota(Some(1_000_000), recipient)
        .build();

    // 1. Node dry-run via JSON-RPC.
    let dry_run = test_cluster
        .iota_client()
        .read_api()
        .dry_run_transaction_block(tx_data.clone())
        .await
        .expect("dry-run should succeed");

    let node_summary = {
        let effects = &dry_run.effects;
        let is_success = matches!(
            effects.status(),
            iota_json_rpc_types::IotaExecutionStatus::Success
        );
        SimulationSummary {
            success: is_success,
            gas: effects.gas_cost_summary().clone(),
            created_count: effects.created().len(),
            mutated_count: effects.mutated().len(),
            deleted_count: effects.deleted().len(),
            events_count: dry_run.events.data.len(),
        }
    };
    assert!(node_summary.success, "node dry-run should succeed");

    // 2. JsonRpcExecutor (JSON-RPC backend).
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .expect("should connect to JSON-RPC");

    let local = JsonRpcExecutor::new(iota_client)
        .await
        .expect("JsonRpcExecutor::new should succeed");

    let local_result = local
        .simulate_transaction(tx_data.clone(), VmChecks::Enabled)
        .await
        .expect("JsonRpcExecutor simulate should succeed");

    let local_summary = {
        let effects = &local_result.effects;
        SimulationSummary {
            success: effects.status().is_ok(),
            gas: effects.gas_cost_summary().clone(),
            created_count: effects.created().len(),
            mutated_count: effects.mutated().len(),
            deleted_count: effects.deleted().len(),
            events_count: local_result
                .events
                .as_ref()
                .map(|e| e.data.len())
                .unwrap_or(0),
        }
    };
    assert!(local_summary.success, "JsonRpcExecutor should succeed");

    // 3. GrpcExecutor (gRPC backend).
    let grpc_url = test_cluster.grpc_url();
    let grpc_client = iota_grpc_client::Client::connect(grpc_url)
        .await
        .expect("should connect to gRPC");

    let grpc = GrpcExecutor::new(grpc_client)
        .await
        .expect("GrpcExecutor::new should succeed");

    let grpc_result = grpc
        .simulate_transaction(tx_data.clone(), VmChecks::Enabled)
        .await
        .expect("GrpcExecutor simulate should succeed");

    let grpc_summary = {
        let effects = &grpc_result.effects;
        SimulationSummary {
            success: effects.status().is_ok(),
            gas: effects.gas_cost_summary().clone(),
            created_count: effects.created().len(),
            mutated_count: effects.mutated().len(),
            deleted_count: effects.deleted().len(),
            events_count: grpc_result
                .events
                .as_ref()
                .map(|e| e.data.len())
                .unwrap_or(0),
        }
    };
    assert!(grpc_summary.success, "GrpcExecutor should succeed");

    // Compare all three.
    node_summary.assert_matches(&local_summary, "node", "JsonRpcExecutor");
    node_summary.assert_matches(&grpc_summary, "node", "GrpcExecutor");
}

/// Simulate a dev-inspect (VmChecks::Disabled) transfer via both executors and
/// verify they agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_executors_dev_inspect() {
    let test_cluster = TestClusterBuilder::new()
        .with_fullnode_enable_grpc_api(true)
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    let (sender, gas) = test_cluster
        .wallet
        .get_one_gas_object()
        .await
        .unwrap()
        .unwrap();
    let rgp = test_cluster.get_reference_gas_price().await;
    let recipient = IotaAddress::random_for_testing_only();

    let tx_data = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_iota(Some(500_000), recipient)
        .build();

    // JsonRpcExecutor with VmChecks::Disabled (dev-inspect mode).
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::new(iota_client)
        .await
        .expect("JsonRpcExecutor::new should succeed");

    let local_result = local
        .simulate_transaction(tx_data.clone(), VmChecks::Disabled)
        .await
        .expect("JsonRpcExecutor dev-inspect should succeed");
    assert!(
        local_result.effects.status().is_ok(),
        "JsonRpcExecutor dev-inspect should succeed"
    );

    // GrpcExecutor with VmChecks::Disabled.
    let grpc_client = iota_grpc_client::Client::connect(test_cluster.grpc_url())
        .await
        .unwrap();

    let grpc = GrpcExecutor::new(grpc_client)
        .await
        .expect("GrpcExecutor::new should succeed");

    let grpc_result = grpc
        .simulate_transaction(tx_data.clone(), VmChecks::Disabled)
        .await
        .expect("GrpcExecutor dev-inspect should succeed");
    assert!(
        grpc_result.effects.status().is_ok(),
        "GrpcExecutor dev-inspect should succeed"
    );

    // Both should agree on gas costs and object changes.
    let local_gas = local_result.effects.gas_cost_summary();
    let grpc_gas = grpc_result.effects.gas_cost_summary();
    assert_eq!(local_gas, grpc_gas, "gas costs should match");

    assert_eq!(
        local_result.effects.created().len(),
        grpc_result.effects.created().len(),
        "created count should match"
    );
    assert_eq!(
        local_result.effects.mutated().len(),
        grpc_result.effects.mutated().len(),
        "mutated count should match"
    );
}

/// Verify that `ObjectFetchMode::UseLatestVersions` also produces a successful
/// simulation for both executors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_executors_latest_version_mode() {
    let test_cluster = TestClusterBuilder::new()
        .with_fullnode_enable_grpc_api(true)
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    let (sender, gas) = test_cluster
        .wallet
        .get_one_gas_object()
        .await
        .unwrap()
        .unwrap();
    let rgp = test_cluster.get_reference_gas_price().await;
    let recipient = IotaAddress::random_for_testing_only();

    let tx_data = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_iota(Some(100_000), recipient)
        .build();

    // JsonRpcExecutor with UseLatestVersions.
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::new(iota_client)
        .await
        .expect("JsonRpcExecutor::new should succeed")
        .with_fetch_mode(ObjectFetchMode::UseLatestVersions);

    let local_result = local
        .simulate_transaction(tx_data.clone(), VmChecks::Disabled)
        .await
        .expect("JsonRpcExecutor latest-version simulate should succeed");
    assert!(
        local_result.effects.status().is_ok(),
        "JsonRpcExecutor latest-version should succeed"
    );

    // GrpcExecutor with UseLatestVersions.
    let grpc_client = iota_grpc_client::Client::connect(test_cluster.grpc_url())
        .await
        .unwrap();

    let grpc = GrpcExecutor::new(grpc_client)
        .await
        .expect("GrpcExecutor::new should succeed")
        .with_fetch_mode(ObjectFetchMode::UseLatestVersions);

    let grpc_result = grpc
        .simulate_transaction(tx_data.clone(), VmChecks::Disabled)
        .await
        .expect("GrpcExecutor latest-version simulate should succeed");
    assert!(
        grpc_result.effects.status().is_ok(),
        "GrpcExecutor latest-version should succeed"
    );

    // Both should produce the same gas costs.
    assert_eq!(
        local_result.effects.gas_cost_summary(),
        grpc_result.effects.gas_cost_summary(),
        "gas costs should match in latest-version mode"
    );
}

/// Simulate a Move call (staking) that involves shared objects, and compare
/// results across executors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_executors_staking_move_call() {
    let test_cluster = TestClusterBuilder::new()
        .with_fullnode_enable_grpc_api(true)
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    // Get a validator address for the staking call.
    let validator = test_cluster
        .iota_client()
        .governance_api()
        .get_latest_iota_system_state()
        .await
        .expect("should get system state")
        .active_validators()
        .first()
        .expect("should have at least one validator")
        .iota_address;

    // The test cluster wallet has multiple addresses, each with coins.
    // Use one coin as gas and get a second coin for staking.
    let sender = test_cluster.wallet.get_addresses()[0];
    let rgp = test_cluster.get_reference_gas_price().await;
    let all_coins = test_cluster
        .wallet
        .get_gas_objects_owned_by_address(sender, 5)
        .await
        .unwrap();
    assert!(
        all_coins.len() >= 2,
        "need at least 2 coins, got {}",
        all_coins.len()
    );
    let gas = all_coins[0];
    let stake_coin = all_coins[1];

    let tx_data = TestTransactionBuilder::new(sender, gas, rgp)
        .call_staking(stake_coin, validator)
        .build();

    // Node dry-run.
    let dry_run = test_cluster
        .iota_client()
        .read_api()
        .dry_run_transaction_block(tx_data.clone())
        .await
        .expect("dry-run should succeed");
    let node_success = matches!(
        dry_run.effects.status(),
        iota_json_rpc_types::IotaExecutionStatus::Success
    );
    assert!(node_success, "node dry-run staking should succeed");

    // JsonRpcExecutor — use dev-inspect mode since the staking call involves
    // shared objects whose versions may shift between fetch and simulate.
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();
    let local = JsonRpcExecutor::new(iota_client).await.unwrap();
    let local_result = local
        .simulate_transaction(tx_data.clone(), VmChecks::Disabled)
        .await
        .expect("JsonRpcExecutor staking should succeed");
    assert!(
        local_result.effects.status().is_ok(),
        "JsonRpcExecutor staking should succeed: {:?}",
        local_result.effects.status()
    );

    // GrpcExecutor.
    let grpc_client = iota_grpc_client::Client::connect(test_cluster.grpc_url())
        .await
        .unwrap();
    let grpc = GrpcExecutor::new(grpc_client).await.unwrap();
    let grpc_result = grpc
        .simulate_transaction(tx_data.clone(), VmChecks::Disabled)
        .await
        .expect("GrpcExecutor staking should succeed");
    assert!(
        grpc_result.effects.status().is_ok(),
        "GrpcExecutor staking should succeed: {:?}",
        grpc_result.effects.status()
    );

    // Both local executors should agree on gas and object counts.
    assert_eq!(
        local_result.effects.gas_cost_summary(),
        grpc_result.effects.gas_cost_summary(),
        "staking gas costs should match"
    );
    assert_eq!(
        local_result.effects.created().len(),
        grpc_result.effects.created().len(),
        "staking created count should match"
    );
}
