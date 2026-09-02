use borsh::{BorshDeserialize, BorshSerialize};
use confidential_escrow::canonical::{
    operation_digest, DIRECTION_CANCEL, DIRECTION_MINT, DIRECTION_RELEASE, RECEIPT_LOCKED,
    RECEIPT_MINT_AUTHORIZED,
};
use confidential_escrow::handlers::{ApproveArgs, MovementArgs};
use confidential_escrow::state::{
    BridgeConfig, LockReceipt, OperationApproval, APPROVAL_SEED, CONFIG_SEED, RECEIPT_SEED,
};
use ed25519_dalek::{Signer as DalekSigner, SigningKey};
use sha2::{Digest, Sha256};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program_test::{ProgramTest, ProgramTestContext};
use solana_sdk::account::Account;
use solana_sdk::account::AccountSharedData;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::sysvar;
use solana_sdk::transaction::Transaction;

const PROGRAM_ID: Pubkey = confidential_escrow::ID;

fn disc(name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("global:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

fn account_disc(name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("account:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

fn pda(seeds: &[&[u8]]) -> (Pubkey, u8) {
    Pubkey::find_program_address(seeds, &PROGRAM_ID)
}

fn pack_config(config: &BridgeConfig) -> Vec<u8> {
    let mut data = account_disc("BridgeConfig").to_vec();
    data.extend_from_slice(&config.try_to_vec().unwrap());
    data
}

fn pack_receipt(receipt: &LockReceipt) -> Vec<u8> {
    let mut data = account_disc("LockReceipt").to_vec();
    data.extend_from_slice(&receipt.try_to_vec().unwrap());
    data
}

fn pack_approval(approval: &OperationApproval) -> Vec<u8> {
    let mut data = account_disc("OperationApproval").to_vec();
    data.extend_from_slice(&approval.try_to_vec().unwrap());
    data
}

fn write_account(ctx: &mut ProgramTestContext, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
    let account = Account {
        lamports: 1_000_000_000,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    };
    ctx.set_account(&key, &AccountSharedData::from(account));
}

fn set_clock(ctx: &mut ProgramTestContext, unix_timestamp: i64) {
    ctx.set_sysvar(&Clock {
        slot: 10,
        epoch_start_timestamp: unix_timestamp,
        epoch: 0,
        leader_schedule_epoch: 0,
        unix_timestamp,
    });
}

fn ed25519_ix(secret: &[u8; 32], digest: &[u8; 32]) -> Instruction {
    let signing = SigningKey::from_bytes(secret);
    let signature = signing.sign(digest);
    solana_sdk::ed25519_instruction::new_ed25519_instruction_with_signature(
        digest,
        &signature.to_bytes(),
        &signing.verifying_key().to_bytes(),
    )
}

fn approve_ix(payer: Pubkey, args: &ApproveArgs) -> Instruction {
    let (config, _) = pda(&[CONFIG_SEED]);
    let (receipt, _) = pda(&[RECEIPT_SEED, args.operation.as_ref()]);
    let (approval, _) = pda(&[APPROVAL_SEED, args.operation.as_ref(), &[args.direction]]);
    let mut data = disc("approve_operation").to_vec();
    data.extend_from_slice(&args.try_to_vec().unwrap());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(config, false),
            AccountMeta::new(receipt, false),
            AccountMeta::new(approval, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data,
    }
}

fn move_ix(
    name: &str,
    destination: Pubkey,
    mint: Pubkey,
    args: &MovementArgs,
    operation: &[u8; 32],
    direction: u8,
) -> Instruction {
    let (config, _) = pda(&[CONFIG_SEED]);
    let (receipt, _) = pda(&[RECEIPT_SEED, operation]);
    let (approval, _) = pda(&[APPROVAL_SEED, operation, &[direction]]);
    let (vault_authority, _) = pda(&[b"vault-authority"]);
    let mut data = disc(name).to_vec();
    data.extend_from_slice(&args.try_to_vec().unwrap());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config, false),
            AccountMeta::new(receipt, false),
            AccountMeta::new(approval, false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
        ],
        data,
    }
}

struct Harness {
    ctx: ProgramTestContext,
    attester_a: [u8; 32],
    attester_b: [u8; 32],
    chain_id: [u8; 32],
    operation: [u8; 32],
    destination: Pubkey,
    mint: Pubkey,
    amount_commitment: [u8; 32],
    reservation: [u8; 32],
    previous: [u8; 32],
    mint_expiry: i64,
    proof: [u8; 32],
}

fn attester_pubkey(secret: &[u8; 32]) -> Pubkey {
    Pubkey::new_from_array(SigningKey::from_bytes(secret).verifying_key().to_bytes())
}

async fn start_harness(status: u8, mint_expiry: i64) -> Harness {
    let deploy = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy");
    std::env::set_var("SBF_OUT_DIR", &deploy);
    let program = ProgramTest::new("confidential_escrow", PROGRAM_ID, None);
    let ctx = program.start_with_context().await;
    let attester_a = [7u8; 32];
    let attester_b = [8u8; 32];
    let attester_c = [9u8; 32];
    let attester_a_pk = attester_pubkey(&attester_a);
    let attester_b_pk = attester_pubkey(&attester_b);
    let attester_c_pk = attester_pubkey(&attester_c);
    let (config, config_bump) = pda(&[CONFIG_SEED]);
    let (_, vault_bump) = pda(&[b"vault-authority"]);
    let operation = [5u8; 32];
    let (receipt, receipt_bump) = pda(&[RECEIPT_SEED, operation.as_ref()]);
    let destination = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let vault = Pubkey::new_unique();
    let amount_commitment = [6u8; 32];
    let reservation = [4u8; 32];
    let previous = [0u8; 32];
    let proof = [10u8; 32];
    let chain_id = [11u8; 32];
    let config_data = BridgeConfig {
        chain_id,
        token_program: spl_token_2022::id(),
        mint,
        vault,
        attesters: [attester_a_pk, attester_b_pk, attester_c_pk],
        bump: config_bump,
        vault_authority_bump: vault_bump,
    };
    let receipt_data = LockReceipt {
        operation,
        destination,
        amount_commitment,
        zama_reservation: reservation,
        previous_operation: previous,
        expiry: mint_expiry,
        lock_proof_commitment: proof,
        status,
        bump: receipt_bump,
    };
    let mut harness = Harness {
        ctx,
        attester_a,
        attester_b,
        chain_id,
        operation,
        destination,
        mint,
        amount_commitment,
        reservation,
        previous,
        mint_expiry,
        proof,
    };
    write_account(
        &mut harness.ctx,
        config,
        PROGRAM_ID,
        pack_config(&config_data),
    );
    write_account(
        &mut harness.ctx,
        receipt,
        PROGRAM_ID,
        pack_receipt(&receipt_data),
    );
    harness
}

fn mint_args(h: &Harness) -> ApproveArgs {
    ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_MINT,
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: h.mint_expiry,
        proof_commitment: h.proof,
    }
}

fn digest_for(
    h: &Harness,
    direction: u8,
    dest: &Pubkey,
    expiry: i64,
    proof: &[u8; 32],
) -> [u8; 32] {
    let (config, _) = pda(&[CONFIG_SEED]);
    let (receipt, _) = pda(&[RECEIPT_SEED, h.operation.as_ref()]);
    operation_digest(
        &h.chain_id,
        &PROGRAM_ID,
        &config,
        &h.operation,
        direction,
        &receipt,
        dest,
        &h.amount_commitment,
        &h.reservation,
        &h.previous,
        expiry,
        proof,
    )
}

async fn approve(
    h: &mut Harness,
    secret: &[u8; 32],
    args: &ApproveArgs,
) -> std::result::Result<(), solana_sdk::transaction::TransactionError> {
    let digest = digest_for(
        h,
        args.direction,
        &args.destination,
        args.expiry,
        &args.proof_commitment,
    );
    let ed = ed25519_ix(secret, &digest);
    let ix = approve_ix(h.ctx.payer.pubkey(), args);
    let blockhash = h.ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ed, ix],
        Some(&h.ctx.payer.pubkey()),
        &[&h.ctx.payer],
        blockhash,
    );
    h.ctx
        .banks_client
        .process_transaction(tx)
        .await
        .map_err(|e| match e {
            solana_program_test::BanksClientError::TransactionError(err) => err,
            other => panic!("{other}"),
        })
}

