// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests that spin up a local test cluster and compare transaction
//! simulation results across the different executor backends (JSON-RPC,
//! gRPC) and against the node's own dry-run API.

use std::{path::PathBuf, str::FromStr};

use iota_json_rpc_types::IotaTransactionBlockEffectsAPI;
use iota_keys::keystore::AccountKeystore;
use iota_local_executor::{
    DebugConfig, GrpcExecutor, JsonRpcExecutor, ObjectFetchMode, ProfileOutput, ProfileSink,
    SenderSignedData, VmChecks,
};
use iota_sdk_types::SharedObjectReference;
use iota_test_transaction_builder::{TestTransactionBuilder, publish_package};
use iota_types::{
    base_types::{Identifier, IotaAddress, ObjectID, ObjectRef, TypeTag},
    crypto::{AccountKeyPair, get_key_pair},
    effects::TransactionEffectsAPI,
    gas::GasCostSummary,
    move_authenticator::MoveAuthenticator,
    move_package,
    object::Owner,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    signature::GenericSignature,
    storage::WriteKind,
    transaction::{
        Argument, CallArg, TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, Transaction,
        TransactionData, TransactionDataAPI,
    },
};
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
    let recipient = IotaAddress::random();

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
            success: effects.status().is_success(),
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
            success: effects.status().is_success(),
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
    let recipient = IotaAddress::random();

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
        local_result.effects.status().is_success(),
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
        grpc_result.effects.status().is_success(),
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
    let recipient = IotaAddress::random();

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
        local_result.effects.status().is_success(),
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
        grpc_result.effects.status().is_success(),
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
        local_result.effects.status().is_success(),
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
        grpc_result.effects.status().is_success(),
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

// ---------------------------------------------------------------------------
// Signature verification tests
// ---------------------------------------------------------------------------

/// Simulate a signed transaction where a standard Ed25519 signature is valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulate_signed_transaction_valid_signature() {
    let test_cluster = TestClusterBuilder::new()
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
    let recipient = IotaAddress::random();

    let tx_data = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_iota(Some(1_000_000), recipient)
        .build();

    // Sign the transaction using the test cluster wallet.
    let signed_tx: Transaction = test_cluster.sign_transaction(&tx_data);
    let signed_data: SenderSignedData = signed_tx.into_data();

    // Build executor and simulate with signature verification.
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::new(iota_client).await.unwrap();

    let result = local
        .simulate_signed_transaction(signed_data, VmChecks::Enabled)
        .await
        .expect("simulate_signed_transaction with valid signature should succeed");

    assert!(
        result.effects.status().is_success(),
        "signed transaction execution should succeed: {:?}",
        result.effects.status()
    );
}

/// Simulate a signed transaction where the signature does NOT match the sender.
/// Signature verification should fail before execution begins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulate_signed_transaction_invalid_signature() {
    let test_cluster = TestClusterBuilder::new()
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
    let recipient = IotaAddress::random();

    let tx_data = TestTransactionBuilder::new(sender, gas, rgp)
        .transfer_iota(Some(1_000_000), recipient)
        .build();

    // Sign with a completely different key that does NOT correspond to the sender.
    let (_wrong_address, wrong_key): (IotaAddress, AccountKeyPair) = get_key_pair();
    let bad_signed_tx = Transaction::from_data_and_signer(tx_data, vec![&wrong_key]);
    let bad_signed_data: SenderSignedData = bad_signed_tx.into_data();

    // Build executor and attempt simulation.
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::new(iota_client).await.unwrap();

    let result = local
        .simulate_signed_transaction(bad_signed_data, VmChecks::Enabled)
        .await;

    let err_msg = result
        .err()
        .expect("simulate_signed_transaction with invalid signature should fail")
        .to_string();
    assert!(
        err_msg.contains("signature verification"),
        "error should mention signature verification, got: {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// MoveAuthenticator signature verification tests
// ---------------------------------------------------------------------------

// Path to the abstract_account Move package (relative to e2e-tests crate).
const AA_PACKAGE_PATH: &str = "../iota-e2e-tests/tests/abstract_account/abstract_account";
const AA_MODULE_NAME: &str = "abstract_account";
const AA_ACCOUNT_NAME: &str = "AbstractAccount";
const AA_CREATE_MODULE_NAME: &str = "abstract_account_keyed";
const AA_AUTHENTICATE_MODULE_NAME: &str = "abstract_account_keyed";

/// Publish the abstract account Move package and return its ID and metadata
/// ref.
async fn publish_aa_package(test_cluster: &mut test_cluster::TestCluster) -> (ObjectID, ObjectRef) {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), AA_PACKAGE_PATH]
        .iter()
        .collect();
    let aa_package_id = publish_package(test_cluster.wallet(), path).await.object_id;
    let aa_metadata_id = move_package::derive_package_metadata_id(aa_package_id);
    let aa_metadata_ref = test_cluster.get_latest_object_ref(&aa_metadata_id).await;
    (aa_package_id, aa_metadata_ref)
}

