use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use solana_system_interface::instruction as system_instruction;
use solana_zk_sdk::encryption::auth_encryption::AeKey;
use solana_zk_sdk::encryption::elgamal::{ElGamalKeypair, ElGamalPubkey};
use solana_zk_sdk::zk_elgamal_proof_program::proof_data::PubkeyValidityProofData;
use spl_token_2022::extension::confidential_transfer::instruction as confidential_ix;
use spl_token_2022::extension::confidential_transfer::{
    ConfidentialTransferAccount, DecryptableBalance,
};
use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};
use spl_token_2022::instruction::{
    initialize_account3, initialize_mint2, mint_to, set_authority, AuthorityType,
};
use spl_token_2022::state::{Account as TokenAccount, Mint};
use spl_token_confidential_transfer_proof_extraction::instruction::{ProofData, ProofLocation};
use std::convert::TryInto;
use std::num::NonZeroI8;
use std::thread;
use std::time::Duration;

use crate::program::{initialize_ix, vault_authority_pda};

pub const DECIMALS: u8 = 6;

pub struct ConfidentialOwner {
    pub authority: Keypair,
    pub token: Pubkey,
    pub elgamal: ElGamalKeypair,
    pub aes: AeKey,
}

pub struct BridgeAccounts {
    pub mint: Pubkey,
    pub source: ConfidentialOwner,
    pub vault: Pubkey,
    pub vault_elgamal: ElGamalKeypair,
    pub vault_aes: AeKey,
    pub destination: ConfidentialOwner,
}

