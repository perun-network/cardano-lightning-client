//! Cardano Lightning Liquidity Manager contract types.
//!
//! Matches the Aiken `State` and `Invoice` types from the LM contract.

use serde_json::Value;
use std::fmt;

use crate::datum::{plutus_bytes, plutus_int};
use crate::CardanoError;

/// Contract pool state (top-level datum).
#[derive(Debug, Clone)]
pub struct State {
    pub total_liquidity: i64,
    pub reserved: i64,
    pub last_invoice_id: i64,
    pub invoices: Vec<Invoice>,
}

/// A single invoice within the contract state.
#[derive(Debug, Clone)]
pub struct Invoice {
    pub invoice_id: i64,
    pub amount: i64,
    pub owner: String, // hex-encoded payment key hash
    pub timestamp: i64,
    pub expires_at: i64,
}

/// The 5 LM contract redeemer actions. Constructor indices match the Aiken `Action` type.
#[derive(Debug, Clone)]
pub enum Action {
    /// Constructor 0: Deposit { amount: Int }
    Deposit { amount: i64 },
    /// Constructor 1: Withdraw { amount: Int }
    Withdraw { amount: i64 },
    /// Constructor 2: CreateInvoice { amount, owner, timestamp, expires_at }
    CreateInvoice {
        amount: i64,
        owner: String,
        timestamp: i64,
        expires_at: i64,
    },
    /// Constructor 3: FulfillInvoice { invoice: Invoice }
    FulfillInvoice { invoice: Invoice },
    /// Constructor 4: CancelInvoice { invoice_id: Int }
    CancelInvoice { invoice_id: i64 },
}

impl State {
    pub fn available(&self) -> i64 {
        self.total_liquidity - self.reserved
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Pool State:")?;
        writeln!(f, "  total_liquidity: {}", self.total_liquidity)?;
        writeln!(f, "  reserved:        {}", self.reserved)?;
        writeln!(f, "  available:        {}", self.available())?;
        writeln!(f, "  last_invoice_id: {}", self.last_invoice_id)?;
        writeln!(f, "  invoices:        {} active", self.invoices.len())?;
        for inv in &self.invoices {
            writeln!(f, "    - {}", inv)?;
        }
        Ok(())
    }
}

impl fmt::Display for Invoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Invoice #{}: amount={}, owner={}, ts={}, expires={}",
            self.invoice_id, self.amount, self.owner, self.timestamp, self.expires_at,
        )
    }
}

impl TryFrom<&Value> for Invoice {
    type Error = CardanoError;

    /// Parse from Plutus JSON:
    /// ```json
    /// {"constructor": 0, "fields": [
    ///   {"int": invoice_id}, {"int": amount}, {"bytes": "owner_pkh"},
    ///   {"int": timestamp}, {"int": expires_at}
    /// ]}
    /// ```
    fn try_from(val: &Value) -> Result<Self, CardanoError> {
        let fields = val
            .get("fields")
            .and_then(|f| f.as_array())
            .ok_or_else(|| CardanoError::Parse("Invoice: missing 'fields' array".into()))?;

        if fields.len() < 5 {
            return Err(CardanoError::Parse(
                format!("Invoice: expected 5 fields, got {}", fields.len()),
            ));
        }

        Ok(Invoice {
            invoice_id: plutus_int(&fields[0])?,
            amount: plutus_int(&fields[1])?,
            owner: plutus_bytes(&fields[2])?,
            timestamp: plutus_int(&fields[3])?,
            expires_at: plutus_int(&fields[4])?,
        })
    }
}

impl TryFrom<&Value> for State {
    type Error = CardanoError;

    /// Parse from Plutus JSON:
    /// ```json
    /// {"constructor": 0, "fields": [
    ///   {"int": total_liquidity}, {"int": reserved},
    ///   {"int": last_invoice_id}, {"list": [...invoices...]}
    /// ]}
    /// ```
    fn try_from(val: &Value) -> Result<Self, CardanoError> {
        let fields = val
            .get("fields")
            .and_then(|f| f.as_array())
            .ok_or_else(|| CardanoError::Parse("State: missing 'fields' array".into()))?;

        if fields.len() < 4 {
            return Err(CardanoError::Parse(
                format!("State: expected 4 fields, got {}", fields.len()),
            ));
        }

        let invoices_list = fields[3]
            .get("list")
            .and_then(|l| l.as_array())
            .ok_or_else(|| {
                CardanoError::Parse("State: field 3 should be a list of invoices".into())
            })?;

        let invoices: Result<Vec<Invoice>, CardanoError> =
            invoices_list.iter().map(Invoice::try_from).collect();

        Ok(State {
            total_liquidity: plutus_int(&fields[0])?,
            reserved: plutus_int(&fields[1])?,
            last_invoice_id: plutus_int(&fields[2])?,
            invoices: invoices?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_empty_state() {
        let datum = json!({
            "constructor": 0,
            "fields": [
                {"int": 1_000_000},
                {"int": 0},
                {"int": 0},
                {"list": []}
            ]
        });
        let state = State::try_from(&datum).unwrap();
        assert_eq!(state.total_liquidity, 1_000_000);
        assert_eq!(state.reserved, 0);
        assert_eq!(state.last_invoice_id, 0);
        assert_eq!(state.available(), 1_000_000);
        assert!(state.invoices.is_empty());
    }

    #[test]
    fn parse_state_with_invoice() {
        let datum = json!({
            "constructor": 0,
            "fields": [
                {"int": 5_000_000},
                {"int": 100_000},
                {"int": 1},
                {"list": [{
                    "constructor": 0,
                    "fields": [
                        {"int": 1},
                        {"int": 100_000},
                        {"bytes": "abcdef0123456789"},
                        {"int": 1700000000000_i64},
                        {"int": 1700003600000_i64}
                    ]
                }]}
            ]
        });
        let state = State::try_from(&datum).unwrap();
        assert_eq!(state.total_liquidity, 5_000_000);
        assert_eq!(state.reserved, 100_000);
        assert_eq!(state.available(), 4_900_000);
        assert_eq!(state.invoices.len(), 1);
        assert_eq!(state.invoices[0].owner, "abcdef0123456789");
    }
}
