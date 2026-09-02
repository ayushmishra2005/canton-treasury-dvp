use anyhow::{anyhow, Result};

use crate::canton::{CantonLedgerEvidence, CantonLedgerExpectation};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerValue {
    Text(String),
    Party(String),
    ContractId(String),
    Numeric(String),
    Bool(bool),
    Record(Vec<(String, LedgerValue)>),
    List(Vec<LedgerValue>),
    Optional(Option<Box<LedgerValue>>),
    Variant {
        tag: String,
        value: Option<Box<LedgerValue>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedFact {
    pub template: String,
    pub cid: String,
    pub update_id: String,
    pub arguments: Option<LedgerValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExercisedFact {
    pub template: String,
    pub choice: String,
    pub cid: String,
    pub consuming: bool,
    pub update_id: String,
    pub argument: Option<LedgerValue>,
    pub result: Option<LedgerValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedFact {
    pub template: String,
    pub cid: String,
    pub update_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CantonHistory {
    pub created: Vec<CreatedFact>,
    pub exercised: Vec<ExercisedFact>,
    pub archived: Vec<ArchivedFact>,
}

impl CantonHistory {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.exercised.is_empty() && self.archived.is_empty()
    }
}

pub fn parse_canton_history_facts(stdout: &str) -> Result<CantonHistory> {
    let mut history = CantonHistory::default();
    for line in stdout.lines() {
        let line = line.trim();
        let json = line
            .strip_prefix("CANTON_FACT ")
            .or_else(|| line.strip_prefix("CANTON_FACT"));
        let Some(json) = json else {
            continue;
        };
        let json = json.trim();
        if json.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|err| anyhow!("Canton ledger evidence cannot be read: {err}"))?;
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Canton ledger evidence cannot be read: missing fact kind"))?;
        match kind {
            "created" => history.created.push(CreatedFact {
                template: json_string(&value, "template")?,
                cid: json_string(&value, "cid")?,
                update_id: json_string(&value, "updateId")?,
                arguments: value.get("arguments").and_then(parse_value),
            }),
            "exercised" => history.exercised.push(ExercisedFact {
                template: json_string(&value, "template")?,
                choice: json_string(&value, "choice")?,
                cid: json_string(&value, "cid")?,
                consuming: value
                    .get("consuming")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                update_id: json_string(&value, "updateId")?,
                argument: value.get("argument").and_then(parse_value),
                result: value.get("result").and_then(parse_value),
            }),
            "archived" => history.archived.push(ArchivedFact {
                template: json_string(&value, "template")?,
                cid: json_string(&value, "cid")?,
                update_id: json_string(&value, "updateId")?,
            }),
            _ => {
                return Err(anyhow!(
                    "Canton ledger evidence cannot be read: unknown fact kind"
                ))
            }
        }
    }
    Ok(history)
}

pub fn connect_canton_history(
    history: &CantonHistory,
    expected: &CantonLedgerExpectation,
) -> Result<CantonLedgerEvidence> {
    if expected.lock_id.is_empty()
        || expected.mint_holding.is_empty()
        || expected.payout_destination.is_empty()
        || expected.canton_amount.is_empty()
        || expected.treasury_amount.is_empty()
    {
        return Err(anyhow!(
            "Canton completion is missing recorded operation fields"
        ));
    }

    let minted = unique_created(history, "MintedLock", |fact| {
        field_text(fact.arguments.as_ref(), "lockId") == Some(expected.lock_id.as_str())
    })?;
    let minted_args = minted
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing minted lock arguments"))?;
    let mint_holding = require_text(minted_args, "holdingCid", "minted holding")?;
    if mint_holding != expected.mint_holding {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    if history.created.iter().any(|fact| {
        template_ends(fact, "MintedLock")
            && field_text(fact.arguments.as_ref(), "holdingCid")
                == Some(expected.mint_holding.as_str())
            && field_text(fact.arguments.as_ref(), "lockId")
                .is_some_and(|lock| lock != expected.lock_id)
    }) {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    let mint_amount = require_numeric(minted_args, "amount", "minted amount")?;
    require_amount(&mint_amount, &expected.canton_amount, "minted amount")?;
    let buyer = require_text(minted_args, "beneficiary", "buyer")?;
    let cash_registry = require_text(minted_args, "cashRegistry", "cash registry")?;

    let minted_holding = created_by_cid(history, &mint_holding)
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing the minted holding contract"))?;
    if !template_ends(minted_holding, "StablecoinHolding") {
        return Err(anyhow!(
            "Canton ledger evidence is missing the minted stablecoin holding"
        ));
    }
    let holding_args = minted_holding
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing minted holding arguments"))?;
    let holding_owner = require_text(holding_args, "owner", "minted holding owner")?;
    if holding_owner != buyer {
        return Err(anyhow!("minted holding owner does not match the buyer"));
    }
    let holding_amount = require_numeric(holding_args, "amount", "minted holding amount")?;
    require_amount(
        &holding_amount,
        &expected.canton_amount,
        "minted holding amount",
    )?;
    let (payment_instrument, payment_admin) =
        require_instrument(holding_args, "payment instrument")?;
    if payment_admin != cash_registry {
        return Err(anyhow!(
            "payment instrument admin does not match the cash registry"
        ));
    }

    let binding = authorized_binding(
        history,
        &expected.lock_id,
        &mint_holding,
        &minted.update_id,
        &cash_registry,
        &buyer,
    )?;
    let bound_trade = require_text(
        binding.arguments.as_ref().ok_or_else(|| {
            anyhow!("Canton ledger evidence is missing BridgeTradeBinding arguments")
        })?,
        "tradeCid",
        "bound trade",
    )?;

    let allocate = unique_exercised(history, "AllocationFactory_Allocate", |fact| {
        fact.argument
            .as_ref()
            .is_some_and(|argument| value_contains_cid(argument, &mint_holding))
    })?;
    let allocate_args = allocate
        .argument
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing payment allocation arguments"))?;
    let allocate_result = allocate
        .result
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing payment allocation result"))?;
    if !consumed_in(history, &mint_holding, &allocate.update_id) {
        return Err(anyhow!(
            "minted holding was not consumed by its payment allocation"
        ));
    }
    let payment_allocation = require_text(allocate_result, "allocationCid", "payment allocation")?;
    let expected_admin = require_text(allocate_args, "expectedAdmin", "payment allocation admin")?;
    if expected_admin != payment_admin {
        return Err(anyhow!(
            "payment allocation admin does not match the payment instrument"
        ));
    }
    let transfer_leg = field(allocate_args, "transferLeg")
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing payment transfer leg"))?;
    let payment_sender = require_text(transfer_leg, "sender", "payment sender")?;
    let seller = require_text(transfer_leg, "receiver", "seller")?;
    if payment_sender != buyer {
        return Err(anyhow!(
            "payment allocation sender does not match the buyer"
        ));
    }
    let payment_leg_amount = require_numeric(transfer_leg, "amount", "payment allocation amount")?;
    require_amount(
        &payment_leg_amount,
        &expected.canton_amount,
        "payment allocation amount",
    )?;
    let (leg_instrument, leg_admin) =
        require_instrument(transfer_leg, "payment allocation instrument")?;
    if leg_instrument != payment_instrument || leg_admin != payment_admin {
        return Err(anyhow!(
            "payment allocation instrument does not match the minted holding"
        ));
    }
    let payment_locked =
        allocation_locked_holding(history, &payment_allocation, &allocate.update_id)?;
    let locked = created_by_cid(history, &payment_locked)
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing the locked payment holding"))?;
    if locked.update_id != allocate.update_id {
        return Err(anyhow!(
            "locked payment holding was not created by the payment allocation"
        ));
    }
    let locked_args = locked
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing locked payment arguments"))?;
    if require_text(locked_args, "owner", "locked payment owner")? != buyer {
        return Err(anyhow!("locked payment owner does not match the buyer"));
    }
    require_amount(
        &require_numeric(locked_args, "amount", "locked payment amount")?,
        &expected.canton_amount,
        "locked payment amount",
    )?;
    let (locked_instrument, locked_admin) =
        require_instrument(locked_args, "locked payment instrument")?;
    if locked_instrument != payment_instrument || locked_admin != payment_admin {
        return Err(anyhow!(
            "locked payment instrument does not match the minted holding"
        ));
    }

    let settle = unique_exercised(history, "DvpTrade_Settle", |fact| {
        fact.argument.as_ref().is_some_and(|argument| {
            allocation_cid_for_leg(argument, "stablecoin-payment").as_deref()
                == Some(payment_allocation.as_str())
        })
    })
    .map_err(|err| {
        if err.to_string().contains("missing") {
            anyhow!("DvpTrade_Settle is not connected to this operation's payment allocation")
        } else {
            err
        }
    })?;
    let settle_args = settle
        .argument
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing DvpTrade_Settle arguments"))?;
    let settle_result = settle
        .result
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing DvpTrade_Settle result"))?;
    let treasury_allocation =
        allocation_cid_for_leg(settle_args, "treasury-delivery").ok_or_else(|| {
            anyhow!("Canton ledger evidence is missing the treasury allocation on DvpTrade_Settle")
        })?;
    if treasury_allocation == payment_allocation {
        return Err(anyhow!(
            "treasury and payment allocations must be distinct contracts"
        ));
    }
    if settle.cid != bound_trade {
        return Err(anyhow!(
            "DvpTrade_Settle is not the trade bound to this lock"
        ));
    }
    if !consumed_in(history, &payment_allocation, &settle.update_id)
        || !consumed_in(history, &payment_locked, &settle.update_id)
    {
        return Err(anyhow!(
            "payment allocation was not consumed by the connected DvpTrade_Settle"
        ));
    }
    let payment_receivers = receiver_cids(settle_result, "paymentResult");
    let treasury_receivers = receiver_cids(settle_result, "treasuryResult");
    if payment_receivers.is_empty() || treasury_receivers.is_empty() {
        return Err(anyhow!(
            "Canton ledger evidence is missing DvpTrade_Settle receiver holdings"
        ));
    }
    let seller_payment = unique_created(history, "StablecoinHolding", |fact| {
        fact.update_id == settle.update_id
            && fact.arguments.as_ref().is_some_and(|args| {
                field_text(Some(args), "owner") == Some(seller.as_str())
                    && field_numeric(Some(args), "amount")
                        .is_some_and(|amount| amounts_match(&amount, &expected.canton_amount))
            })
    })?;
    let buyer_treasury = unique_created(history, "TreasuryHolding", |fact| {
        fact.update_id == settle.update_id
            && fact.arguments.as_ref().is_some_and(|args| {
                field_text(Some(args), "owner") == Some(buyer.as_str())
                    && field_numeric(Some(args), "amount")
                        .is_some_and(|amount| amounts_match(&amount, &expected.treasury_amount))
            })
    })?;
    if !payment_receivers
        .iter()
        .any(|cid| cid == &seller_payment.cid)
    {
        return Err(anyhow!(
            "seller payment was not an output of the connected DvpTrade_Settle"
        ));
    }
    if !treasury_receivers
        .iter()
        .any(|cid| cid == &buyer_treasury.cid)
    {
        return Err(anyhow!(
            "buyer Treasury was not an output of the connected DvpTrade_Settle"
        ));
    }
    let payment_out_args = seller_payment
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing seller payment arguments"))?;
    let treasury_out_args = buyer_treasury
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing buyer Treasury arguments"))?;
    let payment_out_amount = require_numeric(payment_out_args, "amount", "seller payment amount")?;
    require_amount(
        &payment_out_amount,
        &expected.canton_amount,
        "seller payment amount",
    )?;
    let treasury_amount = require_numeric(treasury_out_args, "amount", "buyer Treasury amount")?;
    require_amount(
        &treasury_amount,
        &expected.treasury_amount,
        "buyer Treasury amount",
    )?;
    let (out_instrument, out_admin) =
        require_instrument(payment_out_args, "seller payment instrument")?;
    if out_instrument != payment_instrument || out_admin != payment_admin {
        return Err(anyhow!(
            "seller payment instrument does not match the minted holding"
        ));
    }
    let (treasury_instrument, treasury_admin) =
        require_instrument(treasury_out_args, "buyer Treasury instrument")?;
    if require_text(treasury_out_args, "owner", "buyer Treasury owner")? != buyer {
        return Err(anyhow!("buyer Treasury owner does not match the buyer"));
    }
    if require_text(payment_out_args, "owner", "seller payment owner")? != seller {
        return Err(anyhow!("seller payment owner does not match the seller"));
    }

    let request = unique_created(history, "RedemptionRequest", |fact| {
        field_text(fact.arguments.as_ref(), "lockId") == Some(expected.lock_id.as_str())
    })?;
    let request_args = request
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing redemption request arguments"))?;
    let request_holding = require_text(request_args, "holdingCid", "redemption holding")?;
    if request_holding != seller_payment.cid {
        return Err(anyhow!(
            "redemption request is not connected to this operation's seller payment"
        ));
    }
    let payout = require_text(request_args, "payoutDestination", "redemption payout")?;
    if payout != expected.payout_destination {
        return Err(anyhow!(
            "Canton payout destination does not match the recorded operation"
        ));
    }
    require_amount(
        &require_numeric(request_args, "amount", "redemption amount")?,
        &expected.canton_amount,
        "redemption amount",
    )?;
    if require_text(request_args, "holder", "redemption holder")? != seller {
        return Err(anyhow!("redemption holder does not match the seller"));
    }
    let (request_instrument, request_admin) =
        require_instrument(request_args, "redemption instrument")?;
    if request_instrument != payment_instrument || request_admin != payment_admin {
        return Err(anyhow!(
            "redemption instrument does not match the payment instrument"
        ));
    }

    let redeem = unique_exercised(history, "Gateway_Redeem", |fact| {
        fact.argument.as_ref().is_some_and(|argument| {
            field_text(Some(argument), "requestCid") == Some(request.cid.as_str())
        })
    })
    .map_err(|err| {
        if err.to_string().contains("missing") {
            anyhow!("Gateway_Redeem is not connected to this operation's redemption request")
        } else {
            err
        }
    })?;
    let redeem_args = redeem
        .argument
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing Gateway_Redeem arguments"))?;
    let minted_lock_cid = require_text(redeem_args, "mintedLockCid", "redeemed minted lock")?;
    if minted_lock_cid != minted.cid {
        return Err(anyhow!(
            "Gateway_Redeem is not connected to this operation's minted lock"
        ));
    }
    let redeem_binding = require_text(redeem_args, "bindingCid", "redeemed binding")?;
    if redeem_binding != binding.cid {
        return Err(anyhow!(
            "Gateway_Redeem is not connected to this operation's trade binding"
        ));
    }
    if !consumed_in(history, &seller_payment.cid, &redeem.update_id)
        || !history.exercised.iter().any(|fact| {
            fact.choice == "Burn"
                && fact.consuming
                && fact.cid == seller_payment.cid
                && fact.update_id == redeem.update_id
        })
    {
        return Err(anyhow!(
            "seller payment burn is not connected to Gateway_Redeem"
        ));
    }
    let redeemed = unique_created(history, "RedeemedLock", |fact| {
        fact.update_id == redeem.update_id
            && field_text(fact.arguments.as_ref(), "lockId") == Some(expected.lock_id.as_str())
    })?;
    let redeemed_args = redeemed
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing RedeemedLock arguments"))?;
    require_amount(
        &require_numeric(redeemed_args, "amount", "redeemed amount")?,
        &expected.canton_amount,
        "redeemed amount",
    )?;
    if require_text(redeemed_args, "holder", "redeemed holder")? != seller {
        return Err(anyhow!("RedeemedLock holder does not match the seller"));
    }

    Ok(CantonLedgerEvidence {
        lock_id: expected.lock_id.clone(),
        mint_holding,
        mint_consumed: true,
        buyer_treasury: buyer_treasury.cid,
        seller_payment: seller_payment.cid,
        seller_burned: true,
        redeemed_lock: redeemed.cid,
        payout_destination: payout,
        payment_amount: payment_out_amount,
        treasury_amount,
        instrument_id: payment_instrument,
        settle_seen: true,
        redeem_seen: true,
        payment_allocation,
        payment_locked,
        allocate_update: allocate.update_id,
        settle_update: settle.update_id,
        redeem_update: redeem.update_id,
        buyer,
        seller,
        payment_admin,
        treasury_instrument,
        treasury_admin,
        trade_cid: bound_trade,
    })
}

fn json_string(value: &serde_json::Value, name: &str) -> Result<String> {
    value
        .get(name)
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| anyhow!("Canton ledger evidence cannot be read: missing {name}"))
}

fn parse_value(value: &serde_json::Value) -> Option<LedgerValue> {
    if value.is_null() {
        return None;
    }
    let object = value.as_object()?;
    if let Some(text) = object.get("text").and_then(|v| v.as_str()) {
        return Some(LedgerValue::Text(text.to_string()));
    }
    if let Some(party) = object.get("party").and_then(|v| v.as_str()) {
        return Some(LedgerValue::Party(party.to_string()));
    }
    if let Some(cid) = object.get("cid").and_then(|v| v.as_str()) {
        return Some(LedgerValue::ContractId(cid.to_string()));
    }
    if let Some(number) = object.get("numeric").and_then(|v| v.as_str()) {
        return Some(LedgerValue::Numeric(number.to_string()));
    }
    if let Some(flag) = object.get("bool").and_then(|v| v.as_bool()) {
        return Some(LedgerValue::Bool(flag));
    }
    if let Some(record) = object.get("record").and_then(|v| v.as_object()) {
        let fields = record
            .iter()
            .filter_map(|(name, inner)| parse_value(inner).map(|value| (name.clone(), value)))
            .collect();
        return Some(LedgerValue::Record(fields));
    }
    if let Some(list) = object.get("list").and_then(|v| v.as_array()) {
        return Some(LedgerValue::List(
            list.iter().filter_map(parse_value).collect(),
        ));
    }
    if object.contains_key("optional") {
        return Some(LedgerValue::Optional(
            object.get("optional").and_then(parse_value).map(Box::new),
        ));
    }
    if let Some(variant) = object.get("variant").and_then(|v| v.as_object()) {
        return Some(LedgerValue::Variant {
            tag: variant
                .get("tag")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            value: variant.get("value").and_then(parse_value).map(Box::new),
        });
    }
    None
}

fn field<'a>(value: &'a LedgerValue, name: &str) -> Option<&'a LedgerValue> {
    match value {
        LedgerValue::Record(fields) => fields
            .iter()
            .find(|(label, _)| label == name)
            .map(|(_, value)| value)
            .or_else(|| fields.iter().find_map(|(_, inner)| field(inner, name))),
        LedgerValue::List(values) => values.iter().find_map(|inner| field(inner, name)),
        LedgerValue::Optional(Some(inner)) => field(inner, name),
        LedgerValue::Variant {
            value: Some(inner), ..
        } => field(inner, name),
        _ => None,
    }
}

fn field_text<'a>(value: Option<&'a LedgerValue>, name: &str) -> Option<&'a str> {
    value.and_then(|value| field(value, name)).and_then(as_text)
}

