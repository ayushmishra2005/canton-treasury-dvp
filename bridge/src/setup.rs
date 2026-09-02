use anyhow::{Context, Result};
use canton_treasury_dvp_devnet_zk::{
    decode_hex_array, elgamal_pubkey, encode_hex, encrypt_aes, generate_keys,
    generate_pubkey_validity_proof, require_unused_context, verify_pubkey_validity_proof,
    AES_KEY_LEN, ELGAMAL_SECRET_LEN,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use solana_system_interface::instruction as system_instruction;
use spl_token_2022::extension::confidential_transfer::instruction as confidential_ix;
use spl_token_2022::extension::confidential_transfer::{
    ConfidentialTransferAccount, DecryptableBalance,
};
use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};
use spl_token_2022::instruction::{
    initialize_account3, initialize_mint2, mint_to, set_authority, AuthorityType,
};
use spl_token_2022::state::{Account as TokenAccount, Mint};
use spl_token_confidential_transfer_proof_extraction::instruction::ProofLocation;
use std::thread;
use std::time::Duration;

use crate::program::{initialize_ix, vault_authority_pda};

pub const DECIMALS: u8 = 6;

#[derive(Clone)]
pub struct ConfidentialKeypair {
    secret: [u8; ELGAMAL_SECRET_LEN],
    pubkey: [u8; 32],
}

impl ConfidentialKeypair {
    pub fn generate() -> Result<Self> {
        let keys = generate_keys()?;
        Ok(Self {
            secret: keys.elgamal_secret,
            pubkey: keys.elgamal_pubkey,
        })
    }

    pub fn from_secret(secret: [u8; ELGAMAL_SECRET_LEN]) -> Result<Self> {
        Ok(Self {
            pubkey: elgamal_pubkey(&secret)?,
            secret,
        })
    }

    pub fn secret_bytes(&self) -> &[u8; ELGAMAL_SECRET_LEN] {
        &self.secret
    }

    pub fn pubkey(&self) -> [u8; 32] {
        self.pubkey
    }
}

#[derive(Clone)]
pub struct ConfidentialAesKey {
    key: [u8; AES_KEY_LEN],
}

impl ConfidentialAesKey {
    pub fn generate() -> Result<Self> {
        Ok(Self {
            key: generate_keys()?.aes_key,
        })
    }

    pub fn from_bytes(key: [u8; AES_KEY_LEN]) -> Self {
        Self { key }
    }

    pub fn as_bytes(&self) -> &[u8; AES_KEY_LEN] {
        &self.key
    }

    pub fn encrypt(&self, amount: u64) -> Result<[u8; 36]> {
        encrypt_aes(&self.key, amount)
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<u64> {
        canton_treasury_dvp_devnet_zk::decrypt_aes(&self.key, ciphertext)
    }
}

pub struct ConfidentialOwner {
    pub authority: Keypair,
    pub token: Pubkey,
    pub elgamal: ConfidentialKeypair,
    pub aes: ConfidentialAesKey,
}

pub struct BridgeAccounts {
    pub mint: Pubkey,
    pub source: ConfidentialOwner,
    pub vault: Pubkey,
    pub vault_elgamal: ConfidentialKeypair,
    pub vault_aes: ConfidentialAesKey,
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
    let source_elgamal = ConfidentialKeypair::generate()?;
    let vault_elgamal = ConfidentialKeypair::generate()?;
    let dest_elgamal = ConfidentialKeypair::generate()?;
    let source_aes = ConfidentialAesKey::generate()?;
    let vault_aes = ConfidentialAesKey::generate()?;
    let dest_aes = ConfidentialAesKey::generate()?;

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

