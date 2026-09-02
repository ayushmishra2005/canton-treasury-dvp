use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

pub struct ZamaClient {
    rpc_url: String,
    engine: String,
    requester_key: String,
    settler_key: String,
    client_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZamaReceipt {
    pub approved: bool,
    pub tx_hash: Option<String>,
    pub gas_used: Option<String>,
}

impl ZamaClient {
    pub fn new(
        rpc_url: String,
        engine: String,
        requester_key: String,
        settler_key: String,
        client_id: String,
    ) -> Self {
        Self {
            rpc_url,
            engine,
            requester_key,
            settler_key,
            client_id,
        }
    }

    pub fn reserve(&self, reservation_hex: &str, amount: u64) -> Result<ZamaReceipt> {
        let receipt = self.cast(
            "reserve",
            &self.requester_key,
            &[
                reservation_hex.to_string(),
                self.client_id.clone(),
                amount.to_string(),
            ],
        )?;
        print_zama_receipt("RESERVE", &receipt);
        Ok(receipt)
    }

    pub fn finalize(&self, reservation_hex: &str) -> Result<ZamaReceipt> {
        let receipt = self.cast(
            "finalize",
            &self.settler_key,
            &[reservation_hex.to_string()],
        )?;
        print_zama_receipt("FINALIZE", &receipt);
        Ok(receipt)
    }

    pub fn cancel(&self, reservation_hex: &str) -> Result<ZamaReceipt> {
        let receipt = self.cast("cancel", &self.settler_key, &[reservation_hex.to_string()])?;
        print_zama_receipt("CANCEL", &receipt);
        Ok(receipt)
    }

    pub fn redeem(&self, reservation_hex: &str) -> Result<ZamaReceipt> {
        let receipt = self.cast("redeem", &self.settler_key, &[reservation_hex.to_string()])?;
        print_zama_receipt("REDEEM", &receipt);
        Ok(receipt)
    }

    pub fn status(&self, reservation_hex: &str) -> Result<u8> {
        let value = self.query("status", reservation_hex)?;
        value
            .get("status")
            .and_then(|v| v.as_u64())
            .map(|status| status as u8)
            .ok_or_else(|| anyhow!("zama status did not return status"))
    }

    pub fn approved(&self, reservation_hex: &str) -> Result<bool> {
        let value = self.query("approved", reservation_hex)?;
        value
            .get("approved")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow!("zama approved did not return approved"))
    }

    fn query(&self, method: &str, reservation_hex: &str) -> Result<Value> {
        let output = run_hardhat(
            method,
            &self.rpc_url,
            &self.engine,
            &self.settler_key,
            reservation_hex,
        )?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_zama_result(&stdout).ok_or_else(|| anyhow!("zama {method} did not print ZAMA_RESULT"))
    }

    fn cast(&self, method: &str, key: &str, args: &[String]) -> Result<ZamaReceipt> {
        let output = run_hardhat(method, &self.rpc_url, &self.engine, key, &args.join(","))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut receipt = parse_zama_receipt(&stdout);
        let value = parse_zama_result(&stdout)
            .ok_or_else(|| anyhow!("zama {method} did not print ZAMA_RESULT"))?;
        if method == "reserve" {
            receipt.approved = value
                .get("approved")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| anyhow!("zama reserve did not return approved"))?;
        } else {
            receipt.approved = true;
        }
        Ok(receipt)
    }
}

fn run_hardhat(method: &str, rpc_url: &str, engine: &str, key: &str, args: &str) -> Result<Output> {
    let script =
        std::env::var("ZAMA_BRIDGE_RPC").unwrap_or_else(|_| "scripts/bridge-rpc.ts".into());
    let network = std::env::var("ZAMA_HARDHAT_NETWORK").unwrap_or_else(|_| "localhost".into());
    let dir = std::env::var("ZAMA_DIR").unwrap_or_else(|_| "zama".into());
    let mut last = String::new();
    for attempt in 1..=5 {
        let output = Command::new("npx")
            .current_dir(&dir)
            .args(["hardhat", "run", &script, "--network", &network])
            .env("ZAMA_RPC_URL", rpc_url)
            .env("ZAMA_ENGINE", engine)
            .env("ZAMA_KEY", key)
            .env("ZAMA_METHOD", method)
            .env("ZAMA_ARGS", args)
            .output()
            .with_context(|| format!("zama {method}"))?;
        if output.status.success() {
            return Ok(output);
        }
        last = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !is_transient_zama_rpc(&last) || attempt == 5 {
            break;
        }
        println!("ZAMA_RPC_RETRY {attempt} {method}");
        thread::sleep(Duration::from_secs(attempt as u64 * 2));
    }
    Err(anyhow!("zama {method} failed: {last}"))
}

fn is_transient_zama_rpc(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("headerstimeout")
        || text.contains("und_err_headers_timeout")
        || text.contains("econnreset")
        || text.contains("socket hang up")
        || text.contains("503")
        || text.contains("502")
        || text.contains("429")
        || text.contains("timed out")
}

fn print_zama_receipt(kind: &str, receipt: &ZamaReceipt) {
    if let Some(hash) = &receipt.tx_hash {
        println!("ZAMA_{kind}_TX {hash}");
    }
    if let Some(gas) = &receipt.gas_used {
        println!("ZAMA_{kind}_GAS {gas}");
    }
}

fn parse_zama_result(stdout: &str) -> Option<Value> {
    stdout.lines().rev().find_map(|line| {
        line.strip_prefix("ZAMA_RESULT ")
            .and_then(|rest| serde_json::from_str(rest).ok())
    })
}

fn parse_zama_receipt(stdout: &str) -> ZamaReceipt {
    let mut receipt = ZamaReceipt::default();
    for line in stdout.lines() {
        if let Some(hash) = line.strip_prefix("ZAMA_TX ") {
            if looks_like_tx_hash(hash) {
                receipt.tx_hash = Some(hash.trim().to_string());
            }
        }
        if let Some(gas) = line.strip_prefix("ZAMA_GAS ") {
            if gas.chars().all(|c| c.is_ascii_digit()) {
                receipt.gas_used = Some(gas.trim().to_string());
            }
        }
    }
    receipt
}

fn looks_like_tx_hash(value: &str) -> bool {
    let value = value.trim();
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_successful_zama_receipt_lines() {
        let stdout = "\
ZAMA_TX 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
ZAMA_GAS 123456
ZAMA_RESULT {\"approved\":true}
";
        let receipt = parse_zama_receipt(stdout);
        assert_eq!(
            receipt.tx_hash.as_deref(),
            Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(receipt.gas_used.as_deref(), Some("123456"));
        let value = parse_zama_result(stdout).unwrap();
        assert_eq!(value.get("approved").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn retries_transient_public_rpc_timeouts() {
        assert!(is_transient_zama_rpc(
            "zama status failed: HeadersTimeoutError: Headers Timeout Error"
        ));
        assert!(is_transient_zama_rpc("UND_ERR_HEADERS_TIMEOUT"));
        assert!(!is_transient_zama_rpc("Zama rejected the reservation"));
    }

    #[test]
    fn ignores_non_hash_zama_tx_lines() {
        let stdout = "ZAMA_TX not-a-hash\nZAMA_RESULT {\"ok\":true}\n";
        let receipt = parse_zama_receipt(stdout);
        assert!(receipt.tx_hash.is_none());
    }
}
