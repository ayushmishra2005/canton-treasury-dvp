use anyhow::{Context, Result};
use canton_treasury_dvp_devnet_zk::{
    encode_close_context, encode_verify_pubkey_validity, generate_pubkey_validity_proof,
    pubkey_validity_context_len, require_unused_context, verify_pubkey_validity_proof,
    zk_proof_program_id,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_system_interface::instruction as system_instruction;
use spl_token_2022::extension::confidential_transfer::instruction as confidential_ix;
use spl_token_2022::extension::ExtensionType;
use spl_token_2022::instruction::{initialize_account3, initialize_mint2, mint_to};
use spl_token_2022::state::{Account as TokenAccount, Mint};
use spl_token_confidential_transfer_proof_extraction::instruction::ProofLocation;
use std::path::{Path, PathBuf};

use crate::confidential::{confidential_transfer_ixs, generate_transfer_proofs};
use crate::setup::{
    decryptable_from_bytes, has_confidential_extension, has_pending_encrypted_credit, raw_to_ix,
    send, ConfidentialAesKey, ConfidentialKeypair, DECIMALS,
};

const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
const PUBLIC_DEVNET_RPC: &str = "https://api.devnet.solana.com";
const DEFAULT_PAYER: &str = "bridge/.run/testnet/devnet-deployer-keypair.json";
const PROBE_AMOUNT: u64 = 1_000;
const TRANSFER_AMOUNT: u64 = 100;

pub struct ProbeReport {
    pub passed: bool,
    pub lines: Vec<String>,
}

pub fn run() -> Result<ProbeReport> {
    let mut lines = Vec::new();
    match run_inner(&mut lines) {
        Ok(()) => {
            lines.push("PROBE_RESULT PASS".to_string());
            Ok(ProbeReport {
                passed: true,
                lines,
            })
        }
        Err(err) => {
            lines.push(format!("PROBE_ERROR {err:#}"));
            lines.push("PROBE_RESULT FAIL".to_string());
            Ok(ProbeReport {
                passed: false,
                lines,
            })
        }
    }
}

fn run_inner(lines: &mut Vec<String>) -> Result<()> {
    let rpc_url = std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| PUBLIC_DEVNET_RPC.to_string());
    lines.push(format!("PROBE_RPC {}", rpc_label(&rpc_url)));
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let genesis = rpc.get_genesis_hash().context("genesis hash")?;
    lines.push(format!("PROBE_CLUSTER_GENESIS {genesis}"));
    let allow_local = std::env::var("BRIDGE_PROBE_ALLOW_LOCAL").ok().as_deref() == Some("1");
    if !allow_local {
        anyhow::ensure!(
            genesis.to_string() == DEVNET_GENESIS,
            "RPC is not Solana Devnet (genesis {genesis})"
        );
    }

    let token_program = spl_token_2022::id();
    let zk_program = Pubkey::new_from_array(zk_proof_program_id());
    require_program(&rpc, &token_program, "Token-2022", lines)?;
    require_program(&rpc, &zk_program, "zk-elgamal-proof", lines)?;
    let stop_after = std::env::var("BRIDGE_PROBE_STOP_AFTER").ok();
    if stop_after.as_deref() != Some("configure") {
        require_program(
            &rpc,
            &Pubkey::new_from_array(canton_treasury_dvp_devnet_zk::record_program_id()),
            "spl-record",
            lines,
        )?;
    }

    let payer_path =
        PathBuf::from(std::env::var("BRIDGE_PAYER").unwrap_or_else(|_| DEFAULT_PAYER.to_string()));
    let payer = load_keypair(&payer_path)?;
    lines.push(format!("PROBE_PAYER {}", payer.pubkey()));
    let start_balance = rpc.get_balance(&payer.pubkey())?;
    lines.push(format!("PROBE_PAYER_BALANCE_START {start_balance}"));
    anyhow::ensure!(
        start_balance >= 200_000_000,
        "existing Devnet payer has {start_balance} lamports; not enough for the official-program probe"
    );

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

    let mint_len = ExtensionType::try_calculate_account_len::<Mint>(&[
        ExtensionType::ConfidentialTransferMint,
    ])?;
    let account_len = ExtensionType::try_calculate_account_len::<TokenAccount>(&[
        ExtensionType::ConfidentialTransferAccount,
    ])?;
    let mint_rent = rpc.get_minimum_balance_for_rent_exemption(mint_len)?;
    let account_rent = rpc.get_minimum_balance_for_rent_exemption(account_len)?;

    let mint_sig = send(
        &rpc,
        &payer,
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
    record_step(
        lines,
        "initialize_mint_confidential",
        &mint.pubkey(),
        &mint_sig,
    );

    let mut used_contexts = Vec::new();
    configure_one(
        &rpc,
        &payer,
        &token_program,
        &mint.pubkey(),
        &source_token,
        &source_authority,
        &source_elgamal,
        &source_aes,
        account_rent,
        account_len,
        false,
        "source",
        &mut used_contexts,
        lines,
    )?;
    configure_one(
        &rpc,
        &payer,
        &token_program,
        &mint.pubkey(),
        &vault_token,
        &payer,
        &vault_elgamal,
        &vault_aes,
        account_rent,
        account_len,
        true,
        "vault",
        &mut used_contexts,
        lines,
    )?;
    configure_one(
        &rpc,
        &payer,
        &token_program,
        &mint.pubkey(),
        &dest_token,
        &dest_authority,
        &dest_elgamal,
        &dest_aes,
        account_rent,
        account_len,
        false,
        "destination",
        &mut used_contexts,
        lines,
    )?;
    anyhow::ensure!(used_contexts.len() == 3, "expected three proof contexts");
    if stop_after.as_deref() == Some("configure") {
        lines.push("PROBE_STOPPED_AFTER configure".to_string());
        return Ok(());
    }

    let mint_sig = send(
        &rpc,
        &payer,
        &[mint_to(
            &token_program,
            &mint.pubkey(),
            &source_token.pubkey(),
            &mint_authority.pubkey(),
            &[],
            PROBE_AMOUNT,
        )?],
        &[&mint_authority],
    )?;
    record_step(lines, "mint_to_source", &source_token.pubkey(), &mint_sig);

    let deposit_sig = send(
        &rpc,
        &payer,
        &[confidential_ix::deposit(
            &token_program,
            &source_token.pubkey(),
            &mint.pubkey(),
            PROBE_AMOUNT,
            DECIMALS,
            &source_authority.pubkey(),
            &[],
        )?],
        &[&source_authority],
    )?;
    record_step(
        lines,
        "confidential_deposit",
        &source_token.pubkey(),
        &deposit_sig,
    );

    let apply_sig = send(
        &rpc,
        &payer,
        &[confidential_ix::apply_pending_balance(
            &token_program,
            &source_token.pubkey(),
            1,
            &decryptable_from_bytes(&source_aes.encrypt(PROBE_AMOUNT)?)?,
            &source_authority.pubkey(),
            &[],
        )?],
        &[&source_authority],
    )?;
    record_step(lines, "apply_pending", &source_token.pubkey(), &apply_sig);

    let proofs = generate_transfer_proofs(
        &rpc,
        &payer,
        &source_elgamal,
        &source_aes,
        &source_token.pubkey(),
        dest_elgamal.pubkey(),
        TRANSFER_AMOUNT,
    )?;
    lines.push(format!(
        "PROBE_TRANSFER_CONTEXTS equality={} validity={} range={}",
        proofs.equality.pubkey(),
        proofs.validity.pubkey(),
        proofs.range.pubkey()
    ));
    let transfer = confidential_transfer_ixs(
        &token_program,
        &source_token.pubkey(),
        &mint.pubkey(),
        &dest_token.pubkey(),
        &source_authority.pubkey(),
        &proofs,
    )?;
    let transfer_sig = send(&rpc, &payer, &transfer, &[&source_authority])?;
    record_step(
        lines,
        "confidential_transfer",
        &dest_token.pubkey(),
        &transfer_sig,
    );

    anyhow::ensure!(
        has_pending_encrypted_credit(&rpc, &dest_token.pubkey())?,
        "destination did not receive an encrypted pending credit"
    );
    let pending = crate::setup::pending_credit_counter(&rpc, &dest_token.pubkey())?;
    lines.push(format!("PROBE_DEST_PENDING_COUNTER {pending}"));
    let end_balance = rpc.get_balance(&payer.pubkey())?;
    lines.push(format!("PROBE_PAYER_BALANCE_END {end_balance}"));
    lines.push(format!(
        "PROBE_PAYER_LAMPORTS_SPENT {}",
        start_balance.saturating_sub(end_balance)
    ));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn configure_one(
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
    label: &str,
    used_contexts: &mut Vec<[u8; 32]>,
    lines: &mut Vec<String>,
) -> Result<()> {
    let create_sig = send(
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
    record_step(
        lines,
        &format!("create_{label}_account"),
        &account.pubkey(),
        &create_sig,
    );

    let context = Keypair::new();
    require_unused_context(
        used_contexts,
        &context.pubkey().to_bytes(),
        &account.pubkey().to_bytes(),
    )?;
    let proof = generate_pubkey_validity_proof(elgamal.secret_bytes())?;
    verify_pubkey_validity_proof(&proof)?;
    lines.push(format!(
        "PROBE_{}_PUBKEY_VALIDITY_PROOF_BYTES {}",
        label.to_uppercase(),
        proof.len()
    ));
    let zk_program = Pubkey::new_from_array(zk_proof_program_id());
    let context_len = pubkey_validity_context_len();
    let context_rent = rpc.get_minimum_balance_for_rent_exemption(context_len)?;
    let create_context = system_instruction::create_account(
        &payer.pubkey(),
        &context.pubkey(),
        context_rent,
        context_len as u64,
        &zk_program,
    );
    let verify = raw_to_ix(encode_verify_pubkey_validity(
        context.pubkey().to_bytes(),
        owner.pubkey().to_bytes(),
        &proof,
    )?)?;
    let verify_sig = send(rpc, payer, &[create_context, verify], &[&context])?;
    record_step(
        lines,
        &format!("verify_pubkey_validity_{label}"),
        &context.pubkey(),
        &verify_sig,
    );
    let context_account = rpc
        .get_account(&context.pubkey())
        .context("read proof context")?;
    anyhow::ensure!(
        context_account.owner == zk_program,
        "{label} proof context is not owned by the ZK ElGamal program"
    );
    lines.push(format!(
        "PROBE_{}_CONTEXT_OWNER {}",
        label.to_uppercase(),
        context_account.owner
    ));

    let zero = decryptable_from_bytes(&aes.encrypt(0)?)?;
    let configure = confidential_ix::configure_account(
        token_program,
        &account.pubkey(),
        mint,
        &zero,
        65_535,
        &owner.pubkey(),
        &[],
        ProofLocation::ContextStateAccount(&context.pubkey()),
    )?;
    let configure_sig = send(rpc, payer, &configure, &[owner])?;
    record_step(
        lines,
        &format!("configure_{label}"),
        &account.pubkey(),
        &configure_sig,
    );
    anyhow::ensure!(
        has_confidential_extension(rpc, &account.pubkey())?,
        "{label} confidential-transfer extension is missing"
    );
    lines.push(format!(
        "PROBE_{}_CONFIDENTIAL_EXTENSION present",
        label.to_uppercase()
    ));

    let close = raw_to_ix(encode_close_context(
        context.pubkey().to_bytes(),
        owner.pubkey().to_bytes(),
        payer.pubkey().to_bytes(),
    )?)?;
    let close_sig = send(rpc, payer, &[close], &[owner])?;
    record_step(
        lines,
        &format!("close_{label}_context"),
        &context.pubkey(),
        &close_sig,
    );
    used_contexts.push(context.pubkey().to_bytes());

    if disable_public_credits {
        let disable_sig = send(
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
        record_step(
            lines,
            &format!("disable_public_credits_{label}"),
            &account.pubkey(),
            &disable_sig,
        );
    }
    Ok(())
}

fn require_program(
    rpc: &RpcClient,
    program: &Pubkey,
    name: &str,
    lines: &mut Vec<String>,
) -> Result<()> {
    let account = rpc
        .get_account(program)
        .with_context(|| format!("{name} program {program}"))?;
    anyhow::ensure!(
        account.executable,
        "{name} account {program} is not executable"
    );
    lines.push(format!("PROBE_PROGRAM_PRESENT {name} {program}"));
    Ok(())
}

fn record_step(lines: &mut Vec<String>, step: &str, address: &Pubkey, signature: &str) {
    lines.push(format!("PROBE_ADDRESS {step} {address}"));
    lines.push(format!("PROBE_STEP {step} {signature}"));
    lines.push(format!(
        "PROBE_LINK {step} https://explorer.solana.com/tx/{signature}?cluster=devnet"
    ));
}

fn load_keypair(path: &Path) -> Result<Keypair> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let secret: Vec<u8> = serde_json::from_slice(&bytes).context("keypair json")?;
    Keypair::try_from(secret.as_slice()).context("keypair bytes")
}

fn rpc_label(url: &str) -> String {
    if url.contains("api.devnet.solana.com") {
        PUBLIC_DEVNET_RPC.to_string()
    } else if url.contains("api-key") || url.contains("token=") || url.contains("apiKey") {
        "redacted".to_string()
    } else {
        url.split('?').next().unwrap_or("redacted").to_string()
    }
}
