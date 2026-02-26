//! Transaction building for the 5 LM contract operations via whisky SDK.
//!
//! Each function builds a complete transaction for one LM action:
//! - `build_deposit_tx` — Deposit cBTC into the pool
//! - `build_withdraw_tx` — Withdraw cBTC from the pool
//! - `build_create_invoice_tx` — Reserve cBTC for a Lightning swap
//! - `build_fulfill_invoice_tx` — Fulfill invoice, sending cBTC to owner
//! - `build_cancel_invoice_tx` — Cancel expired invoice, unreserve cBTC

use whisky::*;

use crate::datum::{action_to_plutus_json, plutus_json_to_cbor_hex, state_to_plutus_json};
use crate::error::CardanoError;
use crate::types::{Action, Invoice, Offramp, State};

/// All context needed to build a contract transaction.
pub struct TxContext<'a> {
    /// The script UTxO (tx_hash#index).
    pub script_tx_hash: &'a str,
    pub script_tx_index: u32,
    /// Current assets on the script UTxO (lovelace + cBTC).
    pub script_lovelace: u64,
    pub script_cbtc: i64,
    /// The script address.
    pub script_address: &'a str,
    /// The applied Plutus script CBOR hex.
    pub script_cbor: &'a str,
    /// Operator's Cardano address.
    pub operator_address: &'a str,
    /// Operator's payment key hash (hex, for required_signer_hash).
    pub operator_pkh: &'a str,
    /// Operator's signing key hex.
    pub operator_skey: &'a str,
    /// cBTC policy ID (hex).
    pub cbtc_policy: &'a str,
    /// cBTC asset name (hex).
    pub cbtc_name: &'a str,
    /// Operator wallet UTxOs for fee payment / collateral.
    pub wallet_utxos: &'a [UTxO],
    /// Network (with cost models) for correct script integrity hash.
    pub network: Network,
    /// Protocol parameters for correct fee calculation (None = use default).
    pub protocol_params: Option<Protocol>,
}

impl TxContext<'_> {
    fn cbtc_unit(&self) -> String {
        format!("{}{}", self.cbtc_policy, self.cbtc_name)
    }
}

fn redeemer_for(action: &Action) -> WRedeemer {
    let json = action_to_plutus_json(action);
    let cbor_hex = plutus_json_to_cbor_hex(&json).expect("redeemer serialization cannot fail");
    WRedeemer {
        data: WData::CBOR(cbor_hex),
        ex_units: Budget { mem: 800_000, steps: 400_000_000 },
    }
}

fn datum_cbor(state: &State) -> String {
    let json = state_to_plutus_json(state);
    plutus_json_to_cbor_hex(&json).expect("datum serialization cannot fail")
}

/// Build a Deposit transaction.
///
/// Operator sends `amount` cBTC to the contract, increasing `total_liquidity`.
pub fn build_deposit_tx(
    ctx: &TxContext, current_state: &State, amount: i64,
) -> Result<String, CardanoError> {
    if amount <= 0 {
        return Err(CardanoError::Parse("deposit amount must be positive".into()));
    }

    let new_state = State {
        total_liquidity: current_state.total_liquidity + amount,
        reserved: current_state.reserved,
        last_invoice_id: current_state.last_invoice_id,
        invoices: current_state.invoices.clone(),
        last_offramp_id: current_state.last_offramp_id,
        offramps: current_state.offramps.clone(),
    };

    let action = Action::Deposit { amount };
    let new_cbtc = ctx.script_cbtc + amount;

    build_contract_tx(ctx, current_state, &new_state, &action, new_cbtc, None)
}

