use anyhow::{Context, Result};
use rand::RngCore;
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use std::time::Duration;
use zeroize::Zeroize;

use crate::attest::ed25519_approve_ix;
use crate::canonical::{
    amount_commitment, operation_digest, proof_commitment, DIRECTION_MINT, DIRECTION_RELEASE,
};
use crate::canton::CantonClient;
use crate::confidential::generate_transfer_proofs;
use crate::program::{
    approve_ix, config_pda, lock_ix, move_ix, receipt_pda, ApproveFields, LockFields,
    MovementFields,
};
use crate::relayer::{RelayerClient, RelayerInstruction};
use crate::setup::{
    apply_pending, create_bridge_accounts, decrypt_available, vault_elgamal_pubkey,
};
use crate::txsize::{report_size, serialize_legacy, LEGACY_LIMIT};
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
}

impl Workflow {
    pub fn run(&self, amount: u64) -> Result<()> {
        self.run_with(amount, true)
    }

    pub fn prove_relayer(&self, amount: u64) -> Result<()> {
        self.run_with(amount, false)
    }

    fn run_with(&self, amount: u64, include_canton_zama: bool) -> Result<()> {
        let fee_payer: Pubkey = self.relayer.address()?.parse()?;
        crate::setup::airdrop(&self.rpc, &fee_payer)?;
        crate::setup::airdrop(&self.rpc, &self.payer.pubkey())?;

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
        let source_before =
            decrypt_available(&self.rpc, &accounts.source.token, &accounts.source.aes)?;
        let vault_before = decrypt_available(&self.rpc, &accounts.vault, &accounts.vault_aes)?;
        let dest_before = decrypt_available(
            &self.rpc,
            &accounts.destination.token,
            &accounts.destination.aes,
        )?;
        anyhow::ensure!(
            source_before == amount,
            "source confidential balance before lock"
        );
        anyhow::ensure!(vault_before == 0, "vault confidential balance before lock");
        anyhow::ensure!(
            dest_before == 0,
            "destination confidential balance before lock"
        );

        let mut blinding = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut blinding);
        let commitment = amount_commitment(amount, &blinding);
        blinding.zeroize();

