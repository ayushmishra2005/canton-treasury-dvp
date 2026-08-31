use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct CantonClient {
    dar: PathBuf,
    participants: PathBuf,
}

impl CantonClient {
    pub fn new(dar: PathBuf, participants: PathBuf) -> Self {
        Self { dar, participants }
    }

    pub fn prepare(&self) -> Result<()> {
        self.run_script("Tests.Bridge.Runtime:prepare", "", "0", "", "")?;
        Ok(())
    }

    pub fn mint(&self, lock_id: &str, canton_amount: &str, digest_hex: &str) -> Result<String> {
        let stdout = self.run_script(
            "Tests.Bridge.Runtime:mint",
            lock_id,
            canton_amount,
            digest_hex,
            "",
        )?;
        marker(&stdout, "CANTON_MINT_HOLDING")
    }

    pub fn prepare_trade(
        &self,
        lock_id: &str,
        canton_amount: &str,
        digest_hex: &str,
    ) -> Result<String> {
        let stdout = self.run_script(
            "Tests.Bridge.Runtime:prepareTrade",
            lock_id,
            canton_amount,
            digest_hex,
            "",
        )?;
        marker(&stdout, "CANTON_TRADE")
    }

    pub fn settle(
        &self,
        lock_id: &str,
        canton_amount: &str,
        digest_hex: &str,
    ) -> Result<SettleEvidence> {
        let stdout = self.run_script(
            "Tests.Bridge.Runtime:settle",
            lock_id,
            canton_amount,
            digest_hex,
            "",
        )?;
        Ok(SettleEvidence {
            buyer_treasury: marker(&stdout, "CANTON_SETTLE_BUYER_TREASURY")?,
            seller_stablecoin: marker(&stdout, "CANTON_SETTLE_SELLER_STABLECOIN")?,
            payment_amount: marker(&stdout, "CANTON_SETTLE_PAYMENT_AMOUNT")?,
            treasury_amount: marker(&stdout, "CANTON_SETTLE_TREASURY_AMOUNT")?,
            consumed_payment: optional_marker(&stdout, "CANTON_CONSUMED_PAYMENT"),
            consumed_treasury: optional_marker(&stdout, "CANTON_CONSUMED_TREASURY"),
            raw: stdout,
        })
    }

    pub fn verify_completion(
        &self,
        expected: &CantonLedgerExpectation,
    ) -> Result<CantonLedgerEvidence> {
        let acs = match self.run_script(
            "Tests.Bridge.Runtime:verifyCompletion",
            &expected.lock_id,
            &expected.canton_amount,
            "",
            &expected.payout_destination,
        ) {
            Ok(stdout) => stdout,
            Err(err) => {
                let text = err.to_string();
                if text.contains("Canton completion missing")
                    || text.contains("Expected one")
                    || text.contains("CANTON_VERIFY_FAIL")
                {
                    return Ok(CantonLedgerEvidence::default());
                }
                return Err(anyhow!("Canton ledger evidence cannot be read: {err}"));
            }
        };
        let history = match self.run_console_env(
            "canton/scripts/verify-bridge-completion.canton",
            &[
                ("BRIDGE_LOCK_ID", expected.lock_id.as_str()),
                ("BRIDGE_CANTON_AMOUNT", expected.canton_amount.as_str()),
                ("BRIDGE_TREASURY_AMOUNT", expected.treasury_amount.as_str()),
                ("BRIDGE_PAYOUT_DEST", expected.payout_destination.as_str()),
                ("BRIDGE_MINT_HOLDING", expected.mint_holding.as_str()),
            ],
        ) {
            Ok(stdout) => stdout,
            Err(err) => {
                let text = err.to_string();
                if text.contains("CANTON_HISTORY_OTHER_LOCK") || text.contains("another operation")
                {
                    return Err(anyhow!(
                        "Canton ledger evidence belongs to another operation"
                    ));
                }
                if text.contains("CANTON_VERIFY_UNREADABLE") {
                    return Err(anyhow!("Canton ledger evidence cannot be read: {err}"));
                }
                if text.contains("CANTON_VERIFY_FAIL") || text.contains("missing") {
                    return Ok(parse_canton_ledger_evidence(&format!("{acs}\n{text}")));
                }
                return Err(anyhow!("Canton ledger evidence cannot be read: {err}"));
            }
        };
        let combined = format!("{acs}\n{history}");
        if combined.contains("CANTON_VERIFY_UNREADABLE") {
            return Err(anyhow!("Canton ledger evidence cannot be read"));
        }
        if combined.contains("CANTON_HISTORY_OTHER_LOCK") || combined.contains("another operation")
        {
            return Err(anyhow!(
                "Canton ledger evidence belongs to another operation"
            ));
        }
        Ok(parse_canton_ledger_evidence(&combined))
    }