    let mut used_contexts = Vec::new();
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
        &mut used_contexts,
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
        &mut used_contexts,
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
        &mut used_contexts,
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
    elgamal: &ConfidentialKeypair,
    aes: &ConfidentialAesKey,
    rent: u64,
    space: usize,
    disable_public_credits: bool,
    used_contexts: &mut Vec<[u8; 32]>,
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
    configure_with_fresh_context(
        rpc,
        payer,
        token_program,
        mint,
        &account.pubkey(),
        owner,
        elgamal,
        aes,
        used_contexts,
    )?;
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
fn configure_with_fresh_context(
    rpc: &RpcClient,
    payer: &Keypair,
    token_program: &Pubkey,
    mint: &Pubkey,
    token_account: &Pubkey,
    owner: &Keypair,
    elgamal: &ConfidentialKeypair,
    aes: &ConfidentialAesKey,
    used_contexts: &mut Vec<[u8; 32]>,
) -> Result<()> {
    let context = Keypair::new();
    require_unused_context(
        used_contexts,
        &context.pubkey().to_bytes(),
        &token_account.to_bytes(),
    )?;
    let proof = generate_pubkey_validity_proof(elgamal.secret_bytes())?;
    verify_pubkey_validity_proof(&proof)?;
    let context_len = canton_treasury_dvp_devnet_zk::pubkey_validity_context_len();
    let context_rent = rpc.get_minimum_balance_for_rent_exemption(context_len)?;
    let zk_program = Pubkey::new_from_array(canton_treasury_dvp_devnet_zk::zk_proof_program_id());
    let create_context = system_instruction::create_account(
        &payer.pubkey(),
        &context.pubkey(),
        context_rent,
        context_len as u64,
        &zk_program,
    );
    let verify = raw_to_ix(
        canton_treasury_dvp_devnet_zk::encode_verify_pubkey_validity(
            context.pubkey().to_bytes(),
            owner.pubkey().to_bytes(),
            &proof,
        )?,
    )?;
    let zero = decryptable_from_bytes(&aes.encrypt(0)?)?;
    send(rpc, payer, &[create_context, verify], &[&context])?;
    let context_account = rpc
        .get_account(&context.pubkey())
        .context("read proof context")?;
    anyhow::ensure!(
        context_account.owner == zk_program,
        "proof context {} is owned by {}, not the ZK ElGamal program",
        context.pubkey(),
        context_account.owner
    );
    anyhow::ensure!(
        canton_treasury_dvp_devnet_zk::context_account_is_zk_owned(
            &context_account.owner.to_bytes()
        ),
        "proof context owner bytes do not match the ZK program"
    );
    let location = ProofLocation::ContextStateAccount(&context.pubkey());
    let configure = confidential_ix::configure_account(
        token_program,
        token_account,
        mint,
        &zero,
        65_535,
        &owner.pubkey(),
        &[],
        location,
    )?;
    send(rpc, payer, &configure, &[owner])?;
    let close = raw_to_ix(canton_treasury_dvp_devnet_zk::encode_close_context(
        context.pubkey().to_bytes(),
        owner.pubkey().to_bytes(),
        payer.pubkey().to_bytes(),
    )?)?;
    send(rpc, payer, &[close], &[owner])?;
    used_contexts.push(context.pubkey().to_bytes());
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
    aes: &ConfidentialAesKey,
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
    let new_decryptable = decryptable_from_bytes(&aes.encrypt(amount)?)?;
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
    aes: &ConfidentialAesKey,
    expected_available: u64,
    expected_counter: u64,
) -> Result<()> {
    let new_decryptable = decryptable_from_bytes(&aes.encrypt(expected_available)?)?;
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

pub fn decrypt_available(rpc: &RpcClient, token: &Pubkey, aes: &ConfidentialAesKey) -> Result<u64> {
    let account = rpc.get_account(token)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&account.data)?;
    let confidential = state.get_extension::<ConfidentialTransferAccount>()?;
    let bytes = bytemuck::bytes_of(&confidential.decryptable_available_balance);
    aes.decrypt(bytes)
}

pub fn has_confidential_extension(rpc: &RpcClient, token: &Pubkey) -> Result<bool> {
    let account = rpc.get_account(token)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&account.data)?;
    Ok(state.get_extension::<ConfidentialTransferAccount>().is_ok())
}

pub fn has_pending_encrypted_credit(rpc: &RpcClient, token: &Pubkey) -> Result<bool> {
    let account = rpc.get_account(token)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&account.data)?;
    let confidential = state.get_extension::<ConfidentialTransferAccount>()?;
    let lo = bytemuck::bytes_of(&confidential.pending_balance_lo);
    let hi = bytemuck::bytes_of(&confidential.pending_balance_hi);
    Ok(u64::from(confidential.pending_balance_credit_counter) > 0
        && (lo.iter().any(|byte| *byte != 0) || hi.iter().any(|byte| *byte != 0)))
}