pub fn create_bridge_accounts(
    rpc: &RpcClient,
    payer: &Keypair,
    attesters: [Pubkey; 3],
    amount: u64,
) -> Result<BridgeAccounts> {
    let token_program = spl_token_2022::id();
    let mint_authority = Keypair::new();
    let mint = Keypair::new();
    let source_authority = Keypair::new();
    let dest_authority = Keypair::new();
    let source_token = Keypair::new();
    let vault_token = Keypair::new();
    let dest_token = Keypair::new();
    let source_elgamal = ElGamalKeypair::new_rand();
    let vault_elgamal = ElGamalKeypair::new_rand();
    let dest_elgamal = ElGamalKeypair::new_rand();
    let source_aes = AeKey::new_rand();
    let vault_aes = AeKey::new_rand();
    let dest_aes = AeKey::new_rand();

    airdrop(rpc, &mint_authority.pubkey())?;
    airdrop(rpc, &source_authority.pubkey())?;
    airdrop(rpc, &dest_authority.pubkey())?;

    let mint_len = ExtensionType::try_calculate_account_len::<Mint>(&[
        ExtensionType::ConfidentialTransferMint,
    ])?;
    let account_len = ExtensionType::try_calculate_account_len::<TokenAccount>(&[
        ExtensionType::ConfidentialTransferAccount,
    ])?;
    let mint_rent = rpc.get_minimum_balance_for_rent_exemption(mint_len)?;
    let account_rent = rpc.get_minimum_balance_for_rent_exemption(account_len)?;

    send(
        rpc,
        payer,
        &[
            system_instruction::create_account(
                &payer.pubkey(),
                &mint.pubkey(),
                mint_rent,
                mint_len as u64,
                &token_program,
            ),
            confidential_ix::initialize_mint(
                &token_program,
                &mint.pubkey(),
                Some(mint_authority.pubkey()),
                true,
                None,
            )?,
            initialize_mint2(
                &token_program,
                &mint.pubkey(),
                &mint_authority.pubkey(),
                None,
                DECIMALS,
            )?,
        ],
        &[&mint],
    )?;

    create_confidential_account(
        rpc,
        payer,
        &token_program,
        &mint.pubkey(),
        &source_token,
        &source_authority,
        &source_elgamal,
        &source_aes,
        account_rent,
        account_len,
        false,
    )?;
    create_confidential_account(
        rpc,
        payer,
        &token_program,
        &mint.pubkey(),
        &vault_token,
        payer,
        &vault_elgamal,
        &vault_aes,
        account_rent,
        account_len,
        true,
    )?;
    create_confidential_account(
        rpc,
        payer,
        &token_program,
        &mint.pubkey(),
        &dest_token,
        &dest_authority,
        &dest_elgamal,
        &dest_aes,
        account_rent,
        account_len,
        false,
    )?;

    let (vault_authority, _) = vault_authority_pda();
    send(
        rpc,
        payer,
        &[set_authority(
            &token_program,
            &vault_token.pubkey(),
            Some(&vault_authority),
            AuthorityType::AccountOwner,
            &payer.pubkey(),
            &[],
        )?],
        &[],
    )?;

    let chain_id = rpc.get_genesis_hash()?.to_bytes();
    send(
        rpc,
        payer,
        &[initialize_ix(
            payer.pubkey(),
            mint.pubkey(),
            vault_token.pubkey(),
            token_program,
            chain_id,
            attesters,
        )?],
        &[],
    )?;

    send(
        rpc,
        payer,
        &[mint_to(
            &token_program,
            &mint.pubkey(),
            &source_token.pubkey(),
            &mint_authority.pubkey(),
            &[],
            amount,
        )?],
        &[&mint_authority],
    )?;
    deposit_and_apply(
        rpc,
        payer,
        &token_program,
        &mint.pubkey(),
        &source_token.pubkey(),
        &source_authority,
        &source_aes,
        amount,
    )?;

    let decimals = read_mint_decimals(rpc, &mint.pubkey())?;
    crate::units::require_mint_decimals(decimals, DECIMALS)?;

    Ok(BridgeAccounts {
        mint: mint.pubkey(),
        source: ConfidentialOwner {
            authority: source_authority,
            token: source_token.pubkey(),
            elgamal: source_elgamal,
            aes: source_aes,
        },
        vault: vault_token.pubkey(),
        vault_elgamal,
        vault_aes,
        destination: ConfidentialOwner {
            authority: dest_authority,
            token: dest_token.pubkey(),
            elgamal: dest_elgamal,
            aes: dest_aes,
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn create_confidential_account(
    rpc: &RpcClient,
    payer: &Keypair,
    token_program: &Pubkey,
    mint: &Pubkey,
    account: &Keypair,
    owner: &Keypair,
    elgamal: &ElGamalKeypair,
    aes: &AeKey,
    rent: u64,
    space: usize,
    disable_public_credits: bool,
) -> Result<()> {
    send(
        rpc,
        payer,
        &[
            system_instruction::create_account(
                &payer.pubkey(),
                &account.pubkey(),
                rent,
                space as u64,
                token_program,
            ),
            initialize_account3(token_program, &account.pubkey(), mint, &owner.pubkey())?,
        ],
        &[account],
    )?;
    let proof = PubkeyValidityProofData::new(elgamal).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let zero: DecryptableBalance = aes.encrypt(0).into();
    let configure = confidential_ix::configure_account(
        token_program,
        &account.pubkey(),
        mint,
        &zero,
        65_535,
        &owner.pubkey(),
        &[],
        ProofLocation::InstructionOffset(
            NonZeroI8::new(1).unwrap(),
            ProofData::InstructionData(&proof),
        ),
    )?;
    send(rpc, payer, &configure, &[owner])?;
    if disable_public_credits {
        send(
            rpc,
            payer,
            &[confidential_ix::disable_non_confidential_credits(
                token_program,
                &account.pubkey(),
                &owner.pubkey(),
                &[],
            )?],
            &[owner],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn deposit_and_apply(
    rpc: &RpcClient,
    payer: &Keypair,
    token_program: &Pubkey,
    mint: &Pubkey,
    token: &Pubkey,
    owner: &Keypair,
    aes: &AeKey,
    amount: u64,
) -> Result<()> {
    send(
        rpc,
        payer,
        &[confidential_ix::deposit(
            token_program,
            token,
            mint,
            amount,
            DECIMALS,
            &owner.pubkey(),
            &[],
        )?],
        &[owner],
    )?;
    let new_decryptable: DecryptableBalance = aes.encrypt(amount).into();
    send(
        rpc,
        payer,
        &[confidential_ix::apply_pending_balance(
            token_program,
            token,
            1,
            &new_decryptable,
            &owner.pubkey(),
            &[],
        )?],
        &[owner],
    )?;
    Ok(())
}

pub fn apply_pending(
    rpc: &RpcClient,
    payer: &Keypair,
    token: &Pubkey,
    owner: &Keypair,
    aes: &AeKey,
    expected_available: u64,
    expected_counter: u64,
) -> Result<()> {
    let new_decryptable: DecryptableBalance = aes.encrypt(expected_available).into();
    send(
        rpc,
        payer,
        &[confidential_ix::apply_pending_balance(
            &spl_token_2022::id(),
            token,
            expected_counter,
            &new_decryptable,
            &owner.pubkey(),
            &[],
        )?],
        &[owner],
    )?;
    Ok(())
}

pub fn pending_credit_counter(rpc: &RpcClient, token: &Pubkey) -> Result<u64> {
    let account = rpc.get_account(token)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&account.data)?;
    let confidential = state.get_extension::<ConfidentialTransferAccount>()?;
    Ok(u64::from(confidential.pending_balance_credit_counter))
}

pub fn decrypt_available(rpc: &RpcClient, token: &Pubkey, aes: &AeKey) -> Result<u64> {
    let account = rpc.get_account(token)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&account.data)?;
    let confidential = state.get_extension::<ConfidentialTransferAccount>()?;
    let ciphertext = confidential
        .decryptable_available_balance
        .try_into()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    aes.decrypt(&ciphertext)
        .context("decrypt available confidential balance")
}

pub fn vault_elgamal_pubkey(accounts: &BridgeAccounts) -> ElGamalPubkey {
    *accounts.vault_elgamal.pubkey()
}

pub fn read_mint_decimals(rpc: &RpcClient, mint: &Pubkey) -> Result<u8> {
    let account = rpc.get_account(mint)?;
    let state = StateWithExtensions::<Mint>::unpack(&account.data)?;
    Ok(state.base.decimals)
}

pub fn config_is_initialized(rpc: &RpcClient) -> Result<bool> {
    let (config, _) = crate::program::config_pda();
    match rpc.get_account(&config) {
        Ok(account) => Ok(account.owner == crate::program::PROGRAM_ID),
        Err(err) => {
            let text = err.to_string();
            if text.contains("AccountNotFound") || text.contains("could not find account") {
                Ok(false)
            } else {
                Err(err).context("read bridge config")
            }
        }
    }
}

pub fn encode_keypair(keypair: &Keypair) -> String {
    hex::encode(keypair.to_bytes())
}

pub fn decode_keypair(text: &str) -> Result<Keypair> {
    let bytes = hex::decode(text).context("keypair hex")?;
    Keypair::try_from(bytes.as_slice()).context("keypair bytes")
}

pub fn encode_elgamal(keypair: &ElGamalKeypair) -> String {
    hex::encode(keypair.secret().as_bytes())
}

pub fn decode_elgamal(text: &str) -> Result<ElGamalKeypair> {
    let bytes = hex::decode(text).context("elgamal hex")?;
    let secret = solana_zk_sdk::encryption::elgamal::ElGamalSecretKey::try_from(bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(ElGamalKeypair::new(secret))
}

pub fn encode_aes(key: &AeKey) -> String {
    let bytes: [u8; 16] = key.clone().into();
    hex::encode(bytes)
}

pub fn decode_aes(text: &str) -> Result<AeKey> {
    let bytes = hex::decode(text).context("aes hex")?;
    let arr: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("aes key must be 16 bytes"))?;
    Ok(AeKey::from(arr))
}

pub fn airdrop(rpc: &RpcClient, pubkey: &Pubkey) -> Result<()> {
    const TARGET: u64 = 1_000_000_000;
    for _ in 0..12 {
        if rpc.get_balance(pubkey)? >= TARGET {
            return Ok(());
        }
        if let Ok(sig) = rpc.request_airdrop(pubkey, 10_000_000_000) {
            let _ = rpc.confirm_transaction_with_commitment(&sig, CommitmentConfig::confirmed());
        }
        thread::sleep(Duration::from_millis(250));
    }
    let balance = rpc.get_balance(pubkey)?;
    anyhow::ensure!(
        balance >= TARGET,
        "airdrop did not credit {pubkey}; balance={balance}"
    );
    Ok(())
}

fn send(rpc: &RpcClient, payer: &Keypair, ixs: &[Instruction], extra: &[&Keypair]) -> Result<()> {
    let blockhash = rpc.get_latest_blockhash()?;
    let mut signers = Vec::with_capacity(1 + extra.len());
    signers.push(payer);
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, blockhash);
    rpc.send_and_confirm_transaction(&tx)
        .with_context(|| format!("setup transaction with {} instructions", ixs.len()))?;
    Ok(())
}
