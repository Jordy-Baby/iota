// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

module gas_profiling_authenticator::authenticator;

use std::ascii;

public struct AbstractAccount has key {
    id: UID,
}

#[authenticator]
public fun authenticate_hello_world(
    _account: &AbstractAccount,
    msg: ascii::String,
    _auth_ctx: &AuthContext,
    _ctx: &TxContext,
) {
    assert!(msg == ascii::string(b"HelloWorld"), 0);
}

#[test_only]
use iota::test_scenario;
#[test_only]
use iota::test_utils;

#[test]
fun profile_authenticate_hello_world() {
    let mut scenario_val = test_scenario::begin(@0x0);
    let scenario = &mut scenario_val;

    let account = AbstractAccount { id: object::new(scenario.ctx()) };

    // Auth digest must be 32 bytes; contents are unused by this authenticator.
    let auth_ctx = auth_context::new_with_tx_inputs(
        b"00000000000000000000000000000000",
        vector::empty(),
        vector::empty(),
        vector[],
    );

    authenticate_hello_world(
        &account,
        ascii::string(b"HelloWorld"),
        &auth_ctx,
        scenario.ctx(),
    );

    test_utils::destroy(account);
    test_scenario::end(scenario_val);
}