/// Build a Withdraw transaction.
///
/// Operator withdraws `amount` cBTC from the pool (must not exceed available).
pub fn build_withdraw_tx(
    ctx: &TxContext, current_state: &State, amount: i64,
) -> Result<String, CardanoError> {
    if amount <= 0 {
        return Err(CardanoError::Parse("withdraw amount must be positive".into()));
    }
    if amount > current_state.available() {
        return Err(CardanoError::Parse(format!(
            "withdraw {} exceeds available {}",
            amount,
            current_state.available()
        )));
    }

    let new_state = State {
        total_liquidity: current_state.total_liquidity - amount,
        reserved: current_state.reserved,
        last_invoice_id: current_state.last_invoice_id,
        invoices: current_state.invoices.clone(),
        last_offramp_id: current_state.last_offramp_id,
        offramps: current_state.offramps.clone(),
    };

    let action = Action::Withdraw { amount };
    let new_cbtc = ctx.script_cbtc - amount;

    build_contract_tx(ctx, current_state, &new_state, &action, new_cbtc, None)
}

/// Build a CreateInvoice transaction.
///
/// Reserves `amount` cBTC for the given `owner_pkh` with expiry.
pub fn build_create_invoice_tx(
    ctx: &TxContext, current_state: &State, amount: i64, owner_pkh: &str,
    timestamp: i64, expires_at: i64,
) -> Result<String, CardanoError> {
    if amount <= 0 {
        return Err(CardanoError::Parse("invoice amount must be positive".into()));
    }
    if amount > current_state.available() {
        return Err(CardanoError::Parse(format!(
            "invoice amount {} exceeds available {}",
            amount,
            current_state.available()
        )));
    }

    let new_invoice_id = current_state.last_invoice_id + 1;
    let new_invoice = Invoice {
        invoice_id: new_invoice_id,
        amount,
        owner: owner_pkh.to_string(),
        timestamp,
        expires_at,
    };

    let mut new_invoices = vec![new_invoice];
    new_invoices.extend(current_state.invoices.clone());

    let new_state = State {
        total_liquidity: current_state.total_liquidity,
        reserved: current_state.reserved + amount,
        last_invoice_id: new_invoice_id,
        invoices: new_invoices,
        last_offramp_id: current_state.last_offramp_id,
        offramps: current_state.offramps.clone(),
    };

    let action = Action::CreateInvoice {
        amount,
        owner: owner_pkh.to_string(),
        timestamp,
        expires_at,
    };

    build_contract_tx(ctx, current_state, &new_state, &action, ctx.script_cbtc, None)
}

/// Build a FulfillInvoice transaction.
///
/// Sends cBTC to the invoice owner, decreasing both reserved and total_liquidity.
pub fn build_fulfill_invoice_tx(
    ctx: &TxContext, current_state: &State, invoice: &Invoice,
    owner_address: &str,
) -> Result<String, CardanoError> {
    let remaining: Vec<Invoice> = current_state
        .invoices
        .iter()
        .filter(|i| i.invoice_id != invoice.invoice_id)
        .cloned()
        .collect();

    let new_state = State {
        total_liquidity: current_state.total_liquidity - invoice.amount,
        reserved: current_state.reserved - invoice.amount,
        last_invoice_id: current_state.last_invoice_id,
        invoices: remaining,
        last_offramp_id: current_state.last_offramp_id,
        offramps: current_state.offramps.clone(),
    };

    let action = Action::FulfillInvoice { invoice: invoice.clone() };
    let new_cbtc = ctx.script_cbtc - invoice.amount;

    // Owner output: send cBTC to the invoice owner
    let owner_output = Some((owner_address, invoice.amount));

    build_contract_tx(ctx, current_state, &new_state, &action, new_cbtc, owner_output)
}

/// Build a CancelInvoice transaction.
///
/// Cancels an expired invoice, unreserving the cBTC.
pub fn build_cancel_invoice_tx(
    ctx: &TxContext, current_state: &State, invoice_id: i64,
) -> Result<String, CardanoError> {
    let invoice = current_state
        .invoices
        .iter()
        .find(|i| i.invoice_id == invoice_id)
        .ok_or_else(|| CardanoError::NotFound(format!("invoice {} not found", invoice_id)))?;

    let remaining: Vec<Invoice> = current_state
        .invoices
        .iter()
        .filter(|i| i.invoice_id != invoice_id)
        .cloned()
        .collect();

    let new_state = State {
        total_liquidity: current_state.total_liquidity,
        reserved: current_state.reserved - invoice.amount,
        last_invoice_id: current_state.last_invoice_id,
        invoices: remaining,
        last_offramp_id: current_state.last_offramp_id,
        offramps: current_state.offramps.clone(),
    };

    let action = Action::CancelInvoice { invoice_id };

    build_contract_tx(ctx, current_state, &new_state, &action, ctx.script_cbtc, None)
}