fn field_numeric(value: Option<&LedgerValue>, name: &str) -> Option<String> {
    value
        .and_then(|value| field(value, name))
        .and_then(as_numeric)
}

fn as_text(value: &LedgerValue) -> Option<&str> {
    match value {
        LedgerValue::Text(text) | LedgerValue::Party(text) | LedgerValue::ContractId(text) => {
            Some(text.as_str())
        }
        LedgerValue::Optional(Some(inner)) => as_text(inner),
        _ => None,
    }
}

fn as_numeric(value: &LedgerValue) -> Option<String> {
    match value {
        LedgerValue::Numeric(number) => Some(number.clone()),
        LedgerValue::Text(text) if looks_numeric(text) => Some(text.clone()),
        LedgerValue::Record(fields) => fields.iter().find_map(|(_, inner)| as_numeric(inner)),
        LedgerValue::Optional(Some(inner)) => as_numeric(inner),
        LedgerValue::Variant {
            value: Some(inner), ..
        } => as_numeric(inner),
        _ => None,
    }
}

fn looks_numeric(raw: &str) -> bool {
    !raw.is_empty()
        && raw.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && raw.bytes().any(|b| b.is_ascii_digit())
}

fn require_text(value: &LedgerValue, name: &str, label: &str) -> Result<String> {
    field_text(Some(value), name)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing {label}"))
}