pub fn vault_elgamal_pubkey(accounts: &BridgeAccounts) -> [u8; 32] {
    accounts.vault_elgamal.pubkey()
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

pub fn encode_elgamal(keypair: &ConfidentialKeypair) -> String {
    encode_hex(keypair.secret_bytes())
}

pub fn decode_elgamal(text: &str) -> Result<ConfidentialKeypair> {
    ConfidentialKeypair::from_secret(decode_hex_array(text, "elgamal secret")?)
}

pub fn encode_aes(key: &ConfidentialAesKey) -> String {
    encode_hex(key.as_bytes())
}

pub fn decode_aes(text: &str) -> Result<ConfidentialAesKey> {
    Ok(ConfidentialAesKey::from_bytes(decode_hex_array(
        text, "aes key",
    )?))
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

pub fn decryptable_from_bytes(bytes: &[u8; 36]) -> Result<DecryptableBalance> {
    bytemuck::try_from_bytes::<DecryptableBalance>(bytes)
        .copied()
        .map_err(|e| anyhow::anyhow!("decryptable balance bytes: {e}"))
}

pub fn raw_to_ix(raw: canton_treasury_dvp_devnet_zk::RawInstruction) -> Result<Instruction> {
    Ok(Instruction {
        program_id: Pubkey::new_from_array(raw.program_id),
        accounts: raw
            .accounts
            .into_iter()
            .map(|meta| AccountMeta {
                pubkey: Pubkey::new_from_array(meta.pubkey),
                is_signer: meta.is_signer,
                is_writable: meta.is_writable,
            })
            .collect(),
        data: raw.data,
    })
}

pub fn send(
    rpc: &RpcClient,
    payer: &Keypair,
    ixs: &[Instruction],
    extra: &[&Keypair],
) -> Result<String> {
    let blockhash = rpc.get_latest_blockhash()?;
    let mut signers = Vec::with_capacity(1 + extra.len());
    signers.push(payer);
    for signer in extra {
        if signer.pubkey() != payer.pubkey() {
            signers.push(*signer);
        }
    }
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, blockhash);
    let signature = rpc
        .send_and_confirm_transaction(&tx)
        .with_context(|| format!("setup transaction with {} instructions", ixs.len()))?;
    Ok(signature.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use canton_treasury_dvp_devnet_zk::{
        encode_verify_pubkey_validity, generate_pubkey_validity_proof, keys_from_secret,
        require_unused_context, verify_pubkey_validity_proof, zk_proof_program_id,
    };

    #[test]
    fn configure_instruction_points_at_a_context_account() {
        let token = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let context = Pubkey::new_unique();
        let zero = decryptable_from_bytes(&[0u8; 36]).unwrap_or_else(|_| {
            let keys = generate_keys().unwrap();
            decryptable_from_bytes(&encrypt_aes(&keys.aes_key, 0).unwrap()).unwrap()
        });
        let ixs = confidential_ix::configure_account(
            &spl_token_2022::id(),
            &token,
            &mint,
            &zero,
            65_535,
            &owner,
            &[],
            ProofLocation::ContextStateAccount(&context),
        )
        .unwrap();
        assert_eq!(ixs.len(), 1);
        assert!(ixs[0].accounts.iter().any(|meta| meta.pubkey == context));
        assert!(!ixs[0]
            .accounts
            .iter()
            .any(|meta| { meta.pubkey == solana_sdk::sysvar::instructions::id() }));
    }

    #[test]
    fn wrong_context_account_is_not_the_token_account() {
        let token = Pubkey::new_unique();
        let context = Pubkey::new_unique();
        assert_ne!(token, context);
        let keys = generate_keys().unwrap();
        let proof = generate_pubkey_validity_proof(&keys.elgamal_secret).unwrap();
        let ix =
            encode_verify_pubkey_validity(context.to_bytes(), token.to_bytes(), &proof).unwrap();
        assert_eq!(ix.program_id, zk_proof_program_id());
        assert!(ix
            .accounts
            .iter()
            .any(|meta| meta.pubkey == context.to_bytes() && meta.is_writable));
    }

    #[test]
    fn reused_context_cannot_configure_another_account() {
        let context = [4u8; 32];
        require_unused_context(&[], &context, &[1u8; 32]).unwrap();
        assert!(require_unused_context(&[context], &context, &[2u8; 32]).is_err());
    }

    #[test]
    fn source_vault_and_destination_each_need_their_own_context() {
        let source = [11u8; 32];
        let vault = [12u8; 32];
        let destination = [13u8; 32];
        let mut used = Vec::new();
        require_unused_context(&used, &source, &[1u8; 32]).unwrap();
        used.push(source);
        require_unused_context(&used, &vault, &[2u8; 32]).unwrap();
        used.push(vault);
        require_unused_context(&used, &destination, &[3u8; 32]).unwrap();
        used.push(destination);
        assert_eq!(used.len(), 3);
        assert!(require_unused_context(&used, &vault, &[4u8; 32]).is_err());
    }

    #[test]
    fn generated_keys_round_trip_through_hex() {
        let elgamal = ConfidentialKeypair::generate().unwrap();
        let aes = ConfidentialAesKey::generate().unwrap();
        let restored = decode_elgamal(&encode_elgamal(&elgamal)).unwrap();
        assert_eq!(restored.pubkey(), elgamal.pubkey());
        let restored_aes = decode_aes(&encode_aes(&aes)).unwrap();
        let ciphertext = aes.encrypt(9).unwrap();
        assert_eq!(restored_aes.decrypt(&ciphertext).unwrap(), 9);
        verify_pubkey_validity_proof(
            &generate_pubkey_validity_proof(elgamal.secret_bytes()).unwrap(),
        )
        .unwrap();
        let _ = keys_from_secret(elgamal.secret_bytes(), aes.as_bytes()).unwrap();
    }
}
