// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! The gRPC store bridges objects via BCS between `iota_sdk_types::Object` and
//! `iota_types::object::Object`, relying on the two having an identical layout.
//! Guard that assumption offline so a future drift fails here, not at runtime.

use iota_sdk_types::{Address, ObjectId, Owner};
use iota_types::{
    base_types::SequenceNumber,
    digests::TransactionDigest,
    object::{MoveObject, MoveObjectExt, Object},
};

#[test]
fn object_bcs_layout_matches_sdk_types() {
    let obj = Object::new_move(
        MoveObject::new_gas_coin(SequenceNumber::from(7), ObjectId::random(), 1_000),
        Owner::Address(Address::ZERO),
        TransactionDigest::ZERO,
    );

    // iota_types::Object -> BCS -> iota_sdk_types::Object -> BCS -> back.
    let bytes = bcs::to_bytes(&obj).expect("encode iota_types::Object");
    let sdk: iota_sdk_types::Object = bcs::from_bytes(&bytes).expect("decode as sdk Object");
    let round_tripped: Object =
        bcs::from_bytes(&bcs::to_bytes(&sdk).expect("re-encode sdk Object"))
            .expect("decode back to iota_types::Object");

    assert_eq!(obj, round_tripped, "BCS layout drift between Object types");
}