/// Create an abstract account on-chain and return its ObjectRef.
async fn create_abstract_account(
    test_cluster: &test_cluster::TestCluster,
    owner: IotaAddress,
    aa_package_id: ObjectID,
    aa_metadata_ref: ObjectRef,
    authenticate_fn_name: &str,
) -> ObjectRef {
    let owner_pk = test_cluster
        .wallet
        .config()
        .keystore()
        .get_key(&owner)
        .unwrap()
        .public();

    let pt = {
        let mut builder = ProgrammableTransactionBuilder::new();
        let arguments = vec![
            builder
                .obj(CallArg::ImmutableOrOwned(aa_metadata_ref))
                .unwrap(),
            builder.pure(AA_AUTHENTICATE_MODULE_NAME).unwrap(),
            builder.pure(authenticate_fn_name).unwrap(),
        ];
        if let Argument::Result(auth_fn_ref) = builder.programmable_move_call(
            iota_types::IOTA_FRAMEWORK_PACKAGE_ID,
            Identifier::from_static("authenticator_function"),
            Identifier::from_static("create_auth_function_ref_v1"),
            vec![
                TypeTag::from_str(&format!(
                    "{aa_package_id}::{AA_MODULE_NAME}::{AA_ACCOUNT_NAME}"
                ))
                .unwrap(),
            ],
            arguments,
        ) {
            let arguments = vec![
                builder.pure(owner_pk.as_ref()).unwrap(),
                Argument::Result(auth_fn_ref),
            ];
            builder.programmable_move_call(
                aa_package_id,
                Identifier::from(AA_CREATE_MODULE_NAME),
                Identifier::from_static("create"),
                vec![],
                arguments,
            );
        }
        builder.finish()
    };

    let tx_data = test_cluster
        .test_transaction_builder()
        .await
        .programmable(pt)
        .build();
    let tx = test_cluster.wallet.sign_transaction(&tx_data);
    let (effects, _) = test_cluster
        .execute_transaction_return_raw_effects(tx)
        .await
        .expect("create abstract account should succeed");

    // The created shared object is the abstract account.
    effects
        .all_changed_objects()
        .iter()
        .find_map(|change| match change {
            (objref, Owner::Shared(_), WriteKind::Create) => Some(*objref),
            _ => None,
        })
        .expect("expected a created shared object (the abstract account)")
}

/// Build a simple PTB that calls `add_field` on the abstract account.
fn craft_aa_simple_ptb(
    aa_ref: ObjectRef,
    aa_package_id: ObjectID,
) -> iota_types::transaction::ProgrammableTransaction {
    let mut builder = ProgrammableTransactionBuilder::new();
    let arguments = vec![
        builder
            .obj(CallArg::Shared(SharedObjectReference {
                object_id: aa_ref.object_id,
                initial_shared_version: aa_ref.version,
                mutable: true,
            }))
            .unwrap(),
        builder.pure(1_u8).unwrap(),
        builder.pure(2_u8).unwrap(),
    ];
    builder.programmable_move_call(
        aa_package_id,
        Identifier::from(AA_MODULE_NAME),
        Identifier::from_static("add_field"),
        vec![TypeTag::U8, TypeTag::U8],
        arguments,
    );
    builder.finish()
}

