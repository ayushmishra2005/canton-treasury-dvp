use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::process::Command;

pub struct ZamaClient {
    rpc_url: String,
    engine: String,
    requester_key: String,
    settler_key: String,
    client_id: String,
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

    pub fn reserve(&self, reservation_hex: &str, amount: u64) -> Result<bool> {
        self.cast(
            "reserve",
            &self.requester_key,
            &[
                reservation_hex.to_string(),
                self.client_id.clone(),
                amount.to_string(),
            ],
        )
    }

    pub fn finalize(&self, reservation_hex: &str) -> Result<()> {
        self.cast(
            "finalize",
            &self.settler_key,
            &[reservation_hex.to_string()],
        )?;
        Ok(())
    }

    pub fn cancel(&self, reservation_hex: &str) -> Result<()> {
        self.cast("cancel", &self.settler_key, &[reservation_hex.to_string()])?;
        Ok(())
    }

    pub fn redeem(&self, reservation_hex: &str) -> Result<()> {
        self.cast("redeem", &self.settler_key, &[reservation_hex.to_string()])?;
        Ok(())
    }

    fn cast(&self, method: &str, key: &str, args: &[String]) -> Result<bool> {
        let script = std::env::var("ZAMA_BRIDGE_RPC")
            .unwrap_or_else(|_| "scripts/bridge-rpc.ts".to_string());
        let mut command = Command::new("npx");
        command
            .current_dir(std::env::var("ZAMA_DIR").unwrap_or_else(|_| "zama".to_string()))
            .args(["hardhat", "run", &script, "--network", "localhost"])
            .env("ZAMA_RPC_URL", &self.rpc_url)
            .env("ZAMA_ENGINE", &self.engine)
            .env("ZAMA_KEY", key)
            .env("ZAMA_METHOD", method)
            .env("ZAMA_ARGS", args.join(","));
        let output = command.output().context("zama hardhat run")?;
        if !output.status.success() {
            return Err(anyhow!(
                "zama {method} failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().rev() {
            if let Some(rest) = line.strip_prefix("ZAMA_RESULT ") {
                let value: Value = serde_json::from_str(rest)?;
                if method == "reserve" {
                    return value
                        .get("approved")
                        .and_then(|v| v.as_bool())
                        .ok_or_else(|| anyhow!("zama reserve did not return approved"));
                }
                return Ok(true);
            }
        }
        Err(anyhow!("zama {method} did not print ZAMA_RESULT"))
    }
}