/// Build a CreateOfframp transaction.
///
/// Registers offramp intent on-chain. No cBTC movement (datum-only change).
pub fn build_create_offramp_tx(
    ctx: &TxContext, current_state: &State, amount: i64, payment_hash: &str,
    refund_address: &str, expires_at: i64,
) -> Result<(i64, String), CardanoError> {
    if amount <= 0 {
        return Err(CardanoError::Parse("offramp amount must be positive".into()));
    }

    let new_offramp_id = current_state.last_offramp_id + 1;
    let new_offramp = Offramp {
        offramp_id: new_offramp_id,
        amount,
        payment_hash: payment_hash.to_string(),
        refund_address: refund_address.to_string(),
        expires_at,
    };

    let mut new_offramps = vec![new_offramp];
    new_offramps.extend(current_state.offramps.clone());

    let new_state = State {
        total_liquidity: current_state.total_liquidity,
        reserved: current_state.reserved,
        last_invoice_id: current_state.last_invoice_id,
        invoices: current_state.invoices.clone(),
        last_offramp_id: new_offramp_id,
        offramps: new_offramps,
    };

    let action = Action::CreateOfframp {
        amount,
        payment_hash: payment_hash.to_string(),
        refund_address: refund_address.to_string(),
        expires_at,
    };

    let tx = build_contract_tx(ctx, current_state, &new_state, &action, ctx.script_cbtc, None)?;
    Ok((new_offramp_id, tx))
}

/// Build a FulfillOfframp transaction.
///
/// Deposits cBTC to pool after Lightning payment success. Removes offramp entry.
pub fn build_fulfill_offramp_tx(
    ctx: &TxContext, current_state: &State, offramp: &Offramp,
) -> Result<String, CardanoError> {
    let remaining: Vec<Offramp> = current_state
        .offramps
        .iter()
        .filter(|o| o.offramp_id != offramp.offramp_id)
        .cloned()
        .collect();

    let new_state = State {
        total_liquidity: current_state.total_liquidity + offramp.amount,
        reserved: current_state.reserved,
        last_invoice_id: current_state.last_invoice_id,
        invoices: current_state.invoices.clone(),
        last_offramp_id: current_state.last_offramp_id,
        offramps: remaining,
    };

    let action = Action::FulfillOfframp { offramp: offramp.clone() };
    let new_cbtc = ctx.script_cbtc + offramp.amount;

    build_contract_tx(ctx, current_state, &new_state, &action, new_cbtc, None)
}

/// Build a CancelOfframp transaction.
///
/// Cancels an expired offramp, removing the entry. No cBTC movement.
pub fn build_cancel_offramp_tx(
    ctx: &TxContext, current_state: &State, offramp_id: i64,
) -> Result<String, CardanoError> {
    let _offramp = current_state
        .offramps
        .iter()
        .find(|o| o.offramp_id == offramp_id)
        .ok_or_else(|| CardanoError::NotFound(format!("offramp {} not found", offramp_id)))?;

    let remaining: Vec<Offramp> = current_state
        .offramps
        .iter()
        .filter(|o| o.offramp_id != offramp_id)
        .cloned()
        .collect();

    let new_state = State {
        total_liquidity: current_state.total_liquidity,
        reserved: current_state.reserved,
        last_invoice_id: current_state.last_invoice_id,
        invoices: current_state.invoices.clone(),
        last_offramp_id: current_state.last_offramp_id,
        offramps: remaining,
    };

    let action = Action::CancelOfframp { offramp_id };

    build_contract_tx(ctx, current_state, &new_state, &action, ctx.script_cbtc, None)
}