async fn read_approval(h: &mut Harness, direction: u8) -> OperationApproval {
    let (key, _) = pda(&[APPROVAL_SEED, h.operation.as_ref(), &[direction]]);
    let account = h.ctx.banks_client.get_account(key).await.unwrap().unwrap();
    OperationApproval::try_from_slice(&account.data[8..]).unwrap()
}

async fn read_receipt(h: &mut Harness) -> LockReceipt {
    let (key, _) = pda(&[RECEIPT_SEED, h.operation.as_ref()]);
    let account = h.ctx.banks_client.get_account(key).await.unwrap().unwrap();
    LockReceipt::try_from_slice(&account.data[8..]).unwrap()
}

#[tokio::test]
async fn two_attesters_authorize_mint_and_reject_a_third_reuse() {
    let mut h = start_harness(RECEIPT_LOCKED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let args = mint_args(&h);
    let a = h.attester_a;
    let b = h.attester_b;
    approve(&mut h, &a, &args).await.unwrap();
    approve(&mut h, &b, &args).await.unwrap();
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_MINT_AUTHORIZED);
    let err = approve(&mut h, &a, &args).await.unwrap_err();
    assert!(format!("{err:?}").contains("0x177a") || format!("{err:?}").contains("Custom"));
}

#[tokio::test]
async fn mint_approval_fails_after_the_original_deadline() {
    let mut h = start_harness(RECEIPT_LOCKED, 1_500).await;
    set_clock(&mut h.ctx, 1_600);
    let args = mint_args(&h);
    let a = h.attester_a;
    assert!(approve(&mut h, &a, &args).await.is_err());
}

