//! Throwaway test binary for Phase 0: validate whisky SDK on devnet.
//!
//! Builds a Deposit transaction against a local yaci-devkit node, signs it,
//! submits it, and verifies the state update.
//!
//! Usage:
//!   SCRIPT_ADDRESS=addr_test1... OPERATOR_SKEY=... cargo run --bin test-whisky

use cardano_lightning_client::{
    CardanoAgent, CardanoConfig, CardanoError, OperatorAgent, OperatorConfig,
};

#[tokio::main]
async fn main() -> Result<(), CardanoError> {
    println!("=== whisky SDK validation ===\n");

    // 1. Read-only agent: query current state
    let config = CardanoConfig::from_env()?;
    let agent = CardanoAgent::new(config);

    let state = agent.query_state().await?;
    println!("Current state:\n{}", state);

    // 2. Operator agent: build + sign a deposit tx
    let skey_hex = std::env::var("OPERATOR_SKEY")
        .map_err(|_| CardanoError::NotFound("OPERATOR_SKEY env var not set".into()))?;
    let operator_address = std::env::var("OPERATOR_ADDRESS")
        .map_err(|_| CardanoError::NotFound("OPERATOR_ADDRESS env var not set".into()))?;
    let operator_pkh = std::env::var("OPERATOR_PKH")
        .map_err(|_| CardanoError::NotFound("OPERATOR_PKH env var not set".into()))?;
    let script_cbor = std::env::var("SCRIPT_CBOR")
        .map_err(|_| CardanoError::NotFound("SCRIPT_CBOR env var not set".into()))?;
    let cbtc_policy = std::env::var("CBTC_POLICY_ID")
        .map_err(|_| CardanoError::NotFound("CBTC_POLICY_ID env var not set".into()))?;
    let cbtc_name = std::env::var("CBTC_ASSET_NAME")
        .map_err(|_| CardanoError::NotFound("CBTC_ASSET_NAME env var not set".into()))?;

    let op_config = OperatorConfig {
        skey_hex,
        operator_address,
        operator_pkh,
        script_cbor,
        cbtc_policy,
        cbtc_name,
    };

    let mut operator = OperatorAgent::new(agent, op_config);
    operator.init().await?;

    // 3. Build a deposit tx
    let deposit_amount: i64 = std::env::var("DEPOSIT_AMOUNT")
        .unwrap_or_else(|_| "100000".into())
        .parse()
        .expect("DEPOSIT_AMOUNT must be an integer");

    println!("Building deposit tx for {} cBTC units...", deposit_amount);
    let signed_tx = operator.deposit(deposit_amount).await?;
    println!("Signed tx hex ({} bytes): {}...", signed_tx.len() / 2, &signed_tx[..64.min(signed_tx.len())]);

    // 4. Submit
    println!("\nSubmitting tx...");
    let tx_hash = operator.submit_tx(&signed_tx).await?;
    println!("Submitted! tx_hash: {}", tx_hash);

    // 5. Wait and verify
    println!("\nWaiting 5s for confirmation...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let new_state = operator.agent().query_state().await?;
    println!("New state:\n{}", new_state);

    println!("\n=== Phase 0 validation complete ===");
    Ok(())
}
