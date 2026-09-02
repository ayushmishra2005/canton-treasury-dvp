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
use crate::canton::{require_canton_ledger_evidence, CantonClient, CantonLedgerExpectation};
use crate::confidential::generate_transfer_proofs;
use crate::journal::{
    encode_bytes, resume_matches_recorded_operation, Journal, OperationStore, Secrets, Step,
};
use crate::program::{
    approval_pda, approve_ix, config_pda, lock_ix, move_ix, receipt_pda, ApproveFields, LockFields,
    MovementFields,
};
use crate::recovery::{
    approval_expired_on_chain, attesters_needed, completed_operation_decision, decode_approval,
    decode_chain_clock, decode_receipt_status, should_apply_pending,
    should_refresh_release_materials, should_submit_release, CompletionDecision, OnChainApproval,
    RECEIPT_CANCELLED, RECEIPT_MINT_AUTHORIZED, RECEIPT_RELEASED,
};
use crate::relayer::{RelayerClient, RelayerInstruction};
use crate::reservation::{
    reservation_resume, ReservationAction, RESERVATION_FINALIZED, RESERVATION_REDEEMED,
    RESERVATION_RESERVED,
};
use crate::setup::{
    apply_pending, config_is_initialized, create_bridge_accounts, decode_aes, decode_elgamal,
    decode_keypair, decrypt_available, encode_aes, encode_elgamal, encode_keypair,
    pending_credit_counter, read_mint_decimals, vault_elgamal_pubkey, BridgeAccounts,
    ConfidentialOwner, DECIMALS,
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
    pub omit_journal_save: bool,
    pub halt_after_first_approval: bool,
    pub inject_attester_disagreement: bool,
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

        let journal = self.store.load_journal()?.unwrap_or_default();
        let secrets = self.store.load_secrets()?;
        if include_canton_zama {
            if let Some(mut completed) =
                self.try_completed_resume(amount, journal.clone(), secrets.clone())?
            {
                println!("COMPLETED_RESUME_SKIP_SETUP");
                if let Ok(count) = self.rpc.get_transaction_count() {
                    println!("SOLANA_TX_COUNT {count}");
                }
                self.record_completed_operation(
                    &completed.accounts,
                    amount,
                    &completed.receipt,
                    &completed.reservation_hex,
                    &mut completed.journal,
                )?;
                return Ok(());
            }
        }

        crate::setup::airdrop(&self.rpc, &fee_payer)?;
        crate::setup::airdrop(&self.rpc, &self.payer.pubkey())?;

        let (accounts, mut journal, secrets) =
            self.load_or_create_accounts(amount, journal, secrets)?;
        resume_matches_recorded_operation(
            &journal,
            amount,
            &accounts.destination.token.to_string(),
        )?;
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
            self.mint_deadline()?
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
            self.canton.prepare()?;
            match self.reserve_once(&reservation_hex, amount)? {
                ReservationAction::RecordCompleted => {
                    self.record_completed_operation(
                        &accounts,
                        amount,
                        &receipt,
                        &reservation_hex,
                        &mut journal,
                    )?;
                    return Ok(());
                }
                ReservationAction::SubmitReserve | ReservationAction::ResumeApproved => {}
            }
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
        let mint_receipt = self.read_receipt_status(&receipt)?;
        if mint_receipt != Some(RECEIPT_MINT_AUTHORIZED) && mint_receipt != Some(RECEIPT_RELEASED) {
            self.approve_needed(
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
        if let Some(approval) = self.read_approval(&approval_pda(&operation, DIRECTION_MINT).0)? {
            println!("MINT_APPROVAL_BITMAP {}", approval.signer_bitmap);
        }
        if let Some(status) = self.read_receipt_status(&receipt)? {
            println!("RECEIPT_STATUS {status}");
        }
        if self.halt_after_first_approval {
            return Ok(());
        }
        if self.inject_attester_disagreement {
            self.mark_fault_injected(&mut journal, "attester_disagreement", amount)?;
            if let Some(approval) =
                self.read_approval(&approval_pda(&operation, DIRECTION_MINT).0)?
            {
                anyhow::ensure!(
                    approval.signer_bitmap.count_ones() < 2,
                    "conflicting attestation must not reach quorum"
                );
                println!("MINT_APPROVAL_BITMAP {}", approval.signer_bitmap);
            }
            println!("RECOVERY_RESULT no_mint_without_quorum");
            println!("TERMINAL_STATE locked_awaiting_quorum");
            println!("ACTION_COUNTS mint=0 settle=0 burn=0 release=0 zama=0");
            return Ok(());
        }
        if !journal.reached(Step::MintApproved) {
            self.note_fault_recovered(&mut journal)?;
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
                let zama_status = self.zama.status(&reservation_hex)?;
                anyhow::ensure!(
                    self.zama.approved(&reservation_hex)?,
                    "cannot finalize a rejected reservation"
                );
                match zama_status {
                    1 => self.zama.finalize(&reservation_hex)?,
                    2 | 4 => {}
                    status => anyhow::bail!("unexpected zama status {status} after mint"),
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
            if !self.omit_journal_save {
                self.store.save_journal(&journal)?;
            }
            println!("DVP_BUYER_TREASURY {}", settled.buyer_treasury);
            println!("DVP_SELLER_STABLECOIN {}", settled.seller_stablecoin);
            println!("DVP_PAYMENT_AMOUNT {}", settled.payment_amount);
            println!("DVP_TREASURY_AMOUNT {}", settled.treasury_amount);
            if let Some(consumed) = settled.consumed_payment {
                println!("DVP_CONSUMED_PAYMENT {consumed}");
            }
            if self.halt(Step::Settled, &mut journal)? {
                return Ok(());
            }
            let redeemed = self.canton.redeem(
                &lock_id,
                &canton_amount,
                &hex::encode(mint_digest),
                &accounts.destination.token.to_string(),
            )?;
            println!("CANTON_REDEEM {redeemed}");
            if self.halt(Step::Redeemed, &mut journal)? {
                self.mark_fault_injected(&mut journal, "delayed_release_after_redemption", amount)?;
                println!("TERMINAL_STATE canton_redeemed_solana_locked");
                println!("ACTION_COUNTS mint=1 settle=1 burn=1 release=0 zama=0");
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
            !config_is_initialized(&self.rpc)? || journal.mint.is_empty(),
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
        journal.mint_expiry = self.mint_deadline()?;
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
            mint_expiry: self.mint_deadline()?,
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

    fn try_completed_resume(
        &self,
        amount: u64,
        journal: Journal,
        secrets: Option<Secrets>,
    ) -> Result<Option<CompletedResume>> {
        let Some(secrets) = secrets else {
            return Ok(None);
        };
        if journal.operation_hex.is_empty() || journal.mint.is_empty() {
            return Ok(None);
        }
        if !config_is_initialized(&self.rpc)? {
            return Ok(None);
        }
        resume_matches_recorded_operation(&journal, amount, &journal.payout_destination)?;
        let accounts = accounts_from_secrets(&journal, &secrets)?;
        let reservation_hex = if journal.reservation_hex.is_empty() {
            format!("0x{}", journal.operation_hex)
        } else {
            journal.reservation_hex.clone()
        };
        let status = self.zama.status(&reservation_hex);
        let approved = match &status {
            Ok(status)
                if *status == RESERVATION_RESERVED
                    || *status == RESERVATION_FINALIZED
                    || *status == RESERVATION_REDEEMED =>
            {
                Some(self.zama.approved(&reservation_hex))
            }
            _ => None,
        };
        match reservation_resume(status, approved) {
            Ok(ReservationAction::RecordCompleted) => {
                let operation = decode_operation(&journal.operation_hex)?;
                let (receipt, _) = receipt_pda(&operation);
                Ok(Some(CompletedResume {
                    accounts,
                    journal,
                    receipt,
                    reservation_hex,
                }))
            }
            _ => Ok(None),
        }
    }

    fn reserve_once(&self, reservation_hex: &str, amount: u64) -> Result<ReservationAction> {
        let status = self.zama.status(reservation_hex);
        let approved = match &status {
            Ok(status)
                if *status == RESERVATION_RESERVED
                    || *status == RESERVATION_FINALIZED
                    || *status == RESERVATION_REDEEMED =>
            {
                Some(self.zama.approved(reservation_hex))
            }
            _ => None,
        };
        match reservation_resume(status, approved) {
            Ok(action) => {
                if action == ReservationAction::SubmitReserve
                    && !self.zama.reserve(reservation_hex, amount)?
                {
                    println!("ZAMA_RESERVATION_REJECTED");
                    anyhow::bail!(
                        "Zama rejected the reservation; no lock or mint will be attempted"
                    );
                }
                Ok(action)
            }
            Err(err) => {
                if err.to_string().contains("rejected") {
                    println!("ZAMA_RESERVATION_REJECTED");
                }
                Err(err)
            }
        }
    }

    fn record_completed_operation(
        &self,
        accounts: &BridgeAccounts,
        amount: u64,
        receipt: &Pubkey,
        reservation_hex: &str,
        journal: &mut Journal,
    ) -> Result<()> {
        let expected = CantonLedgerExpectation {
            lock_id: journal.lock_id.clone(),
            canton_amount: journal.canton_amount.clone(),
            treasury_amount: "100.000000".to_string(),
            payout_destination: journal.payout_destination.clone(),
            mint_holding: journal.mint_holding.clone(),
        };
        let evidence =
            require_canton_ledger_evidence(self.canton.verify_completion(&expected), &expected)?;
        println!("CANTON_VERIFY_LOCK {}", evidence.lock_id);
        println!("CANTON_VERIFY_MINT_HOLDING {}", evidence.mint_holding);
        println!("CANTON_VERIFY_MINT_CONSUMED {}", evidence.mint_holding);
        println!(
            "CANTON_VERIFY_PAYMENT_ALLOCATION {}",
            evidence.payment_allocation
        );
        println!("CANTON_VERIFY_PAYMENT_LOCKED {}", evidence.payment_locked);
        println!("CANTON_VERIFY_ALLOCATE_UPDATE {}", evidence.allocate_update);
        println!("CANTON_VERIFY_SETTLE_UPDATE {}", evidence.settle_update);
        println!("CANTON_VERIFY_BUYER_TREASURY {}", evidence.buyer_treasury);
        println!("CANTON_VERIFY_SELLER_PAYMENT {}", evidence.seller_payment);
        println!("CANTON_VERIFY_SELLER_BURN {}", evidence.seller_payment);
        println!("CANTON_VERIFY_REDEEM {}", evidence.redeemed_lock);
        println!("CANTON_VERIFY_REDEEM_UPDATE {}", evidence.redeem_update);
        println!("CANTON_VERIFY_PAYOUT {}", evidence.payout_destination);
        println!("CANTON_VERIFY_PAYMENT_AMOUNT {}", evidence.payment_amount);
        println!("CANTON_VERIFY_TREASURY_AMOUNT {}", evidence.treasury_amount);
        println!("CANTON_VERIFY_INSTRUMENT {}", evidence.instrument_id);
        println!("CANTON_VERIFY_PAYMENT_ADMIN {}", evidence.payment_admin);
        println!(
            "CANTON_VERIFY_TREASURY_INSTRUMENT {}",
            evidence.treasury_instrument
        );
        println!("CANTON_VERIFY_TREASURY_ADMIN {}", evidence.treasury_admin);
        println!("CANTON_VERIFY_BUYER {}", evidence.buyer);
        println!("CANTON_VERIFY_SELLER {}", evidence.seller);
        println!("CANTON_VERIFY_BINDING_TRADE {}", evidence.trade_cid);
        println!("CANTON_VERIFY_OK");
        let decision = completed_operation_decision(
            self.zama.status(reservation_hex),
            Some(self.zama.approved(reservation_hex)),
            self.read_receipt_status(receipt),
            pending_credit_counter(&self.rpc, &accounts.destination.token),
            decrypt_available(
                &self.rpc,
                &accounts.destination.token,
                &accounts.destination.aes,
            ),
            amount,
            evidence.settle_seen && evidence.mint_consumed,
            evidence.redeem_seen && evidence.seller_burned,
            accounts.destination.token.to_string() == journal.payout_destination
                && evidence.payout_destination == journal.payout_destination,
        )?;
        anyhow::ensure!(
            decision == CompletionDecision::RecordAndExit,
            "completed reservation did not present matching recovery evidence"
        );
        if let Some(status) = self.read_receipt_status(receipt)? {
            println!("RECEIPT_STATUS {status}");
        }
        println!(
            "DEST_PENDING {}",
            pending_credit_counter(&self.rpc, &accounts.destination.token)?
        );
        println!(
            "DEST_AVAILABLE {}",
            decrypt_available(
                &self.rpc,
                &accounts.destination.token,
                &accounts.destination.aes,
            )?
        );
        println!("ZAMA_STATUS {RESERVATION_REDEEMED}");
        println!("OPERATION_RECORDED_COMPLETE {reservation_hex}");
        let _ = self.halt(Step::ZamaRedeemed, journal)?;
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
        if journal.reached(Step::Locked) {
            return Ok(());
        }
        if self.read_receipt_status(&receipt)?.is_some() {
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
            vault_elgamal_pubkey(accounts),
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
        let vault_decryptable = accounts.vault_aes.encrypt(vault_before + amount)?;
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
        anyhow::ensure!(
            accounts.destination.token.to_string() == journal.payout_destination,
            "payout destination changed"
        );
        if include_canton_zama {
            anyhow::ensure!(
                journal.reached(Step::Redeemed),
                "release requires redemption evidence"
            );
        }
        let receipt_status = self.read_receipt_status(receipt)?;
        if let Some(status) = receipt_status {
            println!("RECEIPT_STATUS {status}");
            anyhow::ensure!(
                status != RECEIPT_CANCELLED,
                "receipt was cancelled; release is not allowed"
            );
        }
        let chain_now = self.chain_unix_timestamp()?;
        println!("CHAIN_CLOCK {chain_now}");
        let (approval_pda, _) = approval_pda(&operation, DIRECTION_RELEASE);
        let on_chain = self.read_approval(&approval_pda)?;
        if let Some(approval) = &on_chain {
            println!("RELEASE_ONCHAIN_EXPIRY {}", approval.expiry);
            println!("RELEASE_ONCHAIN_CONSUMED {}", approval.consumed);
        }
        if should_refresh_release_materials(
            chain_now,
            on_chain.as_ref(),
            journal.release_expiry,
            receipt_status,
        ) {
            println!("FAULT_INJECTED expiry_after_settlement");
            println!("LOCKED_VALUE {amount}");
            println!("LOCKED_STATE solana_vault");
            if let Some(approval) = &on_chain {
                println!(
                    "RECOVERY_DURATION_CHAIN_SECS {}",
                    chain_now.saturating_sub(approval.expiry)
                );
            }
            println!("RELEASE_REFRESHED_AFTER_CHAIN_EXPIRY");
            journal.release_expiry = 0;
            journal.release_proof_hex.clear();
            journal.release_transfer_hex.clear();
            journal.release_equality.clear();
            journal.release_validity.clear();
            journal.release_range.clear();
        } else if on_chain.as_ref().is_some_and(|approval| {
            !approval.consumed && !approval_expired_on_chain(approval.expiry, chain_now)
        }) {
            println!("RELEASE_REUSING_ONCHAIN_APPROVAL");
        }
        if should_submit_release(journal.reached(Step::Released), receipt_status) {
            let materials = self.release_materials(accounts, amount, chain_now, journal)?;
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
            self.approve_needed(
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
            if self.halt_after_first_approval {
                return Ok(());
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
            self.inspect_public_leak(&release, amount)?;
            println!("RELAYER_RELEASE_CONFIRMED {release_sig}");
            self.note_fault_recovered(journal)?;
            println!("RECOVERY_RESULT released");
            println!("TERMINAL_STATE released");
            println!("ACTION_COUNTS mint=1 settle=1 burn=1 release=1 zama=0");
            if let Some(status) = self.read_receipt_status(receipt)? {
                println!("RECEIPT_STATUS {status}");
                anyhow::ensure!(
                    status == RECEIPT_RELEASED,
                    "release confirmed but receipt status is {status}"
                );
            }
        }
        let dest_pending = pending_credit_counter(&self.rpc, &accounts.destination.token)?;
        let dest_available = decrypt_available(
            &self.rpc,
            &accounts.destination.token,
            &accounts.destination.aes,
        )?;
        println!("DEST_PENDING {dest_pending}");
        println!("DEST_AVAILABLE {dest_available}");
        if should_apply_pending(dest_pending, dest_available, amount)? {
            apply_pending(
                &self.rpc,
                &self.payer,
                &accounts.destination.token,
                &accounts.destination.authority,
                &accounts.destination.aes,
                amount,
                dest_pending,
            )?;
            let dest_after = decrypt_available(
                &self.rpc,
                &accounts.destination.token,
                &accounts.destination.aes,
            )?;
            anyhow::ensure!(
                dest_after == amount,
                "destination available {dest_after} after apply-pending"
            );
            println!(
                "DEST_PENDING {}",
                pending_credit_counter(&self.rpc, &accounts.destination.token)?
            );
            println!("DEST_AVAILABLE {dest_after}");
        }
        if self.halt(Step::Released, journal)? {
            return Ok(());
        }
        if include_canton_zama && !journal.reached(Step::ZamaRedeemed) {
            match self.zama.status(reservation_hex) {
                Ok(RESERVATION_REDEEMED) => {}
                Ok(RESERVATION_FINALIZED) => self.zama.redeem(reservation_hex)?,
                Ok(status) => anyhow::bail!("unexpected zama status {status} before redeem"),
                Err(err) => return Err(err),
            }
            println!("ZAMA_REDEEM_OK {reservation_hex}");
            println!("RECOVERY_RESULT zama_redeemed");
            println!("TERMINAL_STATE zama_redeemed");
            println!("ACTION_COUNTS mint=1 settle=1 burn=1 release=1 zama=1");
        }
        let _ = self.halt(Step::ZamaRedeemed, journal)?;
        Ok(())
    }

    fn release_materials(
        &self,
        accounts: &BridgeAccounts,
        amount: u64,
        chain_now: i64,
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
        journal.release_expiry = chain_now + release_deadline_secs();
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
        let chain_before = self.chain_unix_timestamp()?;
        println!("FAULT_INJECTED expiry_before_settlement");
        println!("LOCKED_VALUE {amount}");
        println!("LOCKED_STATE solana_vault");
        println!("CHAIN_CLOCK {chain_before}");
        let chain_now = self.wait_until_chain_time(mint_expiry)?;
        println!("CHAIN_CLOCK {chain_now}");
        println!(
            "RECOVERY_DURATION_CHAIN_SECS {}",
            chain_now.saturating_sub(chain_before)
        );
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
        let cancel_now = self.chain_unix_timestamp()?;
        let cancel_expiry = cancel_now + 3600;
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
        println!("RECOVERY_RESULT cancelled");
        println!("TERMINAL_STATE cancelled");
        println!("ACTION_COUNTS mint=0 settle=0 burn=0 release=0 zama=1");
        println!("EXPIRY_RECOVERY_COMPLETE");
        Ok(())
    }

    fn wait_until_chain_time(&self, expiry: i64) -> Result<i64> {
        loop {
            let now = self.chain_unix_timestamp()?;
            println!("CHAIN_CLOCK {now}");
            if approval_expired_on_chain(expiry, now) {
                return Ok(now);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn mark_fault_injected(
        &self,
        journal: &mut Journal,
        name: &str,
        locked_value: u64,
    ) -> Result<()> {
        let chain_now = self.chain_unix_timestamp()?;
        journal.fault_injected_chain_time = chain_now;
        journal.fault_recovered_chain_time = 0;
        if !self.omit_journal_save {
            self.store.save_journal(journal)?;
        }
        println!("FAULT_INJECTED {name}");
        println!("LOCKED_VALUE {locked_value}");
        println!("LOCKED_STATE solana_vault");
        println!("CHAIN_CLOCK {chain_now}");
        println!("RECOVERY_WAIT_UNBOUNDED operator_or_quorum");
        Ok(())
    }

    fn note_fault_recovered(&self, journal: &mut Journal) -> Result<()> {
        if journal.fault_injected_chain_time == 0 || journal.fault_recovered_chain_time != 0 {
            return Ok(());
        }
        let chain_now = self.chain_unix_timestamp()?;
        journal.fault_recovered_chain_time = chain_now;
        if !self.omit_journal_save {
            self.store.save_journal(journal)?;
        }
        println!("CHAIN_CLOCK {chain_now}");
        println!(
            "RECOVERY_DURATION_CHAIN_SECS {}",
            chain_now.saturating_sub(journal.fault_injected_chain_time)
        );
        println!("RECOVERY_WAIT_UNBOUNDED operator_or_quorum");
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
            if !self.omit_journal_save {
                self.store.save_journal(journal)?;
            }
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
        self.approve_needed(fee_payer, attester_a, attester_b, args, digest)
    }

    fn approve_needed(
        &self,
        fee_payer: &Pubkey,
        attester_a: &Keypair,
        attester_b: &Keypair,
        args: &ApproveFields,
        digest: &[u8; 32],
    ) -> Result<()> {
        let (approval, _) = approval_pda(&args.operation, args.direction);
        let existing = self.read_approval(&approval)?;
        let chain_now = self.chain_unix_timestamp()?;
        let needed = attesters_needed(
            existing.as_ref(),
            digest,
            args.expiry,
            chain_now,
            args.direction == DIRECTION_RELEASE,
        )?;
        let approve = approve_ix(*fee_payer, args)?;
        measure_legacy(
            "approve_with_relayer_fee_payer",
            &[ed25519_approve_ix(attester_a, digest)?, approve.clone()],
            fee_payer,
        )?;
        for (index, attester) in [attester_a, attester_b].into_iter().enumerate() {
            if !needed[index] {
                continue;
            }
            let ed = ed25519_approve_ix(attester, digest)?;
            let ix = approve_ix(*fee_payer, args)?;
            let id = self
                .relayer
                .send_instructions(&[to_relayer(&ed), to_relayer(&ix)])?;
            let sig = self.relayer.wait_confirmed(&id, Duration::from_secs(60))?;
            self.confirm_on_chain(&sig)?;
            if self.halt_after_first_approval {
                break;
            }
            if self.inject_attester_disagreement {
                let mut bad_digest = *digest;
                bad_digest[0] ^= 0xff;
                let conflicting = [attester_a, attester_b][1 - index];
                let bad_ed = ed25519_approve_ix(conflicting, &bad_digest)?;
                let bad_ix = approve_ix(*fee_payer, args)?;
                if let Ok(bad_id) = self
                    .relayer
                    .send_instructions(&[to_relayer(&bad_ed), to_relayer(&bad_ix)])
                {
                    if let Ok(bad_sig) = self
                        .relayer
                        .wait_confirmed(&bad_id, Duration::from_secs(60))
                    {
                        anyhow::ensure!(
                            self.confirm_on_chain(&bad_sig).is_err(),
                            "conflicting attestation was accepted"
                        );
                    }
                }
                break;
            }
        }
        if let Some(after) = self.read_approval(&approval)? {
            if args.direction == DIRECTION_MINT {
                println!("MINT_APPROVAL_BITMAP {}", after.signer_bitmap);
            } else {
                println!("RELEASE_APPROVAL_BITMAP {}", after.signer_bitmap);
            }
            if !self.halt_after_first_approval && !self.inject_attester_disagreement {
                anyhow::ensure!(
                    after.signer_bitmap.count_ones() >= 2,
                    "2-of-3 approval is missing"
                );
            }
        } else if !self.halt_after_first_approval && !self.inject_attester_disagreement {
            anyhow::bail!("approval account is missing after submit");
        }
        Ok(())
    }

    fn mint_deadline(&self) -> Result<i64> {
        Ok(self.chain_unix_timestamp()? + env_deadline_secs("BRIDGE_MINT_EXPIRY_SECS", 3600))
    }

    fn chain_unix_timestamp(&self) -> Result<i64> {
        let account = self
            .rpc
            .get_account(&solana_sdk::sysvar::clock::id())
            .context("Solana clock sysvar")?;
        decode_chain_clock(&account.data)
    }

    fn read_approval(&self, approval: &Pubkey) -> Result<Option<OnChainApproval>> {
        match self.rpc.get_account(approval) {
            Ok(account) => Ok(Some(decode_approval(&account.data)?)),
            Err(err) if account_missing(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn read_receipt_status(&self, receipt: &Pubkey) -> Result<Option<u8>> {
        match self.rpc.get_account(receipt) {
            Ok(account) => Ok(Some(decode_receipt_status(&account.data)?)),
            Err(err) if account_missing(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
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

fn release_deadline_secs() -> i64 {
    env_deadline_secs("BRIDGE_RELEASE_EXPIRY_SECS", 3600)
}

fn env_deadline_secs(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default)
        .max(5)
}

fn account_missing(err: &solana_client::client_error::ClientError) -> bool {
    let text = err.to_string();
    text.contains("AccountNotFound") || text.contains("could not find account")
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

struct CompletedResume {
    accounts: BridgeAccounts,
    journal: Journal,
    receipt: Pubkey,
    reservation_hex: String,
}

impl Step {
    fn max_completed(self, current: Option<Step>) -> Step {
        match current {
            Some(done) if done.rank() > self.rank() => done,
            _ => self,
        }
    }
}