#[tokio::test]
async fn fresh_release_approval_is_allowed_after_mint_deadline() {
    let mut h = start_harness(RECEIPT_MINT_AUTHORIZED, 1_500).await;
    set_clock(&mut h.ctx, 1_600);
    let payout = Pubkey::new_unique();
    let args = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_RELEASE,
        destination: payout,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 3_000,
        proof_commitment: h.proof,
    };
    let a = h.attester_a;
    let b = h.attester_b;
    approve(&mut h, &a, &args).await.unwrap();
    approve(&mut h, &b, &args).await.unwrap();
    let approval = read_approval(&mut h, DIRECTION_RELEASE).await;
    assert_eq!(approval.expiry, 3_000);
    assert_eq!(approval.distinct_signers(), 2);
    assert!(!approval.consumed);
}

#[tokio::test]
async fn unexpired_approval_rejects_a_changed_digest() {
    let mut h = start_harness(RECEIPT_MINT_AUTHORIZED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let args = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_RELEASE,
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 1_500,
        proof_commitment: h.proof,
    };
    let a = h.attester_a;
    approve(&mut h, &a, &args).await.unwrap();
    let changed = ApproveArgs {
        expiry: 4_000,
        proof_commitment: [11u8; 32],
        ..args
    };
    let err = approve(&mut h, &a, &changed).await.unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("0x1780") || text.contains("Custom"),
        "changing the digest under a live approval must fail: {text}"
    );
    let approval = read_approval(&mut h, DIRECTION_RELEASE).await;
    assert_eq!(approval.expiry, 1_500);
    assert_eq!(approval.distinct_signers(), 1);
}

#[tokio::test]
async fn chain_time_equal_to_expiry_allows_replacement() {
    let mut h = start_harness(RECEIPT_MINT_AUTHORIZED, 1_500).await;
    set_clock(&mut h.ctx, 1_000);
    let args = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_RELEASE,
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 1_200,
        proof_commitment: h.proof,
    };
    let a = h.attester_a;
    approve(&mut h, &a, &args).await.unwrap();
    set_clock(&mut h.ctx, 1_200);
    let refreshed = ApproveArgs {
        expiry: 4_000,
        proof_commitment: [12u8; 32],
        ..args.clone()
    };
    approve(&mut h, &a, &refreshed).await.unwrap();
    let approval = read_approval(&mut h, DIRECTION_RELEASE).await;
    assert_eq!(approval.expiry, 4_000);
    assert_eq!(approval.distinct_signers(), 1);
}

#[tokio::test]
async fn expired_unused_approval_can_be_replaced_but_consumed_cannot() {
    let mut h = start_harness(RECEIPT_MINT_AUTHORIZED, 1_500).await;
    set_clock(&mut h.ctx, 1_000);
    let args = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_RELEASE,
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 1_200,
        proof_commitment: h.proof,
    };
    let a = h.attester_a;
    approve(&mut h, &a, &args).await.unwrap();
    set_clock(&mut h.ctx, 1_300);
    let refreshed = ApproveArgs {
        expiry: 4_000,
        ..args.clone()
    };
    approve(&mut h, &a, &refreshed).await.unwrap();
    let approval = read_approval(&mut h, DIRECTION_RELEASE).await;
    assert_eq!(approval.expiry, 4_000);
    assert_eq!(approval.distinct_signers(), 1);
    let (approval_key, approval_bump) =
        pda(&[APPROVAL_SEED, h.operation.as_ref(), &[DIRECTION_RELEASE]]);
    write_account(
        &mut h.ctx,
        approval_key,
        PROGRAM_ID,
        pack_approval(&OperationApproval {
            operation: h.operation,
            direction: DIRECTION_RELEASE,
            digest: approval.digest,
            signer_bitmap: approval.signer_bitmap,
            consumed: true,
            bump: approval_bump,
            expiry: 100,
        }),
    );
    let after_consumed = ApproveArgs {
        expiry: 5_000,
        ..refreshed
    };
    assert!(approve(&mut h, &a, &after_consumed).await.is_err());
}