    pub fn redeem(
        &self,
        lock_id: &str,
        canton_amount: &str,
        digest_hex: &str,
        payout_destination: &str,
    ) -> Result<String> {
        let stdout = self.run_script(
            "Tests.Bridge.Runtime:redeem",
            lock_id,
            canton_amount,
            digest_hex,
            payout_destination,
        )?;
        let dest = marker(&stdout, "CANTON_PAYOUT_DEST")?;
        anyhow::ensure!(
            dest == payout_destination,
            "redemption payout destination {dest} did not match {payout_destination}"
        );
        marker(&stdout, "CANTON_REDEEM")
    }

    pub fn originate(&self) -> Result<String> {
        self.run_console("canton/scripts/origination.canton")
    }

    pub fn grant_reassignment(&self) -> Result<String> {
        self.run_console_env(
            "canton/scripts/reassignment-capability.canton",
            &[("REASSIGNMENT_CAPABILITY", "granted")],
        )
    }

    pub fn reassign(&self) -> Result<String> {
        self.run_console("canton/scripts/reassign.canton")
    }

    pub fn revoke_reassignment(&self) -> Result<String> {
        self.run_console_env(
            "canton/scripts/reassignment-capability.canton",
            &[("REASSIGNMENT_CAPABILITY", "revoked")],
        )
    }

    fn run_script(
        &self,
        name: &str,
        lock_id: &str,
        amount: &str,
        digest_hex: &str,
        payout_destination: &str,
    ) -> Result<String> {
        let input = self.write_input(lock_id, amount, digest_hex, payout_destination)?;
        let output = Command::new("dpm")
            .args([
                "script",
                "--dar",
                self.dar.to_str().unwrap(),
                "--script-name",
                name,
                "--participant-config",
                self.participants.to_str().unwrap(),
                "--input-file",
                input.to_str().unwrap(),
                "--wall-clock-time",
            ])
            .output()
            .context("dpm script")?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(anyhow!("canton {name} failed: {stdout} {stderr}"));
        }
        Ok(format!("{stdout}\n{stderr}"))
    }

    fn run_console(&self, script: &str) -> Result<String> {
        self.run_console_env(script, &[])
    }

    fn run_console_env(&self, script: &str, extra: &[(&str, &str)]) -> Result<String> {
        let jar = canton_jar()?;
        let run_dir =
            std::env::var("CANTON_RUN_DIR").unwrap_or_else(|_| "canton/.run-bridge".to_string());
        let mut command = Command::new("java");
        command
            .args([
                "-jar",
                jar.to_str().unwrap(),
                "run",
                script,
                "-c",
                "canton/remote-console.conf",
                "--no-tty",
                "--log-level-stdout",
                "WARN",
            ])
            .env("CANTON_RUN_DIR", run_dir);
        for (key, value) in extra {
            command.env(key, value);
        }
        let output = command.output().context("canton console")?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            return Err(anyhow!("canton {script} failed: {stdout} {stderr}"));
        }
        Ok(format!("{stdout}\n{stderr}"))
    }

    fn write_input(
        &self,
        lock_id: &str,
        amount: &str,
        digest_hex: &str,
        payout_destination: &str,
    ) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!("bridge-canton-{lock_id}.json"));
        fs_write_atomic(
            &path,
            script_input_json(lock_id, amount, digest_hex, payout_destination),
        )?;
        Ok(path)
    }
}

