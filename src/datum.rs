//! Plutus datum parsing utilities.
//!
//! Converts between Plutus JSON format (real Blockfrost) and CBOR hex strings
//! (yaci-devkit). Provides helpers for extracting typed fields from Plutus JSON.

use ciborium::value::Value as CborValue;
use serde_json::Value;

use crate::CardanoError;

/// Extract an integer from a Plutus JSON field like `{"int": 42}`.
pub fn plutus_int(val: &Value) -> Result<i64, CardanoError> {
    val.get("int")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| CardanoError::Parse(format!("expected {{\"int\": ...}}, got {}", val)))
}

/// Extract a hex byte string from a Plutus JSON field like `{"bytes": "abcd"}`.
pub fn plutus_bytes(val: &Value) -> Result<String, CardanoError> {
    val.get("bytes")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| CardanoError::Parse(format!("expected {{\"bytes\": ...}}, got {}", val)))
}

/// Convert a CBOR hex string (as returned by yaci-devkit) into Plutus JSON format.
///
/// Plutus CBOR encoding:
/// - Tag 121..127 → Constructor 0..6 with fields
/// - Tag 102 → Constructor N > 6 with `[N, fields]`
/// - Integer → `{"int": N}`
/// - Bytes → `{"bytes": "hex"}`
/// - Array → `{"list": [...]}`
pub fn cbor_hex_to_plutus_json(hex_str: &str) -> Result<Value, CardanoError> {
    let bytes = hex::decode(hex_str)
        .map_err(|e| CardanoError::Parse(format!("invalid hex in inline_datum: {}", e)))?;
    let cbor: CborValue = ciborium::de::from_reader(&bytes[..])
        .map_err(|e| CardanoError::Parse(format!("CBOR decode failed: {}", e)))?;
    cbor_to_plutus_json(&cbor)
}

fn cbor_to_plutus_json(val: &CborValue) -> Result<Value, CardanoError> {
    match val {
        CborValue::Integer(n) => {
            let n: i128 = (*n).into();
            Ok(serde_json::json!({"int": n as i64}))
        }
        CborValue::Bytes(b) => {
            Ok(serde_json::json!({"bytes": hex::encode(b)}))
        }
        CborValue::Array(items) => {
            let converted: Result<Vec<Value>, CardanoError> =
                items.iter().map(cbor_to_plutus_json).collect();
            Ok(serde_json::json!({"list": converted?}))
        }
        CborValue::Tag(tag, inner) => {
            // Plutus constructor encoding: tags 121-127 = constructors 0-6
            let constructor = if (121..=127).contains(tag) {
                *tag - 121
            } else if *tag == 102 {
                if let CborValue::Array(arr) = inner.as_ref() {
                    if let Some(CborValue::Integer(n)) = arr.first() {
                        let n: i128 = (*n).into();
                        let fields: Result<Vec<Value>, CardanoError> =
                            arr[1..].iter().flat_map(|item| {
                                if let CborValue::Array(inner_fields) = item {
                                    inner_fields.iter().map(cbor_to_plutus_json).collect()
                                } else {
                                    vec![cbor_to_plutus_json(item)]
                                }
                            }).collect();
                        return Ok(serde_json::json!({
                            "constructor": n as i64,
                            "fields": fields?
                        }));
                    }
                }
                return Err(CardanoError::Parse(format!("invalid Tag 102 encoding: {:?}", inner)));
            } else {
                return Err(CardanoError::Parse(format!("unexpected CBOR tag: {}", tag)));
            };

            let fields = match inner.as_ref() {
                CborValue::Array(items) => {
                    let converted: Result<Vec<Value>, CardanoError> =
                        items.iter().map(cbor_to_plutus_json).collect();
                    converted?
                }
                _ => return Err(CardanoError::Parse(
                    format!("expected array inside constructor tag, got {:?}", inner),
                )),
            };

            Ok(serde_json::json!({
                "constructor": constructor,
                "fields": fields
            }))
        }
        CborValue::Text(s) => {
            Ok(serde_json::json!({"bytes": hex::encode(s.as_bytes())}))
        }
        other => Err(CardanoError::Parse(format!("unsupported CBOR value type: {:?}", other))),
    }
}

