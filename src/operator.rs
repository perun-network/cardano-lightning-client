//! OperatorAgent — extends CardanoAgent with transaction building capabilities.
//!
//! Composes the read-only `CardanoAgent` with an operator signing key and
//! the LM script CBOR for building and submitting contract transactions.

use crate::agent::CardanoAgent;
use crate::contract::{self, TxContext};
use crate::error::CardanoError;
use crate::types::{Invoice, Offramp, State};
use whisky::{Asset, Network, TxBuilder, UTxO, UtxoInput, UtxoOutput};

/// Configuration for the operator agent.
#[derive(Debug, Clone)]
pub struct OperatorConfig {
    /// Operator's signing key (hex, ed25519 or CBOR envelope).
    pub skey_hex: String,
    /// Operator's Cardano address (bech32).
    pub operator_address: String,
    /// Operator's payment key hash (hex).
    pub operator_pkh: String,
    /// Applied Plutus script CBOR hex.
    pub script_cbor: String,
    /// cBTC policy ID (hex).
    pub cbtc_policy: String,
    /// cBTC asset name (hex).
    pub cbtc_name: String,
}

/// Operator agent: CardanoAgent + operator key + script CBOR for tx building.
pub struct OperatorAgent {
    agent: CardanoAgent,
    config: OperatorConfig,
    network: Network,
}

/// Raw UTxO data from Blockfrost, needed for tx building.
pub struct ScriptUtxoInfo {
    pub tx_hash: String,
    pub tx_index: u32,
    pub lovelace: u64,
    pub cbtc_amount: i64,
    pub state: State,
}

impl OperatorAgent {
    pub fn new(agent: CardanoAgent, config: OperatorConfig) -> Self {
        Self {
            agent,
            config,
            network: Network::Mainnet, // overwritten by init()
        }
    }

    /// Initialize network cost models from the chain. Call once after construction.
    pub async fn init(&mut self) -> Result<(), CardanoError> {
        let cost_models = self.agent.fetch_cost_models().await?;
        self.network = Network::Custom(cost_models);
        Ok(())
    }

    pub fn agent(&self) -> &CardanoAgent {
        &self.agent
    }

    pub fn config(&self) -> &OperatorConfig {
        &self.config
    }

