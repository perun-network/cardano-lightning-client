//! CardanoAgent — central struct for all Cardano contract interactions.
//!
//! Holds configuration, a shared HTTP client, and provides methods for
//! querying and (later) mutating the LM contract state.

use serde_json::Value;

use crate::datum::cbor_hex_to_plutus_json;
use crate::error::CardanoError;
use crate::types::State;

/// Configuration for connecting to a Cardano network.
#[derive(Debug, Clone)]
pub struct CardanoConfig {
    /// Blockfrost-compatible API base URL (e.g. `http://localhost:8080/api/v1/`).
    pub blockfrost_url: String,
    /// Blockfrost API key (use `"local"` for yaci-devkit).
    pub blockfrost_key: String,
    /// LM contract script address.
    pub script_address: String,
}

impl CardanoConfig {
    /// Create config from environment variables with sensible defaults.
    pub fn from_env() -> Result<Self, CardanoError> {
        let script_address = std::env::var("SCRIPT_ADDRESS")
            .map_err(|_| CardanoError::NotFound("SCRIPT_ADDRESS env var not set".into()))?;

        Ok(Self {
            blockfrost_url: std::env::var("BLOCKFROST_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080/api/v1/".into()),
            blockfrost_key: std::env::var("BLOCKFROST_PROJECT_ID")
                .unwrap_or_else(|_| "local".into()),
            script_address,
        })
    }
}

/// Agent for interacting with the Lightning Liquidity Manager contract.
///
/// Reuses a single HTTP client across calls. Currently read-only;
/// MS2 will add tx building via whisky SDK.
pub struct CardanoAgent {
    client: reqwest::Client,
    config: CardanoConfig,
}

impl CardanoAgent {
    pub fn new(config: CardanoConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub fn config(&self) -> &CardanoConfig {
        &self.config
    }

    /// Query the current contract state from the Blockfrost-compatible API.
    pub async fn query_state(&self) -> Result<State, CardanoError> {
        let base = self.config.blockfrost_url.trim_end_matches('/');
        let url = format!("{}/addresses/{}/utxos", base, self.config.script_address);

        let resp = self
            .client
            .get(&url)
            .header("project_id", &self.config.blockfrost_key)
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

        let utxos: Vec<Value> = resp.json().await?;

        if utxos.is_empty() {
            return Err(CardanoError::NotFound(
                "no UTxOs found at script address".into(),
            ));
        }

        // Find UTxO with inline datum (the contract state UTxO)
        for utxo in &utxos {
            if let Some(inline_datum) = utxo.get("inline_datum") {
                if inline_datum.is_null() {
                    continue;
                }
                // yaci-devkit returns CBOR hex string; real Blockfrost returns Plutus JSON object
                if let Some(cbor_hex) = inline_datum.as_str() {
                    let plutus_json = cbor_hex_to_plutus_json(cbor_hex)?;
                    return State::try_from(&plutus_json);
                }
                return State::try_from(inline_datum);
            }
        }

        // Fallback: fetch datum by hash
        let base = self.config.blockfrost_url.trim_end_matches('/');
        for utxo in &utxos {
            if let Some(data_hash) = utxo.get("data_hash").and_then(|d| d.as_str()) {
                let datum_url = format!("{}/scripts/datum/{}", base, data_hash);
                let datum_resp = self
                    .client
                    .get(&datum_url)
                    .header("project_id", &self.config.blockfrost_key)
                    .send()
                    .await?;

                if datum_resp.status().is_success() {
                    let wrapper: Value = datum_resp.json().await?;
                    if let Some(json_value) = wrapper.get("json_value") {
                        return State::try_from(json_value);
                    }
                }
            }
        }

        Err(CardanoError::NotFound(
            "no UTxO with datum found at script address".into(),
        ))
    }

    /// Query the current slot number from the latest block.
    pub async fn query_current_slot(&self) -> Result<u64, CardanoError> {
        let base = self.config.blockfrost_url.trim_end_matches('/');
        let url = format!("{}/blocks/latest", base);

        let resp = self
            .client
            .get(&url)
            .header("project_id", &self.config.blockfrost_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(CardanoError::NotFound("failed to fetch latest block".into()));
        }

        let block: Value = resp.json().await?;
        let slot = block
            .get("slot")
            .and_then(|s| s.as_u64())
            .ok_or_else(|| CardanoError::NotFound("no slot in latest block".into()))?;

        Ok(slot)
    }

    /// Fetch cost models from the Blockfrost-compatible API (epoch parameters).
    /// Returns `Vec<Vec<i64>>` with [PlutusV1, PlutusV2, PlutusV3] cost model values.
    pub async fn fetch_cost_models(&self) -> Result<Vec<Vec<i64>>, CardanoError> {
        let base = self.config.blockfrost_url.trim_end_matches('/');
        let url = format!("{}/epochs/latest/parameters", base);

        let resp = self
            .client
            .get(&url)
            .header("project_id", &self.config.blockfrost_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(CardanoError::NotFound("failed to fetch epoch parameters".into()));
        }

        let params: Value = resp.json().await?;
        let cost_models = params
            .get("cost_models")
            .ok_or_else(|| CardanoError::NotFound("no cost_models in epoch parameters".into()))?;

        // Prefer cost_models_raw (pre-ordered arrays) over cost_models (named keys).
        // Hosted Blockfrost returns named keys whose alphabetical order differs from
        // the canonical Plutus cost model order. cost_models_raw avoids this issue.
        let source = params.get("cost_models_raw").unwrap_or(cost_models);

        let mut result = Vec::new();
        for lang in &["PlutusV1", "PlutusV2", "PlutusV3"] {
            if let Some(cm) = source.get(lang) {
                if let Some(arr) = cm.as_array() {
                    // cost_models_raw: values are already a pre-ordered array
                    let values: Vec<i64> = arr.iter().filter_map(|v| v.as_i64()).collect();
                    result.push(values);
                } else if let Some(obj) = cm.as_object() {
                    // cost_models with numeric keys (yaci-devkit local): sort numerically
                    let mut keys: Vec<&String> = obj.keys().collect();
                    keys.sort_by_key(|k| k.parse::<u64>().unwrap_or(u64::MAX));
                    let values: Vec<i64> = keys
                        .iter()
                        .filter_map(|k| obj.get(*k).and_then(|v| v.as_i64()))
                        .collect();
                    result.push(values);
                }
            }
        }

        Ok(result)
    }
}