// ─── Rust → Plutus JSON serialization (for building transactions) ───

use crate::types::{Action, Invoice, Offramp, State};

/// Serialize a State to Plutus JSON (for inline datum on tx output).
pub fn state_to_plutus_json(state: &State) -> Value {
    let invoices: Vec<Value> = state.invoices.iter().map(invoice_to_plutus_json).collect();
    let offramps: Vec<Value> = state.offramps.iter().map(offramp_to_plutus_json).collect();
    serde_json::json!({
        "constructor": 0,
        "fields": [
            {"int": state.total_liquidity},
            {"int": state.reserved},
            {"int": state.last_invoice_id},
            {"list": invoices},
            {"int": state.last_offramp_id},
            {"list": offramps}
        ]
    })
}

/// Serialize an Invoice to Plutus JSON.
pub fn invoice_to_plutus_json(invoice: &Invoice) -> Value {
    serde_json::json!({
        "constructor": 0,
        "fields": [
            {"int": invoice.invoice_id},
            {"int": invoice.amount},
            {"bytes": invoice.owner},
            {"int": invoice.timestamp},
            {"int": invoice.expires_at}
        ]
    })
}

/// Serialize an Offramp to Plutus JSON.
pub fn offramp_to_plutus_json(offramp: &Offramp) -> Value {
    serde_json::json!({
        "constructor": 0,
        "fields": [
            {"int": offramp.offramp_id},
            {"int": offramp.amount},
            {"bytes": offramp.payment_hash},
            {"bytes": offramp.refund_address},
            {"int": offramp.expires_at}
        ]
    })
}

/// Serialize an Action (redeemer) to Plutus JSON.
pub fn action_to_plutus_json(action: &Action) -> Value {
    match action {
        Action::Deposit { amount } => serde_json::json!({
            "constructor": 0,
            "fields": [{"int": *amount}]
        }),
        Action::Withdraw { amount } => serde_json::json!({
            "constructor": 1,
            "fields": [{"int": *amount}]
        }),
        Action::CreateInvoice { amount, owner, timestamp, expires_at } => serde_json::json!({
            "constructor": 2,
            "fields": [
                {"int": *amount},
                {"bytes": owner},
                {"int": *timestamp},
                {"int": *expires_at}
            ]
        }),
        Action::FulfillInvoice { invoice } => serde_json::json!({
            "constructor": 3,
            "fields": [invoice_to_plutus_json(invoice)]
        }),
        Action::CancelInvoice { invoice_id } => serde_json::json!({
            "constructor": 4,
            "fields": [{"int": *invoice_id}]
        }),
        Action::CreateOfframp { amount, payment_hash, refund_address, expires_at } => serde_json::json!({
            "constructor": 5,
            "fields": [
                {"int": *amount},
                {"bytes": payment_hash},
                {"bytes": refund_address},
                {"int": *expires_at}
            ]
        }),
        Action::FulfillOfframp { offramp } => serde_json::json!({
            "constructor": 6,
            "fields": [offramp_to_plutus_json(offramp)]
        }),
        Action::CancelOfframp { offramp_id } => serde_json::json!({
            "constructor": 7,
            "fields": [{"int": *offramp_id}]
        }),
    }
}

/// Convert Plutus JSON to CBOR hex (for whisky WData).
pub fn plutus_json_to_cbor_hex(val: &Value) -> Result<String, CardanoError> {
    let cbor = plutus_json_to_cbor(val)?;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&cbor, &mut buf)
        .map_err(|e| CardanoError::Parse(format!("CBOR encode failed: {}", e)))?;
    Ok(hex::encode(buf))
}

