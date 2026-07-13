// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

// The protocol config override used to enable the feature flags is
// thread-local, so these tests only work under the simulator, where all nodes
// share one thread.
#[cfg(msim)]
mod simtests {
    use std::time::Duration;

    use iota_config::transaction_deny_config::TransactionDenyConfigBuilder;
    use iota_macros::sim_test;
    use iota_protocol_config::ProtocolConfig;
    use iota_sdk_types::Address;
    use test_cluster::TestClusterBuilder;

    /// Validators announce their local deny config through consensus on
    /// startup; the stake-weighted aggregate must become active on every
    /// validator.
    #[sim_test]
    async fn deny_rule_proposals_converge_across_validators() {
        telemetry_subscribers::init_for_testing();
        let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut config| {
            config.set_enable_pcool_flow_for_testing(true);
            config.set_deny_rule_governance_for_testing(true);
            config
        });

        let denied = Address::new([42u8; 32]);
        let deny_config = TransactionDenyConfigBuilder::new()
            .add_denied_address(denied)
            .build();

        let test_cluster = TestClusterBuilder::new()
            .with_transaction_deny_config(deny_config)
            .build()
            .await;

        // Proposals are submitted on startup and aggregated at commit
        // boundaries; poll until the rule is active on every validator.
        let mut converged = false;
        for _ in 0..120 {
            converged = test_cluster
                .swarm
                .validator_node_handles()
                .iter()
                .all(|handle| {
                    handle.with(|node| {
                        node.state()
                            .epoch_store_for_testing()
                            .get_active_deny_rules()
                            .denied_addresses
                            .contains(&denied)
                    })
                });
            if converged {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(
            converged,
            "governance deny rules did not become active on all validators"
        );
    }
}