fn require_numeric(value: &LedgerValue, name: &str, label: &str) -> Result<String> {
    field_numeric(Some(value), name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing {label}"))
}

fn require_instrument(value: &LedgerValue, label: &str) -> Result<(String, String)> {
    let instrument = field(value, "instrumentId").unwrap_or(value);
    let id = field_text(Some(instrument), "id")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing {label} id"))?;
    let admin = field_text(Some(instrument), "admin")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing {label} admin"))?;
    Ok((id.to_string(), admin.to_string()))
}

fn require_amount(actual: &str, expected: &str, label: &str) -> Result<()> {
    if amounts_match(actual, expected) {
        Ok(())
    } else {
        Err(anyhow!("{label} does not match"))
    }
}

fn amounts_match(left: &str, right: &str) -> bool {
    match (decimal_to_base6(left), decimal_to_base6(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn decimal_to_base6(raw: &str) -> Option<u128> {
    let raw = raw.trim();
    let (whole, frac) = match raw.split_once('.') {
        Some((whole, frac)) => (whole, frac.trim_end_matches('0')),
        None => (raw, ""),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) || frac.len() > 6 {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let mut frac = frac.to_string();
    while frac.len() < 6 {
        frac.push('0');
    }
    whole
        .checked_mul(1_000_000)?
        .checked_add(frac.parse().ok()?)
}

fn value_contains_cid(value: &LedgerValue, cid: &str) -> bool {
    match value {
        LedgerValue::ContractId(value) => value == cid,
        LedgerValue::Text(value) => value == cid,
        LedgerValue::Record(fields) => fields
            .iter()
            .any(|(_, inner)| value_contains_cid(inner, cid)),
        LedgerValue::List(values) => values.iter().any(|inner| value_contains_cid(inner, cid)),
        LedgerValue::Optional(Some(inner)) => value_contains_cid(inner, cid),
        LedgerValue::Variant {
            value: Some(inner), ..
        } => value_contains_cid(inner, cid),
        _ => false,
    }
}

fn receiver_cids(value: &LedgerValue, result_field: &str) -> Vec<String> {
    field(value, result_field)
        .and_then(|result| field(result, "receiverHoldingCids"))
        .map(collect_cids)
        .unwrap_or_default()
}

fn collect_cids(value: &LedgerValue) -> Vec<String> {
    match value {
        LedgerValue::ContractId(cid) | LedgerValue::Text(cid) if !cid.is_empty() => {
            vec![cid.clone()]
        }
        LedgerValue::Record(fields) => fields
            .iter()
            .flat_map(|(_, inner)| collect_cids(inner))
            .collect(),
        LedgerValue::List(values) => values.iter().flat_map(collect_cids).collect(),
        LedgerValue::Optional(Some(inner)) => collect_cids(inner),
        LedgerValue::Variant {
            value: Some(inner), ..
        } => collect_cids(inner),
        _ => Vec::new(),
    }
}

fn allocation_cid_for_leg(value: &LedgerValue, transfer_leg_id: &str) -> Option<String> {
    match value {
        LedgerValue::Record(fields) => {
            let leg = field_text(Some(value), "transferLegId");
            let cid = field_text(Some(value), "allocationCid");
            if leg == Some(transfer_leg_id) {
                cid.filter(|value| !value.is_empty())
                    .map(|value| value.to_string())
            } else {
                fields
                    .iter()
                    .find_map(|(_, inner)| allocation_cid_for_leg(inner, transfer_leg_id))
            }
        }
        LedgerValue::List(values) => values
            .iter()
            .find_map(|inner| allocation_cid_for_leg(inner, transfer_leg_id)),
        LedgerValue::Optional(Some(inner)) => allocation_cid_for_leg(inner, transfer_leg_id),
        LedgerValue::Variant {
            value: Some(inner), ..
        } => allocation_cid_for_leg(inner, transfer_leg_id),
        _ => None,
    }
}

fn allocation_locked_holding(
    history: &CantonHistory,
    allocation_cid: &str,
    allocate_update: &str,
) -> Result<String> {
    let allocation = created_by_cid(history, allocation_cid).ok_or_else(|| {
        anyhow!("Canton ledger evidence is missing the payment allocation contract")
    })?;
    if allocation.update_id != allocate_update {
        return Err(anyhow!(
            "payment allocation was not created by AllocationFactory_Allocate"
        ));
    }
    let args = allocation.arguments.as_ref().ok_or_else(|| {
        anyhow!("Canton ledger evidence is missing payment allocation contract arguments")
    })?;
    let locked = field_text(Some(args), "lockedHoldingCid")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            field(args, "holdingCids")
                .map(collect_cids)
                .and_then(|cids| {
                    if cids.len() == 1 {
                        cids.into_iter().next()
                    } else {
                        None
                    }
                })
        })
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing the locked payment holding"))?;
    Ok(locked)
}

fn authorized_binding(
    history: &CantonHistory,
    lock_id: &str,
    mint_holding: &str,
    minted_update: &str,
    cash_registry: &str,
    buyer: &str,
) -> Result<CreatedFact> {
    unique_exercised(history, "Gateway_Mint", |fact| {
        fact.update_id == minted_update
    })
    .map_err(|err| {
        if err.to_string().contains("missing") {
            anyhow!("mint reference was not created by Gateway_Mint")
        } else {
            err
        }
    })?;
    let mint_ref = unique_created(history, "BridgeMintRef", |fact| {
        fact.update_id == minted_update
            && field_text(fact.arguments.as_ref(), "lockId") == Some(lock_id)
    })
    .map_err(|err| {
        if err.to_string().contains("missing") {
            anyhow!("mint reference was not created by Gateway_Mint")
        } else {
            err
        }
    })?;
    let ref_args = mint_ref
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing mint reference arguments"))?;
    if require_text(ref_args, "lockId", "mint reference lock")? != lock_id {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    if require_text(ref_args, "mintHoldingCid", "mint reference holding")? != mint_holding {
        return Err(anyhow!(
            "mint reference does not name the same holding as MintedLock"
        ));
    }
    if require_text(ref_args, "cashRegistry", "mint reference registry")? != cash_registry {
        return Err(anyhow!("mint reference registry does not match MintedLock"));
    }
    if require_text(ref_args, "buyer", "mint reference buyer")? != buyer {
        return Err(anyhow!("mint reference buyer does not match MintedLock"));
    }
    let bind = unique_exercised(history, "BridgeMintRef_Bind", |fact| {
        fact.consuming && fact.cid == mint_ref.cid
    })
    .map_err(|err| {
        if err.to_string().contains("missing") {
            anyhow!("binding was not created by consuming the mint reference")
        } else {
            err
        }
    })?;
    if !consumed_in(history, &mint_ref.cid, &bind.update_id) {
        return Err(anyhow!(
            "binding was not created by consuming the mint reference"
        ));
    }
    let bound_at_bind = unique_created(history, "BridgeTradeBinding", |fact| {
        fact.update_id == bind.update_id
            && field_text(fact.arguments.as_ref(), "lockId") == Some(lock_id)
    })
    .map_err(|err| {
        if err.to_string().contains("missing") {
            anyhow!("binding was not created by consuming the mint reference")
        } else {
            err
        }
    })?;
    let bound_args = bound_at_bind
        .arguments
        .as_ref()
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing BridgeTradeBinding arguments"))?;
    if require_text(bound_args, "mintHoldingCid", "bound mint holding")? != mint_holding {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    if require_text(bound_args, "lockId", "bound lock")? != lock_id {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    binding_for_lock(history, lock_id, mint_holding)
}

fn binding_for_lock(
    history: &CantonHistory,
    lock_id: &str,
    mint_holding: &str,
) -> Result<CreatedFact> {
    let found = history
        .created
        .iter()
        .filter(|fact| {
            template_ends(fact, "BridgeTradeBinding")
                && field_text(fact.arguments.as_ref(), "lockId") == Some(lock_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    if found.is_empty() {
        return Err(anyhow!(
            "Canton ledger evidence is missing BridgeTradeBinding"
        ));
    }
    let mut mint_ids = Vec::new();
    let mut trade_ids = Vec::new();
    for fact in &found {
        if let Some(mint) = field_text(fact.arguments.as_ref(), "mintHoldingCid") {
            if !mint_ids.iter().any(|value: &String| value == mint) {
                mint_ids.push(mint.to_string());
            }
        }
        if let Some(trade) = field_text(fact.arguments.as_ref(), "tradeCid") {
            if !trade_ids.iter().any(|value: &String| value == trade) {
                trade_ids.push(trade.to_string());
            }
        }
    }
    if mint_ids.as_slice() != [mint_holding] || trade_ids.len() != 1 {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    found
        .into_iter()
        .max_by_key(|fact| field_text(fact.arguments.as_ref(), "sellerPaymentCid").is_some())
        .ok_or_else(|| anyhow!("Canton ledger evidence is missing BridgeTradeBinding"))
}

fn created_by_cid<'a>(history: &'a CantonHistory, cid: &str) -> Option<&'a CreatedFact> {
    history
        .created
        .iter()
        .filter(|fact| fact.cid == cid)
        .max_by_key(|fact| fact.arguments.is_some())
}

fn unique_created<F>(history: &CantonHistory, template: &str, pred: F) -> Result<CreatedFact>
where
    F: Fn(&CreatedFact) -> bool,
{
    let mut found = history
        .created
        .iter()
        .filter(|fact| template_ends(fact, template) && pred(fact))
        .cloned()
        .collect::<Vec<_>>();
    found.sort_by_key(|fact| std::cmp::Reverse(fact.arguments.is_some()));
    found.dedup_by(|a, b| a.cid == b.cid && a.update_id == b.update_id);
    match found.as_slice() {
        [fact] => Ok(fact.clone()),
        [] => Err(anyhow!("Canton ledger evidence is missing {template}")),
        _ => Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        )),
    }
}

fn unique_exercised<F>(history: &CantonHistory, choice: &str, pred: F) -> Result<ExercisedFact>
where
    F: Fn(&ExercisedFact) -> bool,
{
    let mut found = history
        .exercised
        .iter()
        .filter(|fact| fact.choice == choice && pred(fact))
        .cloned()
        .collect::<Vec<_>>();
    found.sort_by_key(|fact| std::cmp::Reverse((fact.argument.is_some(), fact.result.is_some())));
    found.dedup_by(|a, b| a.cid == b.cid && a.update_id == b.update_id && a.choice == b.choice);
    match found.as_slice() {
        [fact] => Ok(fact.clone()),
        [] => Err(anyhow!("Canton ledger evidence is missing {choice}")),
        _ => Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        )),
    }
}

fn template_ends(fact: &CreatedFact, suffix: &str) -> bool {
    fact.template == suffix || fact.template.ends_with(suffix)
}

fn consumed_in(history: &CantonHistory, cid: &str, update_id: &str) -> bool {
    history
        .archived
        .iter()
        .any(|fact| fact.cid == cid && fact.update_id == update_id)
        || history
            .exercised
            .iter()
            .any(|fact| fact.consuming && fact.cid == cid && fact.update_id == update_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_a() -> CantonLedgerExpectation {
        expected("lock-a", "holding-mint-a", "dest-a")
    }

    fn expected_b() -> CantonLedgerExpectation {
        expected("lock-b", "holding-mint-b", "dest-b")
    }

    fn expected(lock: &str, holding: &str, dest: &str) -> CantonLedgerExpectation {
        CantonLedgerExpectation {
            lock_id: lock.to_string(),
            canton_amount: "100000.000000".to_string(),
            treasury_amount: "100.000000".to_string(),
            payout_destination: dest.to_string(),
            mint_holding: holding.to_string(),
        }
    }

    fn rec(fields: &[(&str, LedgerValue)]) -> LedgerValue {
        LedgerValue::Record(
            fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        )
    }

    fn text(value: &str) -> LedgerValue {
        LedgerValue::Text(value.to_string())
    }

    fn party(value: &str) -> LedgerValue {
        LedgerValue::Party(value.to_string())
    }

    fn cid(value: &str) -> LedgerValue {
        LedgerValue::ContractId(value.to_string())
    }

    fn num(value: &str) -> LedgerValue {
        LedgerValue::Numeric(value.to_string())
    }

    fn list(values: &[LedgerValue]) -> LedgerValue {
        LedgerValue::List(values.to_vec())
    }

    fn instrument(admin: &str, id: &str) -> LedgerValue {
        rec(&[("admin", party(admin)), ("id", text(id))])
    }

    fn holding_args(owner: &str, admin: &str, id: &str, amount: &str) -> LedgerValue {
        rec(&[
            ("owner", party(owner)),
            ("amount", num(amount)),
            ("instrumentId", instrument(admin, id)),
        ])
    }

    struct OpIds {
        lock: &'static str,
        mint_holding: &'static str,
        minted_lock: &'static str,
        mint_update: &'static str,
        mint_ref: &'static str,
        allocate_update: &'static str,
        payment_allocation: &'static str,
        payment_locked: &'static str,
        treasury_allocation: &'static str,
        settle_update: &'static str,
        buyer_treasury: &'static str,
        seller_payment: &'static str,
        request: &'static str,
        redeem_update: &'static str,
        redeemed_lock: &'static str,
        payout: &'static str,
        trade: &'static str,
    }

    const OP_A: OpIds = OpIds {
        lock: "lock-a",
        mint_holding: "holding-mint-a",
        minted_lock: "minted-a",
        mint_update: "upd-mint-a",
        mint_ref: "mint-ref-a",
        allocate_update: "upd-alloc-a",
        payment_allocation: "alloc-pay-a",
        payment_locked: "locked-pay-a",
        treasury_allocation: "alloc-treas-a",
        settle_update: "upd-settle-a",
        buyer_treasury: "treasury-a",
        seller_payment: "payment-a",
        request: "request-a",
        redeem_update: "upd-redeem-a",
        redeemed_lock: "redeem-a",
        payout: "dest-a",
        trade: "trade-a",
    };

    const OP_B: OpIds = OpIds {
        lock: "lock-b",
        mint_holding: "holding-mint-b",
        minted_lock: "minted-b",
        mint_update: "upd-mint-b",
        mint_ref: "mint-ref-b",
        allocate_update: "upd-alloc-b",
        payment_allocation: "alloc-pay-b",
        payment_locked: "locked-pay-b",
        treasury_allocation: "alloc-treas-b",
        settle_update: "upd-settle-b",
        buyer_treasury: "treasury-b",
        seller_payment: "payment-b",
        request: "request-b",
        redeem_update: "upd-redeem-b",
        redeemed_lock: "redeem-b",
        payout: "dest-b",
        trade: "trade-b",
    };

    fn created(template: &str, cid: &str, update: &str, arguments: LedgerValue) -> CreatedFact {
        CreatedFact {
            template: template.to_string(),
            cid: cid.to_string(),
            update_id: update.to_string(),
            arguments: Some(arguments),
        }
    }

    fn exercised(
        choice: &str,
        cid: &str,
        update: &str,
        consuming: bool,
        argument: LedgerValue,
        result: LedgerValue,
    ) -> ExercisedFact {
        ExercisedFact {
            template: "Template".to_string(),
            choice: choice.to_string(),
            cid: cid.to_string(),
            consuming,
            update_id: update.to_string(),
            argument: Some(argument),
            result: Some(result),
        }
    }

    fn archived(template: &str, cid: &str, update: &str) -> ArchivedFact {
        ArchivedFact {
            template: template.to_string(),
            cid: cid.to_string(),
            update_id: update.to_string(),
        }
    }

    fn operation_history(op: &OpIds) -> CantonHistory {
        let buyer = "buyer::party";
        let seller = "seller::party";
        let cash = "cashRegistry::party";
        let treasury_admin = "treasuryRegistry::party";
        let payment = "100000.000000";
        let treasury = "100.000000";
        CantonHistory {
            created: vec![
                created(
                    "Bridge.Gateway:MintedLock",
                    op.minted_lock,
                    op.mint_update,
                    rec(&[
                        ("cashRegistry", party(cash)),
                        ("lockId", text(op.lock)),
                        ("holdingCid", cid(op.mint_holding)),
                        ("amount", num(payment)),
                        ("beneficiary", party(buyer)),
                    ]),
                ),
                created(
                    "Stablecoin.Holding:StablecoinHolding",
                    op.mint_holding,
                    op.mint_update,
                    holding_args(buyer, cash, "USD-C", payment),
                ),
                created(
                    "Bridge.Binding:BridgeMintRef",
                    op.mint_ref,
                    op.mint_update,
                    rec(&[
                        ("cashRegistry", party(cash)),
                        ("buyer", party(buyer)),
                        ("lockId", text(op.lock)),
                        ("mintHoldingCid", cid(op.mint_holding)),
                    ]),
                ),
                created(
                    "Bridge.Binding:BridgeTradeBinding",
                    if op.lock == "lock-a" {
                        "binding-a"
                    } else {
                        "binding-b"
                    },
                    "upd-bind",
                    rec(&[
                        ("cashRegistry", party(cash)),
                        ("buyer", party(buyer)),
                        ("seller", party(seller)),
                        ("venue", party("venue::party")),
                        ("lockId", text(op.lock)),
                        ("mintHoldingCid", cid(op.mint_holding)),
                        ("tradeCid", text(op.trade)),
                    ]),
                ),
                created(
                    "Stablecoin.Allocation:StablecoinAllocation",
                    op.payment_allocation,
                    op.allocate_update,
                    rec(&[("lockedHoldingCid", cid(op.payment_locked))]),
                ),
                created(
                    "Stablecoin.Holding:StablecoinHolding",
                    op.payment_locked,
                    op.allocate_update,
                    holding_args(buyer, cash, "USD-C", payment),
                ),
                created(
                    "Treasury.Holding:TreasuryHolding",
                    op.buyer_treasury,
                    op.settle_update,
                    holding_args(buyer, treasury_admin, "UST-2028-11", treasury),
                ),
                created(
                    "Stablecoin.Holding:StablecoinHolding",
                    op.seller_payment,
                    op.settle_update,
                    holding_args(seller, cash, "USD-C", payment),
                ),
                created(
                    "Bridge.Gateway:RedemptionRequest",
                    op.request,
                    "upd-request",
                    rec(&[
                        ("holder", party(seller)),
                        ("cashRegistry", party(cash)),
                        ("lockId", text(op.lock)),
                        ("holdingCid", cid(op.seller_payment)),
                        ("amount", num(payment)),
                        ("instrumentId", instrument(cash, "USD-C")),
                        ("payoutDestination", text(op.payout)),
                    ]),
                ),
                created(
                    "Bridge.Gateway:RedeemedLock",
                    op.redeemed_lock,
                    op.redeem_update,
                    rec(&[
                        ("cashRegistry", party(cash)),
                        ("lockId", text(op.lock)),
                        ("amount", num(payment)),
                        ("holder", party(seller)),
                    ]),
                ),
            ],
            exercised: vec![
                exercised(
                    "Gateway_Mint",
                    "gateway",
                    op.mint_update,
                    true,
                    rec(&[]),
                    rec(&[]),
                ),
                exercised(
                    "BridgeMintRef_Bind",
                    op.mint_ref,
                    "upd-bind",
                    true,
                    rec(&[
                        ("seller", party(seller)),
                        ("venue", party("venue::party")),
                        ("tradeCid", text(op.trade)),
                    ]),
                    rec(&[]),
                ),
                exercised(
                    "AllocationFactory_Allocate",
                    "cash-rules",
                    op.allocate_update,
                    false,
                    rec(&[
                        ("expectedAdmin", party(cash)),
                        ("inputHoldingCids", list(&[cid(op.mint_holding)])),
                        (
                            "allocation",
                            rec(&[(
                                "transferLeg",
                                rec(&[
                                    ("sender", party(buyer)),
                                    ("receiver", party(seller)),
                                    ("amount", num(payment)),
                                    ("instrumentId", instrument(cash, "USD-C")),
                                ]),
                            )]),
                        ),
                    ]),
                    rec(&[(
                        "output",
                        LedgerValue::Variant {
                            tag: "AllocationInstructionResult_Completed".to_string(),
                            value: Some(Box::new(rec(&[(
                                "allocationCid",
                                cid(op.payment_allocation),
                            )]))),
                        },
                    )]),
                ),
                exercised(
                    "DvpTrade_Settle",
                    op.trade,
                    op.settle_update,
                    true,
                    rec(&[(
                        "allocations",
                        list(&[
                            rec(&[
                                ("transferLegId", text("treasury-delivery")),
                                ("allocationCid", cid(op.treasury_allocation)),
                            ]),
                            rec(&[
                                ("transferLegId", text("stablecoin-payment")),
                                ("allocationCid", cid(op.payment_allocation)),
                            ]),
                        ]),
                    )]),
                    rec(&[
                        (
                            "treasuryResult",
                            rec(&[("receiverHoldingCids", list(&[cid(op.buyer_treasury)]))]),
                        ),
                        (
                            "paymentResult",
                            rec(&[("receiverHoldingCids", list(&[cid(op.seller_payment)]))]),
                        ),
                    ]),
                ),
                exercised(
                    "Allocation_ExecuteTransfer",
                    op.payment_allocation,
                    op.settle_update,
                    true,
                    rec(&[]),
                    rec(&[("receiverHoldingCids", list(&[cid(op.seller_payment)]))]),
                ),
                exercised(
                    "Gateway_Redeem",
                    "gateway",
                    op.redeem_update,
                    true,
                    rec(&[
                        ("requestCid", cid(op.request)),
                        ("mintedLockCid", cid(op.minted_lock)),
                        (
                            "bindingCid",
                            cid(if op.lock == "lock-a" {
                                "binding-a"
                            } else {
                                "binding-b"
                            }),
                        ),
                    ]),
                    rec(&[]),
                ),
                exercised(
                    "Burn",
                    op.seller_payment,
                    op.redeem_update,
                    true,
                    rec(&[]),
                    rec(&[]),
                ),
            ],
            archived: vec![
                archived("Bridge.Binding:BridgeMintRef", op.mint_ref, "upd-bind"),
                archived(
                    "Stablecoin.Holding:StablecoinHolding",
                    op.mint_holding,
                    op.allocate_update,
                ),
                archived(
                    "Stablecoin.Holding:StablecoinHolding",
                    op.payment_locked,
                    op.settle_update,
                ),
                archived(
                    "Stablecoin.Allocation:StablecoinAllocation",
                    op.payment_allocation,
                    op.settle_update,
                ),
                archived(
                    "Stablecoin.Holding:StablecoinHolding",
                    op.seller_payment,
                    op.redeem_update,
                ),
            ],
        }
    }

    fn merge(left: CantonHistory, right: CantonHistory) -> CantonHistory {
        CantonHistory {
            created: [left.created, right.created].concat(),
            exercised: [left.exercised, right.exercised].concat(),
            archived: [left.archived, right.archived].concat(),
        }
    }

    fn mixed_a_settle_on_b() -> CantonHistory {
        let mut history = operation_history(&OP_B);
        history.exercised.retain(|fact| {
            fact.choice != "DvpTrade_Settle" && fact.update_id != OP_B.settle_update
        });
        history
            .created
            .retain(|fact| fact.cid != OP_B.buyer_treasury && fact.cid != OP_B.seller_payment);
        history
            .archived
            .retain(|fact| fact.update_id != OP_B.settle_update);
        let a = operation_history(&OP_A);
        history.exercised.extend(
            a.exercised
                .into_iter()
                .filter(|fact| fact.choice == "DvpTrade_Settle"),
        );
        history.created.extend(
            a.created
                .into_iter()
                .filter(|fact| fact.cid == OP_A.buyer_treasury || fact.cid == OP_A.seller_payment),
        );
        history
    }

    #[test]
    fn connected_history_accepts_matching_operation() {
        let evidence = connect_canton_history(&operation_history(&OP_A), &expected_a()).unwrap();
        assert_eq!(evidence.mint_holding, "holding-mint-a");
        assert_eq!(evidence.payment_allocation, "alloc-pay-a");
        assert_eq!(evidence.payment_locked, "locked-pay-a");
        assert_eq!(evidence.allocate_update, "upd-alloc-a");
        assert_eq!(evidence.settle_update, "upd-settle-a");
        assert_eq!(evidence.buyer_treasury, "treasury-a");
        assert_eq!(evidence.seller_payment, "payment-a");
        assert_eq!(evidence.redeem_update, "upd-redeem-a");
        assert_eq!(evidence.redeemed_lock, "redeem-a");
        assert_eq!(evidence.payout_destination, "dest-a");
        assert_eq!(evidence.buyer, "buyer::party");
        assert_eq!(evidence.seller, "seller::party");
        assert_eq!(evidence.instrument_id, "USD-C");
        assert_eq!(evidence.payment_admin, "cashRegistry::party");
        assert_eq!(evidence.treasury_instrument, "UST-2028-11");
        assert_eq!(evidence.treasury_admin, "treasuryRegistry::party");
        assert_eq!(evidence.trade_cid, "trade-a");
        assert_ne!(evidence.allocate_update, evidence.settle_update);
    }

    #[test]
    fn two_operations_stay_isolated_on_shared_history() {
        let history = merge(operation_history(&OP_A), operation_history(&OP_B));
        let a = connect_canton_history(&history, &expected_a()).unwrap();
        let b = connect_canton_history(&history, &expected_b()).unwrap();
        assert_eq!(a.settle_update, "upd-settle-a");
        assert_eq!(b.settle_update, "upd-settle-b");
        assert_eq!(a.buyer_treasury, "treasury-a");
        assert_eq!(b.buyer_treasury, "treasury-b");
        assert_eq!(a.trade_cid, "trade-a");
        assert_eq!(b.trade_cid, "trade-b");
        assert_ne!(a.payment_allocation, b.payment_allocation);
        assert_ne!(a.trade_cid, b.trade_cid);
    }

    #[test]
    fn settlement_from_one_operation_does_not_validate_another() {
        let err = connect_canton_history(&mixed_a_settle_on_b(), &expected_b()).unwrap_err();
        assert!(
            err.to_string().contains("not connected")
                || err.to_string().contains("another operation"),
            "A's settlement must not validate B: {err}"
        );
        connect_canton_history(&operation_history(&OP_B), &expected_b()).unwrap();
    }

    #[test]
    fn missing_instrument_admin_fails() {
        let mut history = operation_history(&OP_A);
        if let Some(holding) = history
            .created
            .iter_mut()
            .find(|fact| fact.cid == "holding-mint-a")
        {
            holding.arguments = Some(rec(&[
                ("owner", party("buyer::party")),
                ("amount", num("100000.000000")),
                ("instrumentId", rec(&[("id", text("USD-C"))])),
            ]));
        }
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "missing admin must fail: {err}"
        );
    }

    fn with_unrelated_holdings() -> CantonHistory {
        let mut history = operation_history(&OP_A);
        history.created.push(created(
            "Treasury.Holding:TreasuryHolding",
            "unrelated-treasury",
            "upd-unrelated",
            holding_args(
                "buyer::party",
                "treasuryRegistry::party",
                "UST-2028-11",
                "50.000000",
            ),
        ));
        history.created.push(created(
            "Stablecoin.Holding:StablecoinHolding",
            "unrelated-payment",
            "upd-unrelated",
            holding_args(
                "seller::party",
                "cashRegistry::party",
                "USD-C",
                "25000.000000",
            ),
        ));
        history
    }

    fn without_choice(history: &CantonHistory, choice: &str) -> CantonHistory {
        CantonHistory {
            created: history.created.clone(),
            exercised: history
                .exercised
                .iter()
                .filter(|fact| fact.choice != choice)
                .cloned()
                .collect(),
            archived: history.archived.clone(),
        }
    }

    fn settle_a_redeem_b() -> CantonHistory {
        let mut history = operation_history(&OP_A);
        history
            .created
            .retain(|fact| fact.cid != OP_A.request && fact.cid != OP_A.redeemed_lock);
        history
            .exercised
            .retain(|fact| fact.choice != "Gateway_Redeem" && fact.choice != "Burn");
        history
            .archived
            .retain(|fact| fact.update_id != OP_A.redeem_update);
        let b = operation_history(&OP_B);
        history.created.extend(
            b.created
                .iter()
                .filter(|fact| {
                    fact.cid == OP_B.request
                        || fact.cid == OP_B.redeemed_lock
                        || fact.cid == OP_B.minted_lock
                        || fact.cid == OP_B.mint_holding
                })
                .cloned(),
        );
        history.exercised.extend(
            b.exercised
                .iter()
                .filter(|fact| fact.choice == "Gateway_Redeem" || fact.choice == "Burn")
                .cloned(),
        );
        history.archived.extend(
            b.archived
                .iter()
                .filter(|fact| fact.update_id == OP_B.redeem_update)
                .cloned(),
        );
        history
    }

    #[test]
    fn unrelated_buyer_treasury_holdings_do_not_break_connection() {
        let evidence = connect_canton_history(&with_unrelated_holdings(), &expected_a()).unwrap();
        assert_eq!(evidence.buyer_treasury, "treasury-a");
        assert_ne!(evidence.buyer_treasury, "unrelated-treasury");
    }

    #[test]
    fn unrelated_seller_stablecoin_holdings_do_not_break_connection() {
        let evidence = connect_canton_history(&with_unrelated_holdings(), &expected_a()).unwrap();
        assert_eq!(evidence.seller_payment, "payment-a");
        assert_ne!(evidence.seller_payment, "unrelated-payment");
    }

    #[test]
    fn settlement_from_one_operation_cannot_complete_another_redemption() {
        let err = connect_canton_history(&settle_a_redeem_b(), &expected_b()).unwrap_err();
        assert!(
            err.to_string().contains("missing")
                || err.to_string().contains("another operation")
                || err.to_string().contains("not connected")
                || err.to_string().contains("not created by Gateway_Mint"),
            "A's settlement plus B's redemption must fail: {err}"
        );
        let err = connect_canton_history(&settle_a_redeem_b(), &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("missing")
                || err.to_string().contains("not connected")
                || err.to_string().contains("another operation")
                || err.to_string().contains("not created by Gateway_Mint"),
            "A's settlement plus B's redemption must not complete A: {err}"
        );
    }

    #[test]
    fn mint_ref_not_created_by_gateway_mint_fails() {
        let mut history = operation_history(&OP_A);
        if let Some(mint_ref) = history
            .created
            .iter_mut()
            .find(|fact| template_ends(fact, "BridgeMintRef"))
        {
            mint_ref.update_id = "upd-forged".to_string();
        }
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("not created by Gateway_Mint"),
            "forged mint reference must fail: {err}"
        );
    }

    #[test]
    fn missing_mint_ref_fails_closed() {
        let mut history = operation_history(&OP_A);
        history
            .created
            .retain(|fact| !template_ends(fact, "BridgeMintRef"));
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("not created by Gateway_Mint")
                || err.to_string().contains("missing"),
            "missing mint reference must fail: {err}"
        );
    }

    #[test]
    fn bind_must_consume_the_gateway_mint_ref() {
        let mut history = operation_history(&OP_A);
        history
            .exercised
            .retain(|fact| fact.choice != "BridgeMintRef_Bind");
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            err.to_string()
                .contains("not created by consuming the mint reference")
                || err.to_string().contains("missing"),
            "binding without consuming the mint reference must fail: {err}"
        );
    }

    #[test]
    fn missing_binding_fails_closed() {
        let mut history = operation_history(&OP_A);
        history
            .created
            .retain(|fact| !template_ends(fact, "BridgeTradeBinding"));
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            (err.to_string().contains("missing") && err.to_string().contains("BridgeTradeBinding"))
                || err
                    .to_string()
                    .contains("not created by consuming the mint reference"),
            "missing binding must fail: {err}"
        );
    }

    #[test]
    fn binding_to_the_other_operations_trade_fails() {
        let mut history = operation_history(&OP_A);
        if let Some(binding) = history
            .created
            .iter_mut()
            .find(|fact| template_ends(fact, "BridgeTradeBinding"))
        {
            binding.arguments = Some(rec(&[
                ("lockId", text("lock-a")),
                ("mintHoldingCid", cid("holding-mint-a")),
                ("tradeCid", text("trade-b")),
            ]));
        }
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("not the trade bound")
                || err.to_string().contains("another operation")
                || err.to_string().contains("not connected"),
            "A's mint bound to B's trade must fail: {err}"
        );
    }

    #[test]
    fn redeem_binding_from_another_operation_fails() {
        let mut history = operation_history(&OP_A);
        for fact in &mut history.exercised {
            if fact.choice == "Gateway_Redeem" {
                fact.argument = Some(rec(&[
                    ("requestCid", cid(OP_A.request)),
                    ("mintedLockCid", cid(OP_A.minted_lock)),
                    ("bindingCid", cid("binding-b")),
                ]));
            }
        }
        let err = connect_canton_history(&history, &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("not connected")
                || err.to_string().contains("another operation"),
            "redeem must name this operation's binding: {err}"
        );
    }

    #[test]
    fn mixed_ab_history_rejects_crossed_events() {
        let err = connect_canton_history(&mixed_a_settle_on_b(), &expected_b()).unwrap_err();
        assert!(
            err.to_string().contains("not connected")
                || err.to_string().contains("another operation")
                || err.to_string().contains("not the trade bound"),
            "mixed A/B events must fail: {err}"
        );
        let err = connect_canton_history(&settle_a_redeem_b(), &expected_a()).unwrap_err();
        assert!(
            err.to_string().contains("missing")
                || err.to_string().contains("not connected")
                || err.to_string().contains("another operation")
                || err.to_string().contains("not created by Gateway_Mint"),
            "mixed A settle / B redeem must fail: {err}"
        );
    }

    #[test]
    fn missing_allocation_fails_closed() {
        let err = connect_canton_history(
            &without_choice(&operation_history(&OP_A), "AllocationFactory_Allocate"),
            &expected_a(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "missing allocation must fail: {err}"
        );
    }

    #[test]
    fn missing_settlement_fails_closed() {
        let err = connect_canton_history(
            &without_choice(&operation_history(&OP_A), "DvpTrade_Settle"),
            &expected_a(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("missing") || err.to_string().contains("not connected"),
            "missing settlement must fail: {err}"
        );
    }

    #[test]
    fn missing_burn_fails_closed() {
        let err = connect_canton_history(
            &without_choice(&operation_history(&OP_A), "Burn"),
            &expected_a(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("missing") || err.to_string().contains("not connected"),
            "missing burn must fail: {err}"
        );
    }

    #[test]
    fn unreadable_history_fails_closed() {
        let err = parse_canton_history_facts("CANTON_FACT {not-json").unwrap_err();
        assert!(
            err.to_string().contains("cannot be read"),
            "unreadable history must fail: {err}"
        );
        assert!(parse_canton_history_facts("CANTON_FACT {\"kind\":\"other\"}").is_err());
    }

    #[test]
    fn repeated_verification_of_the_same_completed_operation() {
        let history = operation_history(&OP_A);
        let first = connect_canton_history(&history, &expected_a()).unwrap();
        let second = connect_canton_history(&history, &expected_a()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.settle_update, "upd-settle-a");
        assert_eq!(second.redeemed_lock, "redeem-a");
    }

    #[test]
    fn parses_dumped_facts_and_connects() {
        let stdout = r#"
CANTON_FACT {"kind":"created","template":"Bridge.Gateway:MintedLock","cid":"minted-a","updateId":"upd-mint","arguments":{"record":{"cashRegistry":{"party":"cashRegistry::party"},"lockId":{"text":"lock-a"},"holdingCid":{"cid":"holding-mint-a"},"amount":{"numeric":"100000.000000"},"beneficiary":{"party":"buyer::party"}}}}
"#;
        let parsed = parse_canton_history_facts(stdout).unwrap();
        assert_eq!(parsed.created[0].cid, "minted-a");
        assert_eq!(
            field_text(parsed.created[0].arguments.as_ref(), "lockId"),
            Some("lock-a")
        );
    }
}