#[tokio::test]
async fn cancel_after_mint_authorization_is_rejected() {
    let mut h = start_harness(RECEIPT_MINT_AUTHORIZED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let args = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_CANCEL,
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 2_500,
        proof_commitment: [0u8; 32],
    };
    let a = h.attester_a;
    let b = h.attester_b;
    approve(&mut h, &a, &args).await.unwrap();
    approve(&mut h, &b, &args).await.unwrap();
    let movement = MovementArgs {
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 2_500,
        transfer_data: [0u8; 167],
    };
    let ix = move_ix(
        "cancel_confidential",
        h.destination,
        h.mint,
        &movement,
        &h.operation,
        DIRECTION_CANCEL,
    );
    let blockhash = h.ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&h.ctx.payer.pubkey()),
        &[&h.ctx.payer],
        blockhash,
    );
    assert!(h.ctx.banks_client.process_transaction(tx).await.is_err());
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_MINT_AUTHORIZED);
}

#[tokio::test]
async fn release_without_threshold_and_failed_cpi_leave_receipt_unmoved() {
    let mut h = start_harness(RECEIPT_MINT_AUTHORIZED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let args = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_RELEASE,
        destination: Pubkey::new_unique(),
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 2_500,
        proof_commitment: h.proof,
    };
    let a = h.attester_a;
    let b = h.attester_b;
    approve(&mut h, &a, &args).await.unwrap();
    let movement = MovementArgs {
        destination: args.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 2_500,
        transfer_data: [0u8; 167],
    };
    let ix = move_ix(
        "release_confidential",
        args.destination,
        h.mint,
        &movement,
        &h.operation,
        DIRECTION_RELEASE,
    );
    let blockhash = h.ctx.banks_client.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&h.ctx.payer.pubkey()),
        &[&h.ctx.payer],
        blockhash,
    );
    assert!(h.ctx.banks_client.process_transaction(tx).await.is_err());
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_MINT_AUTHORIZED);
    approve(&mut h, &b, &args).await.unwrap();
    let tx2 = Transaction::new_signed_with_payer(
        &[move_ix(
            "release_confidential",
            args.destination,
            h.mint,
            &movement,
            &h.operation,
            DIRECTION_RELEASE,
        )],
        Some(&h.ctx.payer.pubkey()),
        &[&h.ctx.payer],
        blockhash,
    );
    assert!(h.ctx.banks_client.process_transaction(tx2).await.is_err());
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_MINT_AUTHORIZED);
    let approval = read_approval(&mut h, DIRECTION_RELEASE).await;
    assert!(!approval.consumed);
}

#[tokio::test]
async fn conflicting_attestation_does_not_count_and_quorum_can_recover() {
    let mut h = start_harness(RECEIPT_LOCKED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let args = mint_args(&h);
    let a = h.attester_a;
    let b = h.attester_b;
    approve(&mut h, &a, &args).await.unwrap();
    let conflicting = ApproveArgs {
        proof_commitment: [99u8; 32],
        ..args.clone()
    };
    assert!(approve(&mut h, &b, &conflicting).await.is_err());
    let approval = read_approval(&mut h, DIRECTION_MINT).await;
    assert_eq!(approval.distinct_signers(), 1);
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_LOCKED);
    approve(&mut h, &b, &args).await.unwrap();
    let recovered = read_approval(&mut h, DIRECTION_MINT).await;
    assert_eq!(recovered.distinct_signers(), 2);
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_MINT_AUTHORIZED);
}

#[tokio::test]
async fn cancel_remains_available_when_quorum_is_missing() {
    let mut h = start_harness(RECEIPT_LOCKED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let mint = mint_args(&h);
    let a = h.attester_a;
    let b = h.attester_b;
    approve(&mut h, &a, &mint).await.unwrap();
    let conflicting = ApproveArgs {
        proof_commitment: [99u8; 32],
        ..mint.clone()
    };
    assert!(approve(&mut h, &b, &conflicting).await.is_err());
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_LOCKED);
    let cancel = ApproveArgs {
        operation: h.operation,
        direction: DIRECTION_CANCEL,
        destination: h.destination,
        amount_commitment: h.amount_commitment,
        zama_reservation: h.reservation,
        previous_operation: h.previous,
        expiry: 2_500,
        proof_commitment: [0u8; 32],
    };
    approve(&mut h, &a, &cancel).await.unwrap();
    approve(&mut h, &b, &cancel).await.unwrap();
    let approval = read_approval(&mut h, DIRECTION_CANCEL).await;
    assert_eq!(approval.distinct_signers(), 2);
    assert!(!approval.consumed);
    let receipt = read_receipt(&mut h).await;
    assert_eq!(receipt.status, RECEIPT_LOCKED);
}

#[tokio::test]
async fn unknown_attester_is_rejected() {
    let mut h = start_harness(RECEIPT_LOCKED, 2_000).await;
    set_clock(&mut h.ctx, 1_000);
    let args = mint_args(&h);
    let outsider = [22u8; 32];
    assert!(approve(&mut h, &outsider, &args).await.is_err());
}
