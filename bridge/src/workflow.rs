use anyhow::{Context, Result};
use rand::RngCore;
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use zeroize::Zeroize;

use crate::attest::ed25519_approve_ix;
use crate::canonical::{
    amount_commitment, operation_digest, proof_commitment, DIRECTION_CANCEL, DIRECTION_MINT,
    DIRECTION_RELEASE,
};
use crate::canton::CantonClient;
use crate::confidential::generate_transfer_proofs;
use crate::journal::{encode_bytes, Journal, OperationStore, Secrets, Step};
use crate::program::{
    approve_ix, config_pda, lock_ix, move_ix, receipt_pda, ApproveFields, LockFields,
    MovementFields,
};
use crate::relayer::{RelayerClient, RelayerInstruction};
use crate::setup::{
    apply_pending, config_is_initialized, create_bridge_accounts, decode_aes, decode_elgamal,
    decode_keypair, decrypt_available, encode_aes, encode_elgamal, encode_keypair,
    read_mint_decimals, vault_elgamal_pubkey, BridgeAccounts, ConfidentialOwner, DECIMALS,
};
use crate::txsize::{report_size, serialize_legacy, LEGACY_LIMIT};
use crate::units::{require_mint_decimals, TokenUnits};
use crate::zama::ZamaClient;

pub struct Workflow {
    pub rpc: RpcClient,
    pub relayer: RelayerClient,
    pub zama: ZamaClient,
    pub canton: CantonClient,
    pub payer: Keypair,
    pub attester_a: Keypair,
    pub attester_b: Keypair,
    pub attester_c: Keypair,
    pub store: OperationStore,
    pub stop_after: Option<Step>,
    pub expiry_recovery: bool,
    pub reuse_from: Option<PathBuf>,
}

impl Workflow {
    pub fn run(&self, tokens: u64) -> Result<()> {
        self.run_full(tokens, true)
    }

    pub fn prove_relayer(&self, tokens: u64) -> Result<()> {
        self.run_full(tokens, false)
    }

