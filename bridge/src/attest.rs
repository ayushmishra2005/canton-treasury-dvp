use anyhow::Result;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;

pub fn ed25519_approve_ix(attester: &Keypair, digest: &[u8; 32]) -> Result<Instruction> {
    let signature = attester.sign_message(digest);
    Ok(
        solana_sdk::ed25519_instruction::new_ed25519_instruction_with_signature(
            digest,
            signature.as_ref().try_into().unwrap(),
            &attester.pubkey().to_bytes(),
        ),
    )
}

pub fn attester_pubkey(attester: &Keypair) -> Pubkey {
    attester.pubkey()
}