pub struct SettleEvidence {
    pub buyer_treasury: String,
    pub seller_stablecoin: String,
    pub payment_amount: String,
    pub treasury_amount: String,
    pub consumed_payment: Option<String>,
    pub consumed_treasury: Option<String>,
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CantonLedgerExpectation {
    pub lock_id: String,
    pub canton_amount: String,
    pub treasury_amount: String,
    pub payout_destination: String,
    pub mint_holding: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CantonLedgerEvidence {
    pub lock_id: String,
    pub mint_holding: String,
    pub mint_consumed: bool,
    pub buyer_treasury: String,
    pub seller_payment: String,
    pub seller_burned: bool,
    pub redeemed_lock: String,
    pub payout_destination: String,
    pub payment_amount: String,
    pub treasury_amount: String,
    pub instrument_id: String,
    pub settle_seen: bool,
    pub redeem_seen: bool,
}

pub fn require_canton_ledger_evidence(
    read: Result<CantonLedgerEvidence>,
    expected: &CantonLedgerExpectation,
) -> Result<CantonLedgerEvidence> {
    let evidence = read.map_err(|err| anyhow!("Canton ledger evidence cannot be read: {err}"))?;
    if !evidence.lock_id.is_empty() && evidence.lock_id != expected.lock_id {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    if expected.lock_id.is_empty()
        || expected.mint_holding.is_empty()
        || expected.payout_destination.is_empty()
        || expected.canton_amount.is_empty()
    {
        return Err(anyhow!(
            "Canton completion is missing recorded operation fields"
        ));
    }
    if evidence.lock_id.is_empty()
        || evidence.mint_holding.is_empty()
        || evidence.buyer_treasury.is_empty()
        || evidence.seller_payment.is_empty()
        || evidence.redeemed_lock.is_empty()
        || evidence.payout_destination.is_empty()
        || !evidence.mint_consumed
        || !evidence.seller_burned
        || !evidence.settle_seen
        || !evidence.redeem_seen
    {
        return Err(anyhow!("Canton ledger evidence is missing"));
    }
    if evidence.mint_holding != expected.mint_holding {
        return Err(anyhow!(
            "Canton ledger evidence belongs to another operation"
        ));
    }
    if evidence.payout_destination != expected.payout_destination {
        return Err(anyhow!(
            "Canton payout destination does not match the recorded operation"
        ));
    }
    if !canton_amounts_match(&evidence.payment_amount, &expected.canton_amount) {
        return Err(anyhow!("Canton payment amount does not match"));
    }
    if !canton_amounts_match(&evidence.treasury_amount, &expected.treasury_amount) {
        return Err(anyhow!("Canton Treasury amount does not match"));
    }
    if evidence.instrument_id != "USD-C" {
        return Err(anyhow!("Canton instrument does not match"));
    }
    Ok(evidence)
}

fn canton_amounts_match(left: &str, right: &str) -> bool {
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

fn parse_canton_ledger_evidence(stdout: &str) -> CantonLedgerEvidence {
    CantonLedgerEvidence {
        lock_id: optional_marker(stdout, "CANTON_HISTORY_LOCK")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_LOCK"))
            .unwrap_or_default(),
        mint_holding: optional_marker(stdout, "CANTON_HISTORY_MINT_HOLDING")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_MINT_HOLDING"))
            .unwrap_or_default(),
        mint_consumed: optional_marker(stdout, "CANTON_HISTORY_MINT_CONSUMED").is_some(),
        buyer_treasury: optional_marker(stdout, "CANTON_HISTORY_BUYER_TREASURY")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_BUYER_TREASURY"))
            .unwrap_or_default(),
        seller_payment: optional_marker(stdout, "CANTON_HISTORY_SELLER_PAYMENT")
            .unwrap_or_default(),
        seller_burned: optional_marker(stdout, "CANTON_HISTORY_SELLER_BURN").is_some(),
        redeemed_lock: optional_marker(stdout, "CANTON_HISTORY_REDEEM")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_REDEEM"))
            .unwrap_or_default(),
        payout_destination: optional_marker(stdout, "CANTON_HISTORY_PAYOUT").unwrap_or_default(),
        payment_amount: optional_marker(stdout, "CANTON_HISTORY_PAYMENT_AMOUNT")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_MINT_AMOUNT"))
            .unwrap_or_default(),
        treasury_amount: optional_marker(stdout, "CANTON_HISTORY_TREASURY_AMOUNT")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_TREASURY_AMOUNT"))
            .unwrap_or_default(),
        instrument_id: optional_marker(stdout, "CANTON_HISTORY_INSTRUMENT")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_INSTRUMENT"))
            .unwrap_or_default(),
        settle_seen: optional_marker(stdout, "CANTON_HISTORY_SETTLE").is_some(),
        redeem_seen: optional_marker(stdout, "CANTON_HISTORY_REDEEM")
            .or_else(|| optional_marker(stdout, "CANTON_ACS_REDEEM"))
            .is_some(),
    }
}

fn marker(stdout: &str, label: &str) -> Result<String> {
    optional_marker(stdout, label)
        .ok_or_else(|| anyhow!("missing {label} in canton output: {stdout}"))
}

fn optional_marker(stdout: &str, label: &str) -> Option<String> {
    let normalized = stdout.replace("\\n", "\n");
    normalized.lines().find_map(|line| {
        let cleaned = line.replace(['\\', '"'], "");
        cleaned
            .split(label)
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .map(|value| value.trim_matches(|c| c == '"' || c == '\\').to_string())
            .filter(|value| !value.is_empty())
    })
}

fn canton_jar() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CANTON_JAR") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("DPM_HOME")
        .unwrap_or_else(|_| format!("{}/.dpm", std::env::var("HOME").unwrap_or_default()));
    let root = PathBuf::from(home).join("cache/components/canton-open-source");
    let mut jars = Vec::new();
    visit_jars(&root, &mut jars)?;
    jars.sort();
    jars.pop()
        .ok_or_else(|| anyhow!("canton runtime not found under {root:?}"))
}

fn visit_jars(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            visit_jars(&path, out)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("canton-open-source-") && name.ends_with(".jar"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn fs_write_atomic(path: &Path, body: String) -> Result<()> {
    std::fs::write(path, body)?;
    Ok(())
}

pub fn script_input_json(
    lock_id: &str,
    amount: &str,
    digest_hex: &str,
    payout_destination: &str,
) -> String {
    serde_json::json!({
        "lockId": lock_id,
        "amount": amount,
        "digestHex": digest_hex,
        "payoutDestination": payout_destination,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_json_keeps_exact_decimal_strings() {
        for (amount, base) in [
            ("0.000001", 1u64),
            ("1.000000", 1_000_000),
            ("100000.000000", 100_000_000_000),
            ("9007199254.740993", 9_007_199_254_740_993),
        ] {
            let json = script_input_json("lock-1", amount, "aa", "dest");
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            let field = value.get("amount").expect("amount field");
            assert!(
                field.is_string(),
                "amount must stay a decimal string through JSON, got {field}"
            );
            assert_eq!(field.as_str(), Some(amount));
            assert!(
                !json.contains("9007199254.740992"),
                "JSON must not round 9007199254.740993 to 9007199254.740992: {json}"
            );
            assert_eq!(
                crate::units::canton_decimal_to_base_units(field.as_str().unwrap(), 6).unwrap(),
                base
            );
        }
    }

    #[test]
    fn reads_daml_debug_quoted_markers() {
        let out = "[DA.Internal.Prelude:557]: \\\"CANTON_MINT_HOLDING 00aa713b35856bdb243425c68799684fb25f4e50a37fd34a5399e3ebcf5a838835ca121220abba9a156027012f0fa5c6641b007bae8d269b76bcd492e7a6fe90debc83f628\\\"";
        assert_eq!(
            optional_marker(out, "CANTON_MINT_HOLDING").unwrap(),
            "00aa713b35856bdb243425c68799684fb25f4e50a37fd34a5399e3ebcf5a838835ca121220abba9a156027012f0fa5c6641b007bae8d269b76bcd492e7a6fe90debc83f628"
        );
        let multiline = "CANTON_SETTLE_BUYER_TREASURY 00aa\\nCANTON_SETTLE_SELLER_STABLECOIN 00bb\\nCANTON_SETTLE_PAYMENT_AMOUNT 100000.0";
        assert_eq!(
            optional_marker(multiline, "CANTON_SETTLE_BUYER_TREASURY").unwrap(),
            "00aa"
        );
        assert_eq!(
            optional_marker(multiline, "CANTON_SETTLE_PAYMENT_AMOUNT").unwrap(),
            "100000.0"
        );
    }

    fn expected_completion() -> CantonLedgerExpectation {
        CantonLedgerExpectation {
            lock_id: "lock-a".to_string(),
            canton_amount: "100000.000000".to_string(),
            treasury_amount: "100.000000".to_string(),
            payout_destination: "dest".to_string(),
            mint_holding: "holding-a".to_string(),
        }
    }

    fn valid_evidence() -> CantonLedgerEvidence {
        CantonLedgerEvidence {
            lock_id: "lock-a".to_string(),
            mint_holding: "holding-a".to_string(),
            mint_consumed: true,
            buyer_treasury: "treasury-a".to_string(),
            seller_payment: "payment-a".to_string(),
            seller_burned: true,
            redeemed_lock: "redeem-a".to_string(),
            payout_destination: "dest".to_string(),
            payment_amount: "100000.0".to_string(),
            treasury_amount: "100.0".to_string(),
            instrument_id: "USD-C".to_string(),
            settle_seen: true,
            redeem_seen: true,
        }
    }

    #[test]
    fn journal_completion_is_rejected_when_ledger_evidence_is_missing() {
        let expected = expected_completion();
        let err = require_canton_ledger_evidence(Ok(CantonLedgerEvidence::default()), &expected)
            .unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "empty ledger evidence must not complete: {err}"
        );
        let mut acs_only = valid_evidence();
        acs_only.mint_consumed = false;
        acs_only.seller_burned = false;
        acs_only.settle_seen = false;
        let err = require_canton_ledger_evidence(Ok(acs_only), &expected).unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "active-contract absence is not consume proof: {err}"
        );
    }

    #[test]
    fn journal_completion_is_rejected_when_evidence_is_for_another_operation() {
        let expected = expected_completion();
        let mut other = valid_evidence();
        other.lock_id = "lock-b".to_string();
        let err = require_canton_ledger_evidence(Ok(other), &expected).unwrap_err();
        assert!(
            err.to_string().contains("another operation"),
            "foreign lock must not complete: {err}"
        );
        let mut other_holding = valid_evidence();
        other_holding.mint_holding = "holding-b".to_string();
        let err = require_canton_ledger_evidence(Ok(other_holding), &expected).unwrap_err();
        assert!(
            err.to_string().contains("another operation"),
            "foreign holding must not complete: {err}"
        );
    }

    #[test]
    fn journal_completion_is_rejected_when_ledger_evidence_cannot_be_read() {
        let expected = expected_completion();
        let err =
            require_canton_ledger_evidence(Err(anyhow!("console down")), &expected).unwrap_err();
        assert!(
            err.to_string().contains("cannot be read"),
            "unread ledger evidence must not complete: {err}"
        );
    }

    #[test]
    fn matching_history_evidence_completes() {
        let evidence =
            require_canton_ledger_evidence(Ok(valid_evidence()), &expected_completion()).unwrap();
        assert_eq!(evidence.mint_holding, "holding-a");
        assert!(evidence.mint_consumed);
        assert!(evidence.seller_burned);
        assert!(evidence.settle_seen);
        let mut padded = valid_evidence();
        padded.payment_amount = "100000.0000000000".to_string();
        padded.treasury_amount = "100.0000000000".to_string();
        require_canton_ledger_evidence(Ok(padded), &expected_completion()).unwrap();
    }

    #[test]
    fn history_markers_are_required_for_archived_contracts() {
        let parsed = parse_canton_ledger_evidence(
            "CANTON_ACS_LOCK lock-a\nCANTON_ACS_MINT_HOLDING holding-a\nCANTON_ACS_REDEEM redeem-a\nCANTON_ACS_BUYER_TREASURY treasury-a",
        );
        assert!(!parsed.mint_consumed);
        assert!(!parsed.seller_burned);
        assert!(!parsed.settle_seen);
        assert!(require_canton_ledger_evidence(Ok(parsed), &expected_completion()).is_err());
        let parsed = parse_canton_ledger_evidence(
            "CANTON_HISTORY_LOCK lock-a\nCANTON_HISTORY_MINT_HOLDING holding-a\nCANTON_HISTORY_MINT_CONSUMED holding-a\nCANTON_HISTORY_SETTLE upd-1\nCANTON_HISTORY_BUYER_TREASURY treasury-a\nCANTON_HISTORY_SELLER_PAYMENT payment-a\nCANTON_HISTORY_SELLER_BURN payment-a\nCANTON_HISTORY_REDEEM redeem-a\nCANTON_HISTORY_PAYOUT dest\nCANTON_HISTORY_PAYMENT_AMOUNT 100000.000000\nCANTON_HISTORY_TREASURY_AMOUNT 100.000000\nCANTON_HISTORY_INSTRUMENT USD-C",
        );
        require_canton_ledger_evidence(Ok(parsed), &expected_completion()).unwrap();
    }
}