fn plutus_json_to_cbor(val: &Value) -> Result<CborValue, CardanoError> {
    if let Some(n) = val.get("int").and_then(|v| v.as_i64()) {
        return Ok(CborValue::Integer(n.into()));
    }
    if let Some(b) = val.get("bytes").and_then(|v| v.as_str()) {
        let bytes = hex::decode(b)
            .map_err(|e| CardanoError::Parse(format!("invalid hex in bytes field: {}", e)))?;
        return Ok(CborValue::Bytes(bytes));
    }
    if let Some(items) = val.get("list").and_then(|v| v.as_array()) {
        let converted: Result<Vec<CborValue>, CardanoError> =
            items.iter().map(plutus_json_to_cbor).collect();
        return Ok(CborValue::Array(converted?));
    }
    if let Some(constructor) = val.get("constructor").and_then(|v| v.as_u64()) {
        let fields = val
            .get("fields")
            .and_then(|f| f.as_array())
            .ok_or_else(|| CardanoError::Parse("constructor without fields".into()))?;
        let cbor_fields: Result<Vec<CborValue>, CardanoError> =
            fields.iter().map(plutus_json_to_cbor).collect();
        let tag = if constructor <= 6 {
            121 + constructor
        } else {
            // Use tag 102 with [constructor, fields] for constructor > 6
            return Ok(CborValue::Tag(
                102,
                Box::new(CborValue::Array(vec![
                    CborValue::Integer((constructor as i64).into()),
                    CborValue::Array(cbor_fields?),
                ])),
            ));
        };
        return Ok(CborValue::Tag(tag, Box::new(CborValue::Array(cbor_fields?))));
    }
    Err(CardanoError::Parse(format!("unsupported Plutus JSON value: {}", val)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbor_hex_empty_state() {
        // CBORTag(121, [1000000, 0, 0, []])
        let json = cbor_hex_to_plutus_json("d8799f1a000f424000009fffff").unwrap();
        assert_eq!(json["constructor"], 0);
        let fields = json["fields"].as_array().unwrap();
        assert_eq!(fields[0]["int"], 1_000_000);
        assert_eq!(fields[1]["int"], 0);
        assert_eq!(fields[2]["int"], 0);
        assert!(fields[3]["list"].as_array().unwrap().is_empty());
    }

    #[test]
    fn state_roundtrip_plutus_json() {
        let state = State {
            total_liquidity: 5_000_000,
            reserved: 100_000,
            last_invoice_id: 1,
            invoices: vec![Invoice {
                invoice_id: 1,
                amount: 100_000,
                owner: "abcdef01".into(),
                timestamp: 1700000000000,
                expires_at: 1700003600000,
            }],
            last_offramp_id: 0,
            offramps: vec![],
        };
        let json = state_to_plutus_json(&state);
        let parsed = State::try_from(&json).unwrap();
        assert_eq!(parsed.total_liquidity, state.total_liquidity);
        assert_eq!(parsed.reserved, state.reserved);
        assert_eq!(parsed.last_invoice_id, state.last_invoice_id);
        assert_eq!(parsed.invoices.len(), 1);
        assert_eq!(parsed.invoices[0].owner, state.invoices[0].owner);
        assert_eq!(parsed.last_offramp_id, 0);
        assert!(parsed.offramps.is_empty());
    }

    #[test]
    fn action_deposit_json() {
        let action = Action::Deposit { amount: 500_000 };
        let json = action_to_plutus_json(&action);
        assert_eq!(json["constructor"], 0);
        assert_eq!(json["fields"][0]["int"], 500_000);
    }

    #[test]
    fn plutus_json_cbor_roundtrip() {
        let state = State {
            total_liquidity: 1_000_000,
            reserved: 0,
            last_invoice_id: 0,
            invoices: vec![],
            last_offramp_id: 0,
            offramps: vec![],
        };
        let json = state_to_plutus_json(&state);
        let cbor_hex = plutus_json_to_cbor_hex(&json).unwrap();
        let roundtripped = cbor_hex_to_plutus_json(&cbor_hex).unwrap();
        let parsed = State::try_from(&roundtripped).unwrap();
        assert_eq!(parsed.total_liquidity, 1_000_000);
        assert_eq!(parsed.reserved, 0);
        assert!(parsed.invoices.is_empty());
        assert_eq!(parsed.last_offramp_id, 0);
        assert!(parsed.offramps.is_empty());
    }
}
