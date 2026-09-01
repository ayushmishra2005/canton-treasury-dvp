use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::units::TokenUnits;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Accounts,
    Reserved,
    Locked,
    MintApproved,
    CantonMinted,
    TradePrepared,
    Reassigned,
    Settled,
    Redeemed,
    ReleaseApproved,
    Released,
    ZamaRedeemed,
}

impl Step {
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "accounts" => Self::Accounts,
            "reserved" => Self::Reserved,
            "locked" => Self::Locked,
            "mint_approved" => Self::MintApproved,
            "canton_minted" => Self::CantonMinted,
            "trade_prepared" => Self::TradePrepared,
            "reassigned" => Self::Reassigned,
            "settled" => Self::Settled,
            "redeemed" => Self::Redeemed,
            "release_approved" => Self::ReleaseApproved,
            "released" => Self::Released,
            "zama_redeemed" => Self::ZamaRedeemed,
            other => anyhow::bail!("unknown stop-after step {other}"),
        })
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Accounts => 1,
            Self::Reserved => 2,
            Self::Locked => 3,
            Self::MintApproved => 4,
            Self::CantonMinted => 5,
            Self::TradePrepared => 6,
            Self::Reassigned => 7,
            Self::Settled => 8,
            Self::Redeemed => 9,
            Self::ReleaseApproved => 10,
            Self::Released => 11,
            Self::ZamaRedeemed => 12,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Journal {
    pub operation_hex: String,
    pub reservation_hex: String,
    pub lock_id: String,
    pub completed: Option<Step>,
    pub mint: String,
    pub source: String,
    pub vault: String,
    pub payout_destination: String,
    pub refund_destination: String,
    pub decimals: u8,
    pub base_units: u64,
    pub canton_amount: String,
    pub mint_expiry: i64,
    pub mint_holding: String,
    pub seller_holding: String,
    pub buyer_treasury: String,
    pub lock_signature: String,
    pub release_signature: String,
    pub lock_proof_hex: String,
    pub release_expiry: i64,
    pub release_proof_hex: String,
    pub release_transfer_hex: String,
    pub release_equality: String,
    pub release_validity: String,
    pub release_range: String,
    #[serde(default)]
    pub fault_injected_chain_time: i64,
    #[serde(default)]
    pub fault_recovered_chain_time: i64,
}

impl Journal {
    pub fn reached(&self, step: Step) -> bool {
        self.completed
            .map(|done| done.rank() >= step.rank())
            .unwrap_or(false)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Secrets {
    pub payer: String,
    pub attester_a: String,
    pub attester_b: String,
    pub attester_c: String,
    pub source_authority: String,
    pub dest_authority: String,
    pub source_elgamal: String,
    pub source_aes: String,
    pub dest_elgamal: String,
    pub dest_aes: String,
    pub vault_elgamal: String,
    pub vault_aes: String,
    pub blinding: String,
}

pub struct OperationStore {
    pub root: PathBuf,
}

impl OperationStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        create_secret_dir(&root)?;
        Ok(Self { root })
    }

    pub fn journal_path(&self) -> PathBuf {
        self.root.join("journal.json")
    }

    pub fn secrets_path(&self) -> PathBuf {
        self.root.join("secrets.json")
    }

    pub fn load_journal(&self) -> Result<Option<Journal>> {
        let path = self.journal_path();
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn save_journal(&self, journal: &Journal) -> Result<()> {
        let path = self.journal_path();
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(journal)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn load_secrets(&self) -> Result<Option<Secrets>> {
        let path = self.secrets_path();
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn save_secrets(&self, secrets: &Secrets) -> Result<()> {
        let path = self.secrets_path();
        if path.exists() && is_world_readable(&path)? {
            return Err(anyhow!(
                "refusing to overwrite a world-readable secrets file"
            ));
        }
        write_private_file(&path, serde_json::to_string_pretty(secrets)?.as_bytes())?;
        Ok(())
    }
}

pub fn create_secret_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        } else if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .context("create secret parent")?;
            }
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .context("create secret directory")?;
        } else {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .context("create secret directory")?;
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).context("create journal directory")?;
    }
    Ok(())
}

pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<u32> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_secret_dir(parent)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    let tmp_mode = create_private_file(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let final_mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if final_mode & !0o600 != 0 {
            return Err(anyhow!(
                "secret file mode {final_mode:o} is wider than 0600"
            ));
        }
    }
    Ok(tmp_mode)
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .context("create private secret file")?;
        let mode = file.metadata()?.permissions().mode() & 0o777;
        if mode & !0o600 != 0 {
            return Err(anyhow!(
                "temporary secret file mode {mode:o} is wider than 0600"
            ));
        }
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        Ok(mode)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)?;
        Ok(0o600)
    }
}

fn is_world_readable(path: &Path) -> Result<bool> {
    let meta = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o004 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(false)
    }
}

pub fn encode_bytes(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

pub fn decode_bytes(text: &str) -> Result<Vec<u8>> {
    hex::decode(text).context("hex secret")
}

pub fn resume_matches_recorded_operation(
    journal: &Journal,
    amount: u64,
    payout: &str,
) -> Result<()> {
    if journal.operation_hex.is_empty() {
        return Ok(());
    }
    if journal.base_units != 0 && journal.base_units != amount {
        return Err(anyhow!(
            "resume amount does not match the recorded operation"
        ));
    }
    if !journal.canton_amount.is_empty() {
        let expected =
            TokenUnits::from_base_units(journal.base_units, journal.decimals)?.canton_decimal()?;
        if journal.canton_amount != expected {
            return Err(anyhow!(
                "resume Canton amount does not match the recorded operation"
            ));
        }
    }
    if !journal.payout_destination.is_empty() && journal.payout_destination != payout {
        return Err(anyhow!(
            "resume payout does not match the recorded operation"
        ));
    }
    Ok(())
}

pub fn units_from_journal(journal: &Journal) -> Result<TokenUnits> {
    TokenUnits::from_base_units(journal.base_units, journal.decimals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resume_rejects_changed_amount_or_payout() {
        let journal = Journal {
            operation_hex: "aa".repeat(32),
            base_units: 100_000_000_000,
            decimals: 6,
            canton_amount: "100000.000000".into(),
            payout_destination: "dest".into(),
            ..Journal::default()
        };
        resume_matches_recorded_operation(&journal, 100_000_000_000, "dest").unwrap();
        assert!(resume_matches_recorded_operation(&journal, 200_000_000_000, "dest").is_err());
        assert!(resume_matches_recorded_operation(&journal, 100_000_000_000, "other").is_err());
    }

    #[test]
    fn persist_and_resume_keep_the_same_operation() {
        let dir = tempdir().unwrap();
        let store = OperationStore::open(dir.path().to_path_buf()).unwrap();
        let journal = Journal {
            operation_hex: "aa".repeat(32),
            reservation_hex: "0xbb".to_string(),
            lock_id: "aa".repeat(32),
            completed: Some(Step::Locked),
            base_units: 1_000_000,
            decimals: 6,
            canton_amount: "1.000000".into(),
            ..Journal::default()
        };
        store.save_journal(&journal).unwrap();
        let loaded = store.load_journal().unwrap().unwrap();
        assert_eq!(loaded.operation_hex, journal.operation_hex);
        assert!(loaded.reached(Step::Locked));
        assert!(!loaded.reached(Step::CantonMinted));
    }

    #[test]
    fn secrets_are_written_with_restricted_mode() {
        let dir = tempdir().unwrap();
        let store = OperationStore::open(dir.path().to_path_buf()).unwrap();
        store
            .save_secrets(&Secrets {
                blinding: "00".repeat(32),
                ..Secrets::default()
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.secrets_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
    }

    #[test]
    fn secrets_never_wider_than_0600_under_umask_022() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let old = unsafe { libc::umask(0o022) };
            let dir = tempdir().unwrap();
            let secret_dir = dir.path().join("op-secrets");
            let written =
                write_private_file(&secret_dir.join("secrets.json"), br#"{"blinding":"00"}"#);
            unsafe { libc::umask(old) };
            let tmp_mode = written.unwrap();
            assert_eq!(tmp_mode, 0o600, "temporary file must be created at 0600");
            let dir_mode = fs::metadata(&secret_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "secret directory must be 0700");
            let final_mode = fs::metadata(secret_dir.join("secrets.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(final_mode, 0o600, "final secret file must stay 0600");
        }
    }
}