    fn run_full(&self, tokens: u64, include_canton_zama: bool) -> Result<()> {
        let units = TokenUnits::from_whole_tokens(tokens, DECIMALS)?;
        let canton_amount = units.canton_decimal()?;
        let amount = units.base_units;
        let fee_payer: Pubkey = self.relayer.address()?.parse()?;
        crate::setup::airdrop(&self.rpc, &fee_payer)?;
        crate::setup::airdrop(&self.rpc, &self.payer.pubkey())?;

        let journal = self.store.load_journal()?.unwrap_or_default();
        let secrets = self.store.load_secrets()?;
        let (accounts, mut journal, secrets) =
            self.load_or_create_accounts(amount, journal, secrets)?;
        let attester_a = decode_keypair(&secrets.attester_a)?;
        let attester_b = decode_keypair(&secrets.attester_b)?;
        self.persist(&journal, &secrets)?;
        if self.halt(Step::Accounts, &mut journal)? {
            return Ok(());
        }

        let decimals = read_mint_decimals(&self.rpc, &accounts.mint)?;
        require_mint_decimals(decimals, DECIMALS)?;
        anyhow::ensure!(
            decimals == journal.decimals,
            "journal mint decimals changed"
        );

        let mut blinding = decode_blinding(&secrets.blinding)?;
        let commitment = amount_commitment(amount, &blinding);
        blinding.zeroize();
        let operation = decode_operation(&journal.operation_hex)?;
        let reservation = operation;
        let previous = [0u8; 32];
        let mint_expiry = if journal.mint_expiry == 0 {
            mint_deadline_from_env()
        } else {
            journal.mint_expiry
        };
        journal.mint_expiry = mint_expiry;
        self.persist(&journal, &secrets)?;
        let (config, _) = config_pda();
        let (receipt, _) = receipt_pda(&operation);
        let chain_id = self.rpc.get_genesis_hash()?.to_bytes();
        let reservation_hex = format!("0x{}", hex::encode(reservation));
        let lock_id = hex::encode(operation);
        journal.reservation_hex = reservation_hex.clone();
        journal.lock_id = lock_id.clone();
        self.persist(&journal, &secrets)?;

        if include_canton_zama {
            self.reserve_once(&reservation_hex, amount, &mut journal)?;
            if self.halt(Step::Reserved, &mut journal)? {
                return Ok(());
            }
        }

        if self.expiry_recovery {
            return self.run_expiry_recovery(
                &accounts,
                &fee_payer,
                amount,
                commitment,
                operation,
                reservation,
                previous,
                mint_expiry,
                &chain_id,
                &config,
                &receipt,
                &reservation_hex,
                &attester_a,
                &attester_b,
                &mut journal,
            );
        }

        self.lock_once(
            &accounts,
            &fee_payer,
            amount,
            commitment,
            operation,
            reservation,
            previous,
            mint_expiry,
            &mut journal,
        )?;
        if self.halt(Step::Locked, &mut journal)? {
            return Ok(());
        }

        let lock_proof = decode_proof32(&journal.lock_proof_hex).unwrap_or([0u8; 32]);
        anyhow::ensure!(
            lock_proof != [0u8; 32] || !include_canton_zama,
            "lock proof commitment is missing from the journal"
        );
        let mint_digest = operation_digest(
            &chain_id,
            &crate::program::PROGRAM_ID,
            &config,
            &operation,
            DIRECTION_MINT,
            &receipt,
            &accounts.source.token,
            &commitment,
            &reservation,
            &previous,
            mint_expiry,
            &lock_proof,
        );
        if !journal.reached(Step::MintApproved) {
            self.approve_twice(
                &fee_payer,
                &attester_a,
                &attester_b,
                &ApproveFields {
                    operation,
                    direction: DIRECTION_MINT,
                    destination: accounts.source.token,
                    amount_commitment: commitment,
                    zama_reservation: reservation,
                    previous_operation: previous,
                    expiry: mint_expiry,
                    proof_commitment: lock_proof,
                },
                &mint_digest,
            )?;
        }
        if self.halt(Step::MintApproved, &mut journal)? {
            return Ok(());
        }

        if include_canton_zama {
            if !journal.reached(Step::CantonMinted) {
                let holding =
                    self.canton
                        .mint(&lock_id, &canton_amount, &hex::encode(mint_digest))?;
                journal.mint_holding = holding;
                self.store.save_journal(&journal)?;
                println!("CANTON_MINT_HOLDING {}", journal.mint_holding);
                match self.zama.status(&reservation_hex) {
                    Ok(1) => self.zama.finalize(&reservation_hex)?,
                    Ok(2) | Ok(4) => {}
                    Ok(status) => anyhow::bail!("unexpected zama status {status} after mint"),
                    Err(err) => return Err(err),
                }
            }
            if self.halt(Step::CantonMinted, &mut journal)? {
                return Ok(());
            }
            if !journal.reached(Step::TradePrepared) {
                let trade = self.canton.prepare_trade(
                    &lock_id,
                    &canton_amount,
                    &hex::encode(mint_digest),
                )?;
                println!("CANTON_TRADE {trade}");
            }
            if self.halt(Step::TradePrepared, &mut journal)? {
                return Ok(());
            }
            if !journal.reached(Step::Reassigned) {
                self.canton.grant_reassignment()?;
                let evidence = self.canton.reassign()?;
                anyhow::ensure!(
                    evidence.contains("REASSIGNMENT_COMPLETE"),
                    "treasury reassignment did not complete"
                );
                println!("{evidence}");
                let _ = self.canton.revoke_reassignment();
            }
            if self.halt(Step::Reassigned, &mut journal)? {
                return Ok(());
            }
            if !journal.reached(Step::Settled) {
                let settled =
                    self.canton
                        .settle(&lock_id, &canton_amount, &hex::encode(mint_digest))?;
                anyhow::ensure!(
                    settled.payment_amount.contains("100000"),
                    "seller stablecoin amount"
                );
                anyhow::ensure!(
                    settled.treasury_amount.contains("100"),
                    "buyer treasury amount"
                );
                journal.seller_holding = settled.seller_stablecoin.clone();
                journal.buyer_treasury = settled.buyer_treasury.clone();
                self.store.save_journal(&journal)?;
                println!("DVP_BUYER_TREASURY {}", settled.buyer_treasury);
                println!("DVP_SELLER_STABLECOIN {}", settled.seller_stablecoin);
                println!("DVP_PAYMENT_AMOUNT {}", settled.payment_amount);
                println!("DVP_TREASURY_AMOUNT {}", settled.treasury_amount);
                if let Some(consumed) = settled.consumed_payment {
                    println!("DVP_CONSUMED_PAYMENT {consumed}");
                }
            }
            if self.halt(Step::Settled, &mut journal)? {
                return Ok(());
            }
            if !journal.reached(Step::Redeemed) {
                let redeemed = self.canton.redeem(
                    &lock_id,
                    &canton_amount,
                    &hex::encode(mint_digest),
                    &accounts.destination.token.to_string(),
                )?;
                println!("CANTON_REDEEM {redeemed}");
            }
            if self.halt(Step::Redeemed, &mut journal)? {
                return Ok(());
            }
        }

        self.release_once(
            &accounts,
            &fee_payer,
            &attester_a,
            &attester_b,
            amount,
            commitment,
            operation,
            reservation,
            previous,
            &chain_id,
            &config,
            &receipt,
            include_canton_zama,
            &reservation_hex,
            &mut journal,
        )?;
        Ok(())
    }