/// Core transaction builder for all 5 operations.
///
/// All LM transactions follow the same pattern:
/// 1. Spend the script UTxO (with redeemer)
/// 2. Produce a new script UTxO (with updated datum + cBTC)
/// 3. Optionally send cBTC to an owner address (FulfillInvoice)
/// 4. Operator signs + pays fees
fn build_contract_tx(
    ctx: &TxContext, _current_state: &State, new_state: &State, action: &Action,
    new_script_cbtc: i64, owner_output: Option<(&str, i64)>,
) -> Result<String, CardanoError> {
    let mut tx = TxBuilder::new_core();
    tx.protocol_params = ctx.protocol_params.clone();
    tx.network(ctx.network.clone());

    // 1. Spend the script UTxO
    let script_input_assets = vec![
        Asset::new_from_str("lovelace", &ctx.script_lovelace.to_string()),
        Asset::new_from_str(&ctx.cbtc_unit(), &ctx.script_cbtc.to_string()),
    ];

    tx.spending_plutus_script_v3()
        .tx_in(ctx.script_tx_hash, ctx.script_tx_index, &script_input_assets, ctx.script_address)
        .tx_in_inline_datum_present()
        .tx_in_redeemer_value(&redeemer_for(action))
        .tx_in_script(ctx.script_cbor);

    // 2. Produce the new script UTxO with updated datum
    let new_datum_cbor = datum_cbor(new_state);
    let new_script_assets = vec![
        Asset::new_from_str("lovelace", &ctx.script_lovelace.to_string()),
        Asset::new_from_str(&ctx.cbtc_unit(), &new_script_cbtc.to_string()),
    ];

    tx.tx_out(ctx.script_address, &new_script_assets)
        .tx_out_inline_datum_value(&WData::CBOR(new_datum_cbor));

    // 3. Optional owner output (for FulfillInvoice)
    if let Some((owner_addr, cbtc_amount)) = owner_output {
        let owner_assets = vec![
            Asset::new_from_str("lovelace", "2000000"),
            Asset::new_from_str(&ctx.cbtc_unit(), &cbtc_amount.to_string()),
        ];
        tx.tx_out(owner_addr, &owner_assets);
    }

    // 4. Collateral (required for Plutus script spending)
    // Conway requires ADA-only collateral inputs — no native tokens allowed.
    let collateral_utxo = ctx
        .wallet_utxos
        .iter()
        .find(|u| {
            // Must be ADA-only (no native tokens) and have >= 5 ADA
            let is_ada_only = u.output.amount.iter().all(|a| a.unit() == "lovelace");
            let has_enough = u.output.amount.iter().any(|a| {
                a.unit() == "lovelace"
                    && a.quantity().parse::<u64>().unwrap_or(0) >= 5_000_000
            });
            is_ada_only && has_enough
        })
        .ok_or_else(|| {
            CardanoError::Parse(
                "no ADA-only wallet UTxO with >= 5 ADA for collateral".into(),
            )
        })?;

    let collateral_assets: Vec<Asset> = collateral_utxo
        .output
        .amount
        .iter()
        .map(|a| Asset::new_from_str(&a.unit(), &a.quantity()))
        .collect();

    tx.tx_in_collateral(
        &collateral_utxo.input.tx_hash,
        collateral_utxo.input.output_index,
        &collateral_assets,
        &collateral_utxo.output.address,
    )
    .set_total_collateral("5000000")
    .set_collateral_return_address(ctx.operator_address);

    // 5. Operator signs and pays fees
    tx.required_signer_hash(ctx.operator_pkh)
        .change_address(ctx.operator_address)
        .select_utxos_from(ctx.wallet_utxos, 5_000_000)
        .signing_key(ctx.operator_skey);

    tx.complete_sync(None)
        .map_err(|e| CardanoError::Parse(format!("tx build failed: {:?}", e)))?;

    let signed_tx = tx
        .complete_signing()
        .map_err(|e| CardanoError::Parse(format!("tx signing failed: {:?}", e)))?;

    Ok(signed_tx)
}
