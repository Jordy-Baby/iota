// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tiny lexer for `--arg` / `--auth-arg` values.
//!
//! Parses the fixed grammar documented in the CLI README into a
//! [`ParsedValue`] — either a BCS-encoded pure value, or an object ID that
//! the caller looks up in the store.

use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use iota_types::base_types::{IotaAddress, ObjectID};

/// Parsed form of a single `--arg` / `--auth-arg` value.
#[derive(Debug)]
pub(crate) enum ParsedValue {
    /// BCS-encoded pure value ready to hand to the PTB builder.
    Pure(Vec<u8>),
    /// An object reference; the store decides shared-vs-owned at lowering time.
    Object(ObjectID),
}

/// Parse a single `--arg` / `--auth-arg` value into a typed form.
///
/// Supported forms:
/// - `10u8`, `10u16`, `10u32`, `10u64`, `10u128`, `10u256` — typed integer →
///   pure
/// - `true` / `false` → pure bool
/// - `@0xADDR` — pure address
/// - `"text"` (with literal quotes) → pure `vector<u8>` (UTF-8 bytes)
/// - `object(0xID)` → object reference resolved via the store
pub(crate) fn parse_value(s: &str) -> Result<ParsedValue> {
    let s = s.trim();

    if s == "true" {
        return Ok(ParsedValue::Pure(bcs::to_bytes(&true)?));
    }
    if s == "false" {
        return Ok(ParsedValue::Pure(bcs::to_bytes(&false)?));
    }

    if let Some(rest) = s.strip_prefix('@') {
        let addr =
            parse_address(rest.trim()).with_context(|| format!("parsing address literal `{s}`"))?;
        return Ok(ParsedValue::Pure(bcs::to_bytes(&addr)?));
    }

    if let Some(inner) = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        let bytes: Vec<u8> = inner.as_bytes().to_vec();
        return Ok(ParsedValue::Pure(bcs::to_bytes(&bytes)?));
    }

    if let Some(inner) = s.strip_prefix("object(").and_then(|t| t.strip_suffix(')')) {
        let id = ObjectID::from_prefixed_short_hex(inner.trim())
            .with_context(|| format!("parsing object ID in `{s}`"))?;
        return Ok(ParsedValue::Object(id));
    }

    if s.starts_with('[') {
        bail!(
            "vector literals are not supported in v1; use Rust tests for complex PTBs (got `{s}`)"
        );
    }

    // Typed-integer literal: digits followed by a u<N> suffix.
    if let Some((digits, suffix)) = split_int_suffix(s) {
        return lower_typed_int(digits, suffix)
            .with_context(|| format!("parsing integer literal `{s}`"));
    }

    if s.chars().all(|c| c.is_ascii_digit()) {
        bail!("integer literals require a type suffix like 10u64 (got `{s}`)");
    }

    bail!(
        "unrecognised argument literal `{s}` — expected one of: typed int (10u64), bool, @0xADDR, \"text\", object(0xID)"
    )
}

/// Accept either `0x...` hex-literal or a full 64-char padded address.
pub(crate) fn parse_address(s: &str) -> Result<IotaAddress> {
    if s.starts_with("0x") {
        let obj_id = ObjectID::from_prefixed_short_hex(s)
            .map_err(|e| anyhow!("not a valid hex address `{s}`: {e}"))?;
        Ok(IotaAddress::from(obj_id))
    } else {
        IotaAddress::from_str(s).map_err(|e| anyhow!("not a valid address `{s}`: {e}"))
    }
}

fn split_int_suffix(s: &str) -> Option<(&str, &str)> {
    for suf in ["u256", "u128", "u64", "u32", "u16", "u8"] {
        if let Some(digits) = s.strip_suffix(suf) {
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return Some((digits, suf));
            }
        }
    }
    None
}

fn lower_typed_int(digits: &str, suffix: &str) -> Result<ParsedValue> {
    let bytes = match suffix {
        "u8" => bcs::to_bytes(&digits.parse::<u8>()?)?,
        "u16" => bcs::to_bytes(&digits.parse::<u16>()?)?,
        "u32" => bcs::to_bytes(&digits.parse::<u32>()?)?,
        "u64" => bcs::to_bytes(&digits.parse::<u64>()?)?,
        "u128" => bcs::to_bytes(&digits.parse::<u128>()?)?,
        "u256" => {
            // u256 isn't a Rust primitive. Parse as a move-core U256.
            let n = move_core_types::u256::U256::from_str(digits)
                .map_err(|e| anyhow!("u256 parse failure: {e}"))?;
            bcs::to_bytes(&n)?
        }
        _ => bail!("unsupported integer suffix `{suffix}`"),
    };
    Ok(ParsedValue::Pure(bytes))
}