    fn load_or_create_accounts(
        &self,
        amount: u64,
        mut journal: Journal,
        secrets: Option<Secrets>,
    ) -> Result<(BridgeAccounts, Journal, Secrets)> {
        if let Some(secrets) = secrets {
            if !journal.mint.is_empty() && config_is_initialized(&self.rpc)? {
                let accounts = accounts_from_secrets(&journal, &secrets)?;
                return Ok((accounts, journal, secrets));
            }
        }
        if journal.mint.is_empty() {
            if let Some(prior) = &self.reuse_from {
                return self.reuse_existing_accounts(amount, prior);
            }
        }
        anyhow::ensure!(
            !config_is_initialized(&self.rpc).unwrap_or(false) || journal.mint.is_empty(),
            "bridge config already exists; resume the recorded journal"
        );
        let accounts = create_bridge_accounts(
            &self.rpc,
            &self.payer,
            [
                self.attester_a.pubkey(),
                self.attester_b.pubkey(),
                self.attester_c.pubkey(),
            ],
            amount,
        )?;
        let mut operation = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut operation);
        let mut blinding = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut blinding);
        journal.operation_hex = hex::encode(operation);
        journal.mint = accounts.mint.to_string();
        journal.source = accounts.source.token.to_string();
        journal.vault = accounts.vault.to_string();
        journal.refund_destination = accounts.source.token.to_string();
        journal.payout_destination = accounts.destination.token.to_string();
        journal.decimals = DECIMALS;
        journal.base_units = amount;
        journal.canton_amount = TokenUnits::from_base_units(amount, DECIMALS)?.canton_decimal()?;
        journal.mint_expiry = mint_deadline_from_env();
        let secrets = Secrets {
            payer: encode_keypair(&self.payer),
            attester_a: encode_keypair(&self.attester_a),
            attester_b: encode_keypair(&self.attester_b),
            attester_c: encode_keypair(&self.attester_c),
            source_authority: encode_keypair(&accounts.source.authority),
            dest_authority: encode_keypair(&accounts.destination.authority),
            source_elgamal: encode_elgamal(&accounts.source.elgamal),
            source_aes: encode_aes(&accounts.source.aes),
            dest_elgamal: encode_elgamal(&accounts.destination.elgamal),
            dest_aes: encode_aes(&accounts.destination.aes),
            vault_elgamal: encode_elgamal(&accounts.vault_elgamal),
            vault_aes: encode_aes(&accounts.vault_aes),
            blinding: encode_bytes(&blinding),
        };
        blinding.zeroize();
        Ok((accounts, journal, secrets))
    }

    fn reuse_existing_accounts(
        &self,
        amount: u64,
        prior: &std::path::Path,
    ) -> Result<(BridgeAccounts, Journal, Secrets)> {
        anyhow::ensure!(
            config_is_initialized(&self.rpc)?,
            "cannot reuse accounts before the bridge config exists"
        );
        let prior_store = OperationStore::open(prior.to_path_buf())?;
        let prior_journal = prior_store
            .load_journal()?
            .ok_or_else(|| anyhow::anyhow!("reuse-from journal is missing"))?;
        let mut secrets = prior_store
            .load_secrets()?
            .ok_or_else(|| anyhow::anyhow!("reuse-from secrets are missing"))?;
        let mut journal = Journal {
            mint: prior_journal.mint,
            source: prior_journal.source,
            vault: prior_journal.vault,
            refund_destination: prior_journal.refund_destination,
            payout_destination: prior_journal.payout_destination,
            decimals: prior_journal.decimals,
            base_units: amount,
            canton_amount: TokenUnits::from_base_units(amount, DECIMALS)?.canton_decimal()?,
            mint_expiry: mint_deadline_from_env(),
            ..Journal::default()
        };
        let mut operation = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut operation);
        let mut blinding = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut blinding);
        journal.operation_hex = hex::encode(operation);
        secrets.blinding = encode_bytes(&blinding);
        blinding.zeroize();
        let accounts = accounts_from_secrets(&journal, &secrets)?;
        Ok((accounts, journal, secrets))
    }

    fn reserve_once(
        &self,
        reservation_hex: &str,
        amount: u64,
        journal: &mut Journal,
    ) -> Result<()> {
        if journal.reached(Step::Reserved) {
            return Ok(());
        }
        match self.zama.status(reservation_hex) {
            Ok(0) | Err(_) => {
                if !self.zama.reserve(reservation_hex, amount)? {
                    anyhow::bail!(
                        "Zama rejected the reservation; no lock or mint will be attempted"
                    );
                }
            }
            Ok(status) if status >= 1 => {}
            Ok(status) => anyhow::bail!("unexpected zama reservation status {status}"),
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn lock_once(
        &self,
        accounts: &BridgeAccounts,
        fee_payer: &Pubkey,
        amount: u64,
        commitment: [u8; 32],
        operation: [u8; 32],
        reservation: [u8; 32],
        previous: [u8; 32],
        mint_expiry: i64,
        journal: &mut Journal,
    ) -> Result<()> {
        let (receipt, _) = receipt_pda(&operation);
        if journal.reached(Step::Locked) || self.rpc.get_account(&receipt).is_ok() {
            journal.completed = Some(Step::Locked.max_completed(journal.completed));
            return Ok(());
        }
        let source_before =
            decrypt_available(&self.rpc, &accounts.source.token, &accounts.source.aes)?;
        let vault_before = decrypt_available(&self.rpc, &accounts.vault, &accounts.vault_aes)?;
        anyhow::ensure!(
            source_before == amount,
            "source confidential balance before lock"
        );
        anyhow::ensure!(vault_before == 0, "vault confidential balance before lock");
        let lock_proofs = generate_transfer_proofs(
            &self.rpc,
            &self.payer,
            &accounts.source.elgamal,
            &accounts.source.aes,
            &accounts.source.token,
            &vault_elgamal_pubkey(accounts),
            amount,
        )?;
        let lock_proof = proof_commitment(
            &lock_proofs.decryptable,
            &lock_proofs.auditor_lo,
            &lock_proofs.auditor_hi,
            &lock_proofs.equality.pubkey(),
            &lock_proofs.validity.pubkey(),
            &lock_proofs.range.pubkey(),
        );
        journal.lock_proof_hex = hex::encode(lock_proof);
        self.store.save_journal(journal)?;
        let vault_decryptable = crate::confidential::decryptable_bytes(
            &accounts.vault_aes.encrypt(vault_before + amount),
        )?;
        let lock_fields = LockFields {
            operation,
            destination: accounts.source.token,
            amount_commitment: commitment,
            zama_reservation: reservation,
            previous_operation: previous,
            expiry: mint_expiry,
            transfer_data: lock_proofs.transfer_data,
            vault_decryptable,
        };
        let lock = lock_ix(
            *fee_payer,
            accounts.source.authority.pubkey(),
            accounts.source.token,
            accounts.vault,
            accounts.mint,
            spl_token_2022::id(),
            lock_proofs.equality.pubkey(),
            lock_proofs.validity.pubkey(),
            lock_proofs.range.pubkey(),
            &lock_fields,
        )?;
        measure_legacy(
            "lock_with_relayer_fee_payer",
            std::slice::from_ref(&lock),
            fee_payer,
        )?;
        let lock_sig = self.send_extra_signer(&[lock], &[&accounts.source.authority], fee_payer)?;
        self.confirm_on_chain(&lock_sig)?;
        journal.lock_signature = lock_sig;
        self.store.save_journal(journal)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn release_once(
        &self,
        accounts: &BridgeAccounts,
        fee_payer: &Pubkey,
        attester_a: &Keypair,
        attester_b: &Keypair,
        amount: u64,
        commitment: [u8; 32],
        operation: [u8; 32],
        reservation: [u8; 32],
        previous: [u8; 32],
        chain_id: &[u8; 32],
        config: &Pubkey,
        receipt: &Pubkey,
        include_canton_zama: bool,
        reservation_hex: &str,
        journal: &mut Journal,
    ) -> Result<()> {
        let dest_before = decrypt_available(
            &self.rpc,
            &accounts.destination.token,
            &accounts.destination.aes,
        )?;
        if !journal.reached(Step::Released) {
            let materials = self.release_materials(accounts, amount, journal)?;
            let approval_expiry = materials.expiry;
            let release_proof = materials.proof;
            let transfer_data = materials.transfer_data;
            let equality = materials.equality;
            let validity = materials.validity;
            let range = materials.range;
            let release_digest = operation_digest(
                chain_id,
                &crate::program::PROGRAM_ID,
                config,
                &operation,
                DIRECTION_RELEASE,
                receipt,
                &accounts.destination.token,
                &commitment,
                &reservation,
                &previous,
                approval_expiry,
                &release_proof,
            );
            let movement = MovementFields {
                destination: accounts.destination.token,
                amount_commitment: commitment,
                zama_reservation: reservation,
                previous_operation: previous,
                expiry: approval_expiry,
                transfer_data,
            };
            let release = move_ix(
                "release_confidential",
                accounts.vault,
                accounts.destination.token,
                accounts.mint,
                spl_token_2022::id(),
                equality,
                validity,
                range,
                &operation,
                DIRECTION_RELEASE,
                &movement,
            )?;
            if !journal.reached(Step::ReleaseApproved) {
                self.approve_twice(
                    fee_payer,
                    attester_a,
                    attester_b,
                    &ApproveFields {
                        operation,
                        direction: DIRECTION_RELEASE,
                        destination: accounts.destination.token,
                        amount_commitment: commitment,
                        zama_reservation: reservation,
                        previous_operation: previous,
                        expiry: approval_expiry,
                        proof_commitment: release_proof,
                    },
                    &release_digest,
                )?;
            }
            if self.halt(Step::ReleaseApproved, journal)? {
                return Ok(());
            }
            measure_legacy(
                "release_with_relayer_fee_payer",
                std::slice::from_ref(&release),
                fee_payer,
            )?;
            let release_id = self.relayer.send_instructions(&[to_relayer(&release)])?;
            let release_sig = self
                .relayer
                .wait_confirmed(&release_id, Duration::from_secs(90))?;
            self.confirm_on_chain(&release_sig)?;
            journal.release_signature = release_sig.clone();
            apply_pending(
                &self.rpc,
                &self.payer,
                &accounts.destination.token,
                &accounts.destination.authority,
                &accounts.destination.aes,
                dest_before + amount,
                1,
            )?;
            self.inspect_public_leak(&release, amount)?;
            println!("RELAYER_RELEASE_CONFIRMED {release_sig}");
        }
        if self.halt(Step::Released, journal)? {
            return Ok(());
        }
        if include_canton_zama && !journal.reached(Step::ZamaRedeemed) {
            match self.zama.status(reservation_hex) {
                Ok(4) => {}
                _ => self.zama.redeem(reservation_hex)?,
            }
            println!("ZAMA_REDEEM_OK {reservation_hex}");
        }
        let _ = self.halt(Step::ZamaRedeemed, journal)?;
        Ok(())
    }

    fn release_materials(
        &self,
        accounts: &BridgeAccounts,
        amount: u64,
        journal: &mut Journal,
    ) -> Result<ReleaseMaterials> {
        if journal.release_expiry > 0 && !journal.release_transfer_hex.is_empty() {
            return Ok(ReleaseMaterials {
                expiry: journal.release_expiry,
                proof: decode_proof32(&journal.release_proof_hex)?,
                transfer_data: decode_transfer(&journal.release_transfer_hex)?,
                equality: Pubkey::from_str(&journal.release_equality)?,
                validity: Pubkey::from_str(&journal.release_validity)?,
                range: Pubkey::from_str(&journal.release_range)?,
            });
        }
        let release_proofs = generate_transfer_proofs(
            &self.rpc,
            &self.payer,
            &accounts.vault_elgamal,
            &accounts.vault_aes,
            &accounts.vault,
            accounts.destination.elgamal.pubkey(),
            amount,
        )?;
        let release_proof = proof_commitment(
            &release_proofs.decryptable,
            &release_proofs.auditor_lo,
            &release_proofs.auditor_hi,
            &release_proofs.equality.pubkey(),
            &release_proofs.validity.pubkey(),
            &release_proofs.range.pubkey(),
        );
        journal.release_expiry = chrono::Utc::now().timestamp() + 3600;
        journal.release_proof_hex = hex::encode(release_proof);
        journal.release_transfer_hex = hex::encode(release_proofs.transfer_data);
        journal.release_equality = release_proofs.equality.pubkey().to_string();
        journal.release_validity = release_proofs.validity.pubkey().to_string();
        journal.release_range = release_proofs.range.pubkey().to_string();
        self.store.save_journal(journal)?;
        Ok(ReleaseMaterials {
            expiry: journal.release_expiry,
            proof: release_proof,
            transfer_data: release_proofs.transfer_data,
            equality: release_proofs.equality.pubkey(),
            validity: release_proofs.validity.pubkey(),
            range: release_proofs.range.pubkey(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_expiry_recovery(
        &self,
        accounts: &BridgeAccounts,
        fee_payer: &Pubkey,
        amount: u64,
        commitment: [u8; 32],
        operation: [u8; 32],
        reservation: [u8; 32],
        previous: [u8; 32],
        mint_expiry: i64,
        chain_id: &[u8; 32],
        config: &Pubkey,
        receipt: &Pubkey,
        reservation_hex: &str,
        attester_a: &Keypair,
        attester_b: &Keypair,
        journal: &mut Journal,
    ) -> Result<()> {
        self.lock_once(
            accounts,
            fee_payer,
            amount,
            commitment,
            operation,
            reservation,
            previous,
            mint_expiry,
            journal,
        )?;
        let now = chrono::Utc::now().timestamp();
        if now < mint_expiry {
            std::thread::sleep(Duration::from_secs((mint_expiry - now + 1) as u64));
        }
        let lock_proof = decode_proof32(&journal.lock_proof_hex).unwrap_or([0u8; 32]);
        let mint_digest = operation_digest(
            chain_id,
            &crate::program::PROGRAM_ID,
            config,
            &operation,
            DIRECTION_MINT,
            receipt,
            &accounts.source.token,
            &commitment,
            &reservation,
            &previous,
            mint_expiry,
            &lock_proof,
        );
        let mint_result = self.approve_twice(
            fee_payer,
            attester_a,
            attester_b,
            &ApproveFields {
                operation,
                direction: DIRECTION_MINT,
                destination: accounts.source.token,
                amount_commitment: commitment,
                zama_reservation: reservation,
                previous_operation: previous,
                expiry: mint_expiry,
                proof_commitment: lock_proof,
            },
            &mint_digest,
        );
        anyhow::ensure!(
            mint_result.is_err(),
            "mint approval must fail after the original deadline"
        );
        println!("EXPIRY_RECOVERY_MINT_REJECTED");
        let cancel_expiry = chrono::Utc::now().timestamp() + 3600;
        let cancel_proofs = generate_transfer_proofs(
            &self.rpc,
            &self.payer,
            &accounts.vault_elgamal,
            &accounts.vault_aes,
            &accounts.vault,
            accounts.source.elgamal.pubkey(),
            amount,
        )?;
        let cancel_proof = proof_commitment(
            &cancel_proofs.decryptable,
            &cancel_proofs.auditor_lo,
            &cancel_proofs.auditor_hi,
            &cancel_proofs.equality.pubkey(),
            &cancel_proofs.validity.pubkey(),
            &cancel_proofs.range.pubkey(),
        );
        let cancel_digest = operation_digest(
            chain_id,
            &crate::program::PROGRAM_ID,
            config,
            &operation,
            DIRECTION_CANCEL,
            receipt,
            &accounts.source.token,
            &commitment,
            &reservation,
            &previous,
            cancel_expiry,
            &cancel_proof,
        );
        self.approve_twice(
            fee_payer,
            attester_a,
            attester_b,
            &ApproveFields {
                operation,
                direction: DIRECTION_CANCEL,
                destination: accounts.source.token,
                amount_commitment: commitment,
                zama_reservation: reservation,
                previous_operation: previous,
                expiry: cancel_expiry,
                proof_commitment: cancel_proof,
            },
            &cancel_digest,
        )?;
        let movement = MovementFields {
            destination: accounts.source.token,
            amount_commitment: commitment,
            zama_reservation: reservation,
            previous_operation: previous,
            expiry: cancel_expiry,
            transfer_data: cancel_proofs.transfer_data,
        };
        let cancel = move_ix(
            "cancel_confidential",
            accounts.vault,
            accounts.source.token,
            accounts.mint,
            spl_token_2022::id(),
            cancel_proofs.equality.pubkey(),
            cancel_proofs.validity.pubkey(),
            cancel_proofs.range.pubkey(),
            &operation,
            DIRECTION_CANCEL,
            &movement,
        )?;
        measure_legacy(
            "cancel_with_relayer_fee_payer",
            std::slice::from_ref(&cancel),
            fee_payer,
        )?;
        let id = self.relayer.send_instructions(&[to_relayer(&cancel)])?;
        let sig = self.relayer.wait_confirmed(&id, Duration::from_secs(90))?;
        self.confirm_on_chain(&sig)?;
        apply_pending(
            &self.rpc,
            &self.payer,
            &accounts.source.token,
            &accounts.source.authority,
            &accounts.source.aes,
            amount,
            1,
        )?;
        match self.zama.status(reservation_hex) {
            Ok(1) => self.zama.cancel(reservation_hex)?,
            Ok(3) => {}
            Ok(status) => anyhow::bail!("unexpected zama status {status} after expiry cancel"),
            Err(err) => return Err(err),
        }
        println!("EXPIRY_RECOVERY_CANCEL_CONFIRMED {sig}");
        println!("ZAMA_CANCEL_OK {reservation_hex}");
        println!("EXPIRY_RECOVERY_COMPLETE");
        Ok(())
    }

    fn persist(&self, journal: &Journal, secrets: &Secrets) -> Result<()> {
        self.store.save_journal(journal)?;
        self.store.save_secrets(secrets)?;
        Ok(())
    }

    fn halt(&self, step: Step, journal: &mut Journal) -> Result<bool> {
        if !journal.reached(step) {
            journal.completed = Some(step);
            self.store.save_journal(journal)?;
        }
        Ok(self.stop_after == Some(step))
    }

    fn approve_twice(
        &self,
        fee_payer: &Pubkey,
        attester_a: &Keypair,
        attester_b: &Keypair,
        args: &ApproveFields,
        digest: &[u8; 32],
    ) -> Result<()> {
        let approve = approve_ix(*fee_payer, args)?;
        measure_legacy(
            "approve_with_relayer_fee_payer",
            &[ed25519_approve_ix(attester_a, digest)?, approve.clone()],
            fee_payer,
        )?;
        for attester in [attester_a, attester_b] {
            let ed = ed25519_approve_ix(attester, digest)?;
            let ix = approve_ix(*fee_payer, args)?;
            let id = self
                .relayer
                .send_instructions(&[to_relayer(&ed), to_relayer(&ix)])?;
            let sig = self.relayer.wait_confirmed(&id, Duration::from_secs(60))?;
            self.confirm_on_chain(&sig)?;
        }
        Ok(())
    }

    fn send_extra_signer(
        &self,
        instructions: &[Instruction],
        extra: &[&Keypair],
        fee_payer: &Pubkey,
    ) -> Result<String> {
        let blockhash = self.rpc.get_latest_blockhash()?;
        let message = Message::new_with_blockhash(instructions, Some(fee_payer), &blockhash);
        let mut transaction = Transaction::new_unsigned(message);
        transaction.partial_sign(extra, blockhash);
        let bytes = bincode::serialize(&transaction)?;
        report_size("encoded_relayer_fee_payer", bytes.len());
        anyhow::ensure!(
            bytes.len() <= LEGACY_LIMIT,
            "encoded transaction is {} bytes and exceeds the 1232-byte legacy limit",
            bytes.len()
        );
        let id = self.relayer.send_transaction(&bytes)?;
        self.relayer.wait_confirmed(&id, Duration::from_secs(90))
    }

    fn confirm_on_chain(&self, signature: &str) -> Result<()> {
        let sig = signature.parse().context("signature")?;
        let started = std::time::Instant::now();
        loop {
            let statuses = self.rpc.get_signature_statuses(&[sig])?;
            if let Some(Some(status)) = statuses.value.first() {
                if let Some(err) = &status.err {
                    anyhow::bail!("solana transaction {signature} failed: {err}");
                }
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(45) {
                anyhow::bail!("solana did not confirm {signature}");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn inspect_public_leak(&self, release: &Instruction, amount: u64) -> Result<()> {
        let amount_bytes = amount.to_le_bytes();
        anyhow::ensure!(
            !release.data.windows(8).any(|window| window == amount_bytes),
            "plaintext amount found in public release instruction data"
        );
        Ok(())
    }
}

struct ReleaseMaterials {
    expiry: i64,
    proof: [u8; 32],
    transfer_data: [u8; 167],
    equality: Pubkey,
    validity: Pubkey,
    range: Pubkey,
}

fn measure_legacy(label: &str, instructions: &[Instruction], fee_payer: &Pubkey) -> Result<()> {
    let (_tx, size) = serialize_legacy(instructions, fee_payer)?;
    report_size(label, size);
    anyhow::ensure!(
        size <= LEGACY_LIMIT,
        "{label} is {size} bytes and exceeds the 1232-byte legacy limit"
    );
    Ok(())
}

fn to_relayer(ix: &Instruction) -> RelayerInstruction {
    RelayerInstruction {
        program_id: ix.program_id.to_string(),
        accounts: ix
            .accounts
            .iter()
            .map(|meta| (meta.pubkey.to_string(), meta.is_signer, meta.is_writable))
            .collect(),
        data: ix.data.clone(),
    }
}

fn mint_deadline_from_env() -> i64 {
    let secs = std::env::var("BRIDGE_MINT_EXPIRY_SECS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(3600);
    chrono::Utc::now().timestamp() + secs.max(5)
}

fn decode_operation(hex_text: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_text).context("operation hex")?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("operation must be 32 bytes"))
}

fn decode_blinding(hex_text: &str) -> Result<[u8; 32]> {
    decode_proof32(hex_text)
}

fn decode_proof32(hex_text: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_text).context("32-byte hex")?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("value must be 32 bytes"))
}

fn decode_transfer(hex_text: &str) -> Result<[u8; 167]> {
    let bytes = hex::decode(hex_text).context("transfer hex")?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("transfer data must be 167 bytes"))
}

fn accounts_from_secrets(journal: &Journal, secrets: &Secrets) -> Result<BridgeAccounts> {
    Ok(BridgeAccounts {
        mint: Pubkey::from_str(&journal.mint)?,
        source: ConfidentialOwner {
            authority: decode_keypair(&secrets.source_authority)?,
            token: Pubkey::from_str(&journal.source)?,
            elgamal: decode_elgamal(&secrets.source_elgamal)?,
            aes: decode_aes(&secrets.source_aes)?,
        },
        vault: Pubkey::from_str(&journal.vault)?,
        vault_elgamal: decode_elgamal(&secrets.vault_elgamal)?,
        vault_aes: decode_aes(&secrets.vault_aes)?,
        destination: ConfidentialOwner {
            authority: decode_keypair(&secrets.dest_authority)?,
            token: Pubkey::from_str(&journal.payout_destination)?,
            elgamal: decode_elgamal(&secrets.dest_elgamal)?,
            aes: decode_aes(&secrets.dest_aes)?,
        },
    })
}

impl Step {
    fn max_completed(self, current: Option<Step>) -> Step {
        match current {
            Some(done) if done.rank() > self.rank() => done,
            _ => self,
        }
    }
}