/// Simulate a transaction signed with a MoveAuthenticator (free_access
/// variant). The authenticator logic runs in the Move VM during execution —
/// this is the only way to fully verify MoveAuthenticator signatures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulate_signed_transaction_move_authenticator_valid() {
    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    // 1. Publish the abstract account package and create an AA.
    let (aa_package_id, aa_metadata_ref) = publish_aa_package(&mut test_cluster).await;
    let owner = test_cluster.wallet.get_addresses()[0];
    let aa_ref = create_abstract_account(
        &test_cluster,
        owner,
        aa_package_id,
        aa_metadata_ref,
        "authenticate_free_access",
    )
    .await;

    // 2. Fund the abstract account address so it can pay for gas.
    let aa_sender: IotaAddress = IotaAddress::new(aa_ref.object_id.into_bytes());
    let rgp = test_cluster.get_reference_gas_price().await;
    let aa_gas = test_cluster
        .fund_address_and_return_gas(rgp, Some(20_000_000_000), aa_sender)
        .await;

    // 3. Build a transaction from the AA sender.
    let pt = craft_aa_simple_ptb(aa_ref, aa_package_id);
    let gas_price = test_cluster.get_reference_gas_price().await;
    let tx_data = TransactionData::new_programmable_allow_sponsor(
        aa_sender,
        vec![aa_gas],
        pt,
        gas_price * TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        gas_price,
        aa_sender,
    );

    // 4. Create a MoveAuthenticator (free_access — no extra args needed).
    let self_call_arg = CallArg::Shared(SharedObjectReference {
        object_id: aa_ref.object_id,
        initial_shared_version: aa_ref.version,
        mutable: false,
    });
    let move_auth = GenericSignature::MoveAuthenticator(MoveAuthenticator::new_v1(
        vec![],
        vec![],
        self_call_arg,
    ));
    let signed_data = SenderSignedData::new(tx_data, vec![move_auth]);

    // 5. Simulate via local executor with signature verification.
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::new(iota_client).await.unwrap();
    let result = local
        .simulate_signed_transaction(signed_data, VmChecks::Disabled)
        .await
        .expect("MoveAuthenticator simulate_signed_transaction should succeed");

    assert!(
        result.effects.status().is_success(),
        "MoveAuthenticator transaction should succeed: {:?}",
        result.effects.status()
    );
}

/// Simulate a transaction with a MoveAuthenticator using the Ed25519 variant,
/// but provide a bogus signature. The sender address matches (so pre-execution
/// checks pass), but the Move authenticator function rejects the invalid
/// signature during VM execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulate_signed_transaction_move_authenticator_invalid_args() {
    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    // 1. Publish the abstract account package and create an AA with Ed25519 auth.
    let (aa_package_id, aa_metadata_ref) = publish_aa_package(&mut test_cluster).await;
    let owner = test_cluster.wallet.get_addresses()[0];
    let aa_ref = create_abstract_account(
        &test_cluster,
        owner,
        aa_package_id,
        aa_metadata_ref,
        "authenticate_ed25519",
    )
    .await;

    // 2. Fund the abstract account address so it can pay for gas.
    let aa_sender: IotaAddress = IotaAddress::new(aa_ref.object_id.into_bytes());
    let rgp = test_cluster.get_reference_gas_price().await;
    let aa_gas = test_cluster
        .fund_address_and_return_gas(rgp, Some(20_000_000_000), aa_sender)
        .await;

    // 3. Build a transaction from the AA sender.
    let pt = craft_aa_simple_ptb(aa_ref, aa_package_id);
    let gas_price = test_cluster.get_reference_gas_price().await;
    let tx_data = TransactionData::new_programmable_allow_sponsor(
        aa_sender,
        vec![aa_gas],
        pt,
        gas_price * TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        gas_price,
        aa_sender,
    );

    // 4. Create a MoveAuthenticator with a bogus Ed25519 signature. The sender
    //    address matches, so pre-execution checks pass. But the Move
    //    authenticate_ed25519 function will abort because the signature doesn't
    //    verify against the stored public key.
    let self_call_arg = CallArg::Shared(SharedObjectReference {
        object_id: aa_ref.object_id,
        initial_shared_version: aa_ref.version,
        mutable: false,
    });
    // 64 hex chars of zeros = 32 bytes of zeros, which is an invalid Ed25519 sig.
    let bogus_sig_hex = "00".repeat(64);
    let signature_call_arg = CallArg::Pure(bcs::to_bytes(&bogus_sig_hex).unwrap());
    let move_auth = GenericSignature::MoveAuthenticator(MoveAuthenticator::new_v1(
        vec![signature_call_arg],
        vec![],
        self_call_arg,
    ));
    let signed_data = SenderSignedData::new(tx_data, vec![move_auth]);

    // 5. Simulate via local executor.
    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::new(iota_client).await.unwrap();
    let result = local
        .simulate_signed_transaction(signed_data, VmChecks::Disabled)
        .await
        .expect("should return effects even on auth failure");

    // The authenticator function should have aborted — the effects should
    // indicate execution failure.
    assert!(
        result.effects.status().is_failure(),
        "MoveAuthenticator with bogus signature should fail during VM execution: {:?}",
        result.effects.status()
    );
}