        let mut operation = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut operation);
        let reservation = operation;
        let previous = [0u8; 32];
        let expiry = chrono::Utc::now().timestamp() + 3600;
        let (config, _) = config_pda();
        let (receipt, _) = receipt_pda(&operation);
        let chain_id = self.rpc.get_genesis_hash()?.to_bytes();
        let reservation_hex = format!("0x{}", hex::encode(reservation));
        let lock_id = hex::encode(operation);

        if include_canton_zama && !self.zama.reserve(&reservation_hex, amount)? {
            anyhow::bail!("Zama rejected the reservation; no lock or mint will be attempted");
        }

        let lock_proofs = generate_transfer_proofs(
            &self.rpc,
            &self.payer,
            &accounts.source.elgamal,
            &accounts.source.aes,
            &accounts.source.token,
            &vault_elgamal_pubkey(&accounts),
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
        let vault_decryptable = crate::confidential::decryptable_bytes(
            &accounts.vault_aes.encrypt(vault_before + amount),
        )?;
        let lock_fields = LockFields {
            operation,
            destination: accounts.destination.token,
            amount_commitment: commitment,
            zama_reservation: reservation,
            previous_operation: previous,
            expiry,
            transfer_data: lock_proofs.transfer_data,
            vault_decryptable,
        };
        let lock = lock_ix(
            fee_payer,
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
            &fee_payer,
        )?;
        let lock_sig =
            self.send_extra_signer(&[lock], &[&accounts.source.authority], &fee_payer)?;
        self.confirm_on_chain(&lock_sig)?;

        let mint_digest = operation_digest(
            &chain_id,
            &crate::program::PROGRAM_ID,
            &config,
            &operation,
            DIRECTION_MINT,
            &receipt,
            &accounts.destination.token,
            &commitment,
            &reservation,
            &previous,
            expiry,
            &lock_proof,
        );
        self.approve_twice(
            &fee_payer,
            &ApproveFields {
                operation,
                direction: DIRECTION_MINT,
                destination: accounts.destination.token,
                amount_commitment: commitment,
                zama_reservation: reservation,
                previous_operation: previous,
                expiry,
                proof_commitment: lock_proof,
            },
            &mint_digest,
        )?;

        if include_canton_zama {
            self.canton
                .mint(&lock_id, amount, &hex::encode(mint_digest))?;
            self.zama.finalize(&reservation_hex)?;
            self.canton
                .redeem(&lock_id, amount, &hex::encode(mint_digest))?;
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
        let release_digest = operation_digest(
            &chain_id,
            &crate::program::PROGRAM_ID,
            &config,
            &operation,
            DIRECTION_RELEASE,
            &receipt,
            &accounts.destination.token,
            &commitment,
            &reservation,
            &previous,
            expiry,
            &release_proof,
        );
        let movement = MovementFields {
            destination: accounts.destination.token,
            amount_commitment: commitment,
            zama_reservation: reservation,
            previous_operation: previous,
            expiry,
            transfer_data: release_proofs.transfer_data,
        };
        let release = move_ix(
            "release_confidential",
            accounts.vault,
            accounts.destination.token,
            accounts.mint,
            spl_token_2022::id(),
            release_proofs.equality.pubkey(),
            release_proofs.validity.pubkey(),
            release_proofs.range.pubkey(),
            &operation,
            DIRECTION_RELEASE,
            &movement,
        )?;
        self.approve_twice(
            &fee_payer,
            &ApproveFields {
                operation,
                direction: DIRECTION_RELEASE,
                destination: accounts.destination.token,
                amount_commitment: commitment,
                zama_reservation: reservation,
                previous_operation: previous,
                expiry,
                proof_commitment: release_proof,
            },
            &release_digest,
        )?;
        measure_legacy(
            "release_with_relayer_fee_payer",
            std::slice::from_ref(&release),
            &fee_payer,
        )?;
        let release_id = self.relayer.send_instructions(&[to_relayer(&release)])?;
        let release_sig = self
            .relayer
            .wait_confirmed(&release_id, Duration::from_secs(90))?;
        self.confirm_on_chain(&release_sig)?;

        apply_pending(
            &self.rpc,
            &self.payer,
            &accounts.destination.token,
            &accounts.destination.authority,
            &accounts.destination.aes,
            dest_before + amount,
            1,
        )?;
        let source_after =
            decrypt_available(&self.rpc, &accounts.source.token, &accounts.source.aes)?;
        let vault_after = decrypt_available(&self.rpc, &accounts.vault, &accounts.vault_aes)?;
        let dest_after = decrypt_available(
            &self.rpc,
            &accounts.destination.token,
            &accounts.destination.aes,
        )?;
        anyhow::ensure!(
            source_after + amount == source_before,
            "source confidential amount"
        );
        anyhow::ensure!(vault_after == vault_before, "vault confidential amount");
        anyhow::ensure!(
            dest_after == dest_before + amount,
            "destination confidential amount"
        );

        self.inspect_public_leak(&release, amount)?;
        if include_canton_zama {
            self.zama.redeem(&reservation_hex)?;
            println!("ZAMA_REDEEM_OK {reservation_hex}");
        }
        println!("RELAYER_RELEASE_CONFIRMED {release_sig}");
        Ok(())
    }

    fn approve_twice(
        &self,
        fee_payer: &Pubkey,
        args: &ApproveFields,
        digest: &[u8; 32],
    ) -> Result<()> {
        let approve = approve_ix(*fee_payer, args)?;
        measure_legacy(
            "approve_with_relayer_fee_payer",
            &[
                ed25519_approve_ix(&self.attester_a, digest)?,
                approve.clone(),
            ],
            fee_payer,
        )?;
        for attester in [&self.attester_a, &self.attester_b] {
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