    /// Query the script UTxO with full raw data needed for tx building.
    pub async fn query_script_utxo(&self) -> Result<ScriptUtxoInfo, CardanoError> {
        let base = self.agent.config().blockfrost_url.trim_end_matches('/');
        let url = format!(
            "{}/addresses/{}/utxos",
            base,
            self.agent.config().script_address
        );

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("project_id", &self.agent.config().blockfrost_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(CardanoError::NotFound(format!(
                "Blockfrost API error {}: {}",
                status, body,
            )));
        }

        let utxos: Vec<serde_json::Value> = resp.json().await?;

        for utxo in &utxos {
            // Find the UTxO with an inline datum (the contract state UTxO)
            let inline_datum = utxo.get("inline_datum");
            if inline_datum.is_none() || inline_datum.unwrap().is_null() {
                continue;
            }

            let tx_hash = utxo
                .get("tx_hash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| CardanoError::Parse("missing tx_hash".into()))?
                .to_string();

            let tx_index = utxo
                .get("output_index")
                .or_else(|| utxo.get("tx_index"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| CardanoError::Parse("missing output_index".into()))?
                as u32;

            // Parse amounts
            let (lovelace, cbtc_amount) = parse_utxo_amounts(utxo, &self.config)?;

            // Parse state from datum
            let state = self.agent.query_state().await?;

            return Ok(ScriptUtxoInfo {
                tx_hash,
                tx_index,
                lovelace,
                cbtc_amount,
                state,
            });
        }

        Err(CardanoError::NotFound(
            "no script UTxO with inline datum found".into(),
        ))
    }

    /// Fetch operator wallet UTxOs for fee payment.
    pub async fn query_wallet_utxos(&self) -> Result<Vec<UTxO>, CardanoError> {
        let base = self.agent.config().blockfrost_url.trim_end_matches('/');
        let url = format!(
            "{}/addresses/{}/utxos",
            base,
            self.config.operator_address
        );

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("project_id", &self.agent.config().blockfrost_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(vec![]);
        }

        let raw_utxos: Vec<serde_json::Value> = resp.json().await?;
        let mut utxos = Vec::new();

        for raw in &raw_utxos {
            let tx_hash = raw.get("tx_hash").and_then(|v| v.as_str()).unwrap_or("");
            let tx_index = raw
                .get("output_index")
                .or_else(|| raw.get("tx_index"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let mut assets = Vec::new();
            if let Some(amount_arr) = raw.get("amount").and_then(|v| v.as_array()) {
                for a in amount_arr {
                    let unit = a.get("unit").and_then(|v| v.as_str()).unwrap_or("lovelace");
                    let qty = a.get("quantity").and_then(|v| v.as_str()).unwrap_or("0");
                    assets.push(Asset::new_from_str(unit, qty));
                }
            }

            utxos.push(UTxO {
                input: UtxoInput {
                    tx_hash: tx_hash.to_string(),
                    output_index: tx_index as u32,
                },
                output: UtxoOutput {
                    address: self.config.operator_address.clone(),
                    amount: assets,
                    data_hash: None,
                    plutus_data: None,
                    script_ref: None,
                    script_hash: None,
                },
            });
        }

        Ok(utxos)
    }

    /// Verify that a TX sent the exact cBTC amount to the operator address.
    ///
    /// Queries Blockfrost `GET /txs/{tx_hash}/utxos` and checks that at least one
    /// output to `operator_address` carries exactly `expected_amount` cBTC.
    pub async fn verify_cbtc_received(
        &self, tx_hash: &str, expected_amount: i64,
    ) -> Result<bool, CardanoError> {
        let base = self.agent.config().blockfrost_url.trim_end_matches('/');
        let url = format!("{}/txs/{}/utxos", base, tx_hash);

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("project_id", &self.agent.config().blockfrost_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(CardanoError::NotFound(format!(
                "Blockfrost txs/{}/utxos error {}: {}",
                tx_hash, status, body,
            )));
        }

        let data: serde_json::Value = resp.json().await?;
        let cbtc_unit = format!("{}{}", self.config.cbtc_policy, self.config.cbtc_name);

        // Check outputs for cBTC sent to operator address
        if let Some(outputs) = data.get("outputs").and_then(|v| v.as_array()) {
            for output in outputs {
                let addr = output.get("address").and_then(|v| v.as_str()).unwrap_or("");
                if addr != self.config.operator_address {
                    continue;
                }
                if let Some(amounts) = output.get("amount").and_then(|v| v.as_array()) {
                    for a in amounts {
                        let unit = a.get("unit").and_then(|v| v.as_str()).unwrap_or("");
                        let qty: i64 = a
                            .get("quantity")
                            .and_then(|v| v.as_str())
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(0);
                        if unit == cbtc_unit && qty == expected_amount {
                            return Ok(true);
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    /// Build + sign a Deposit transaction. Returns signed tx hex.
    pub async fn deposit(&self, amount: i64) -> Result<String, CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_deposit_tx(&ctx, &script_utxo.state, amount)
    }

    /// Build + sign a Withdraw transaction. Returns signed tx hex.
    pub async fn withdraw(&self, amount: i64) -> Result<String, CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_withdraw_tx(&ctx, &script_utxo.state, amount)
    }

    /// Build + sign a CreateInvoice transaction. Returns (invoice_id, signed_tx_hex).
    pub async fn create_invoice(
        &self, amount: i64, owner_pkh: &str, timestamp: i64, expires_at: i64,
    ) -> Result<(i64, String), CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;

        let new_invoice_id = script_utxo.state.last_invoice_id + 1;
        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        let tx = contract::build_create_invoice_tx(
            &ctx,
            &script_utxo.state,
            amount,
            owner_pkh,
            timestamp,
            expires_at,
        )?;

        Ok((new_invoice_id, tx))
    }

    /// Build + sign a FulfillInvoice transaction. Returns signed tx hex.
    pub async fn fulfill_invoice(
        &self, invoice: &Invoice, owner_address: &str,
    ) -> Result<String, CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_fulfill_invoice_tx(&ctx, &script_utxo.state, invoice, owner_address)
    }

    /// Build + sign a CancelInvoice transaction. Returns signed tx hex.
    pub async fn cancel_invoice(&self, invoice_id: i64) -> Result<String, CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;
        let current_slot = self.agent.query_current_slot().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_cancel_invoice_tx(&ctx, &script_utxo.state, invoice_id, current_slot)
    }

    /// Build + sign a CreateOfframp transaction. Returns (offramp_id, signed_tx_hex).
    pub async fn create_offramp(
        &self, amount: i64, payment_hash: &str, refund_address: &str, expires_at: i64,
    ) -> Result<(i64, String), CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_create_offramp_tx(
            &ctx,
            &script_utxo.state,
            amount,
            payment_hash,
            refund_address,
            expires_at,
        )
    }

    /// Build + sign a FulfillOfframp transaction. Returns signed tx hex.
    pub async fn fulfill_offramp(&self, offramp: &Offramp) -> Result<String, CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_fulfill_offramp_tx(&ctx, &script_utxo.state, offramp)
    }

    /// Build + sign a CancelOfframp transaction. Returns signed tx hex.
    pub async fn cancel_offramp(&self, offramp_id: i64) -> Result<String, CardanoError> {
        let script_utxo = self.query_script_utxo().await?;
        let wallet_utxos = self.query_wallet_utxos().await?;
        let current_slot = self.agent.query_current_slot().await?;

        let ctx = self.make_tx_context(&script_utxo, &wallet_utxos);
        contract::build_cancel_offramp_tx(&ctx, &script_utxo.state, offramp_id, current_slot)
    }

    /// Build + sign a simple cBTC transfer from operator to a target address.
    /// Used for refunding cBTC when an offramp Lightning payment fails.
    pub async fn send_cbtc(
        &self, target_address: &str, amount: i64,
    ) -> Result<String, CardanoError> {
        let wallet_utxos = self.query_wallet_utxos().await?;

        let cbtc_unit = format!("{}{}", self.config.cbtc_policy, self.config.cbtc_name);

        let mut mesh = TxBuilder::new_core();
        mesh.tx_out(target_address, &[
            Asset::new_from_str("lovelace", "2000000"),
            Asset::new_from_str(&cbtc_unit, &amount.to_string()),
        ])
        .change_address(&self.config.operator_address)
        .select_utxos_from(&wallet_utxos, 5_000_000)
        .signing_key(&self.config.skey_hex)
        .complete_sync(None)
        .map_err(|e| CardanoError::Parse(format!("failed to build cBTC send tx: {:?}", e)))?;

        let signed = mesh
            .complete_signing()
            .map_err(|e| CardanoError::Parse(format!("failed to sign cBTC send tx: {:?}", e)))?;

        Ok(signed)
    }

    /// Submit a signed transaction via Blockfrost.
    pub async fn submit_tx(&self, tx_hex: &str) -> Result<String, CardanoError> {
        let base = self.agent.config().blockfrost_url.trim_end_matches('/');
        let url = format!("{}/tx/submit", base);

        let tx_bytes = hex::decode(tx_hex)
            .map_err(|e| CardanoError::Parse(format!("invalid tx hex: {}", e)))?;

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("project_id", &self.agent.config().blockfrost_key)
            .header("Content-Type", "application/cbor")
            .body(tx_bytes)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(CardanoError::Parse(format!(
                "tx submit failed {}: {}",
                status, body
            )));
        }

        let tx_hash: String = resp.json().await?;
        Ok(tx_hash)
    }

    fn make_tx_context<'a>(
        &'a self, script_utxo: &'a ScriptUtxoInfo, wallet_utxos: &'a [UTxO],
    ) -> TxContext<'a> {
        TxContext {
            script_tx_hash: &script_utxo.tx_hash,
            script_tx_index: script_utxo.tx_index,
            script_lovelace: script_utxo.lovelace,
            script_cbtc: script_utxo.cbtc_amount,
            script_address: &self.agent.config().script_address,
            script_cbor: &self.config.script_cbor,
            operator_address: &self.config.operator_address,
            operator_pkh: &self.config.operator_pkh,
            operator_skey: &self.config.skey_hex,
            cbtc_policy: &self.config.cbtc_policy,
            cbtc_name: &self.config.cbtc_name,
            wallet_utxos,
            network: self.network.clone(),
        }
    }
}

/// Parse lovelace and cBTC amounts from a Blockfrost UTxO response.
fn parse_utxo_amounts(
    utxo: &serde_json::Value, config: &OperatorConfig,
) -> Result<(u64, i64), CardanoError> {
    let cbtc_unit = format!("{}{}", config.cbtc_policy, config.cbtc_name);
    let mut lovelace: u64 = 0;
    let mut cbtc: i64 = 0;

    if let Some(amount_arr) = utxo.get("amount").and_then(|v| v.as_array()) {
        for a in amount_arr {
            let unit = a.get("unit").and_then(|v| v.as_str()).unwrap_or("");
            let qty_str = a.get("quantity").and_then(|v| v.as_str()).unwrap_or("0");
            if unit == "lovelace" {
                lovelace = qty_str.parse().unwrap_or(0);
            } else if unit == cbtc_unit {
                cbtc = qty_str.parse().unwrap_or(0);
            }
        }
    }

    Ok((lovelace, cbtc))
}
