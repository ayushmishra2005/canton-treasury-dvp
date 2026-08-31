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
}
