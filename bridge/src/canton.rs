use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
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
        self.run("Tests.Bridge.Runtime:prepare", "prepare", 0, "prepare")
    }

    pub fn mint(&self, lock_id: &str, amount: u64, digest_hex: &str) -> Result<()> {
        self.run("Tests.Bridge.Runtime:mint", lock_id, amount, digest_hex)
    }

    pub fn redeem(&self, lock_id: &str, amount: u64, digest_hex: &str) -> Result<()> {
        self.run("Tests.Bridge.Runtime:redeem", lock_id, amount, digest_hex)
    }

    fn run(&self, name: &str, lock_id: &str, amount: u64, digest_hex: &str) -> Result<()> {
        let input = self.write_input(lock_id, amount, digest_hex)?;
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
        if !output.status.success() {
            return Err(anyhow!(
                "canton {name} failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn write_input(&self, lock_id: &str, amount: u64, digest_hex: &str) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!("bridge-canton-{lock_id}.json"));
        std::fs::write(
            &path,
            serde_json::json!({
                "lockId": lock_id,
                "amount": amount,
                "digestHex": digest_hex,
            })
            .to_string(),
        )?;
        Ok(path)
    }
}