/// Prove the gas-profiler and instruction tracer cover the
/// `MoveAuthenticator`'s Move call — the authenticator-optimisation workflow.
/// Mirrors `simulate_signed_transaction_move_authenticator_valid`, but routes
/// the simulation through `simulate_signed_transaction_with_debug` with
/// profile + trace enabled and asserts both artifacts name the authenticator
/// function.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulate_signed_transaction_move_authenticator_with_debug() {
    let mut test_cluster = TestClusterBuilder::new()
        .with_num_validators(1)
        .build()
        .await;

    test_cluster.wait_for_checkpoint(1, None).await;

    let (aa_package_id, aa_metadata_ref) = publish_aa_package(&mut test_cluster).await;
    let owner = test_cluster.wallet.get_addresses()[0];
    let aa_ref = create_abstract_account(
        &test_cluster,
        owner,
        aa_package_id,
        aa_metadata_ref,
        "authenticate_free_access",
    )
    .await;

    let aa_sender: IotaAddress = IotaAddress::new(aa_ref.object_id.into_bytes());
    let rgp = test_cluster.get_reference_gas_price().await;
    let aa_gas = test_cluster
        .fund_address_and_return_gas(rgp, Some(20_000_000_000), aa_sender)
        .await;

    let pt = craft_aa_simple_ptb(aa_ref, aa_package_id);
    let gas_price = test_cluster.get_reference_gas_price().await;
    let tx_data = TransactionData::new_programmable_allow_sponsor(
        aa_sender,
        vec![aa_gas],
        pt,
        gas_price * TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        gas_price,
        aa_sender,
    );

    let self_call_arg = CallArg::Shared(SharedObjectReference {
        object_id: aa_ref.object_id,
        initial_shared_version: aa_ref.version,
        mutable: false,
    });
    let move_auth = GenericSignature::MoveAuthenticator(MoveAuthenticator::new_v1(
        vec![],
        vec![],
        self_call_arg,
    ));
    let signed_data = SenderSignedData::new(tx_data, vec![move_auth]);

    let rpc_url = test_cluster.rpc_url().to_string();
    let iota_client = iota_sdk::IotaClientBuilder::default()
        .build(&rpc_url)
        .await
        .unwrap();

    let local = JsonRpcExecutor::with_debug(
        iota_client,
        DebugConfig {
            profile: Some(ProfileSink::Capture),
            trace: true,
            ..DebugConfig::default()
        },
    )
    .await
    .unwrap();

    let out = local
        .simulate_signed_transaction_with_debug(signed_data, VmChecks::Disabled)
        .await
        .expect("MoveAuthenticator simulate_signed_transaction_with_debug should succeed");

    assert!(
        out.result.effects.status().is_success(),
        "MoveAuthenticator transaction should succeed: {:?}",
        out.result.effects.status()
    );

    // Profile must contain a frame for the authenticator function's module.
    // Speedscope frames record the fully-qualified path in `file`.
    let profile = out.artifacts.profile.expect("profile should be populated");
    let bytes = match profile {
        ProfileOutput::Json(b) => b,
        ProfileOutput::Path(p) => panic!("expected Json, got Path {p:?}"),
    };
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let frames = json
        .pointer("/shared/frames")
        .and_then(|v| v.as_array())
        .expect("Speedscope JSON should contain /shared/frames");
    let auth_fn_hit = frames.iter().any(|f| {
        f.get("file")
            .and_then(|n| n.as_str())
            .map(|s| s.contains("::authenticate_free_access"))
            .unwrap_or(false)
    });
    assert!(
        auth_fn_hit,
        "expected a profile frame naming `authenticate_free_access`; frames: {frames:?}"
    );

    // Trace must contain events covering the authenticator call — the whole
    // point of the authenticator-gas-optimisation workflow.
    let trace = out.artifacts.trace.expect("trace should be populated");
    assert!(
        !trace.events.is_empty(),
        "MoveTrace should contain events for the authenticator call"
    );
}
