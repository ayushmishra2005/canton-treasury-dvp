use anyhow::Result;
use clap::{Parser, Subcommand};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Keypair;
use std::path::PathBuf;

use canton_treasury_dvp_bridge::canton::CantonClient;
use canton_treasury_dvp_bridge::journal::{OperationStore, Step};
use canton_treasury_dvp_bridge::relayer::RelayerClient;
use canton_treasury_dvp_bridge::workflow::Workflow;
use canton_treasury_dvp_bridge::zama::ZamaClient;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Workflow {
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        journal: Option<PathBuf>,
        #[arg(long)]
        stop_after: Option<String>,
        #[arg(long)]
        expiry_recovery: bool,
        #[arg(long)]
        reuse_from: Option<PathBuf>,
        #[arg(long)]
        omit_journal_save: bool,
        #[arg(long)]
        halt_after_first_approval: bool,
        #[arg(long)]
        inject_attester_disagreement: bool,
        #[arg(long)]
        inject_unknown_attester: bool,
        #[arg(long)]
        cancel_locked: bool,
        #[arg(long)]
        reverse_endpoints: bool,
    },
    RelayerProof,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args {
        Args {
            command:
                Command::Workflow {
                    resume,
                    journal,
                    stop_after,
                    expiry_recovery,
                    reuse_from,
                    omit_journal_save,
                    halt_after_first_approval,
                    inject_attester_disagreement,
                    inject_unknown_attester,
                    cancel_locked,
                    reverse_endpoints,
                },
        } => run_workflow(
            true,
            resume,
            journal,
            stop_after,
            expiry_recovery,
            reuse_from,
            omit_journal_save,
            halt_after_first_approval,
            inject_attester_disagreement,
            inject_unknown_attester,
            cancel_locked,
            reverse_endpoints,
        ),
        Args {
            command: Command::RelayerProof,
        } => run_workflow(
            false, false, None, None, false, None, false, false, false, false, false, false,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_workflow(
    full: bool,
    _resume: bool,
    journal: Option<PathBuf>,
    stop_after: Option<String>,
    expiry_recovery: bool,
    reuse_from: Option<PathBuf>,
    omit_journal_save: bool,
    halt_after_first_approval: bool,
    inject_attester_disagreement: bool,
    inject_unknown_attester: bool,
    cancel_locked: bool,
    reverse_endpoints: bool,
) -> Result<()> {
    let rpc =
        RpcClient::new_with_commitment(required("SOLANA_RPC_URL"), CommitmentConfig::confirmed());
    let relayer = RelayerClient::new(
        required("RELAYER_URL"),
        required("RELAYER_API_KEY"),
        required("RELAYER_ID"),
    )?;
    let zama = ZamaClient::new(
        optional_env("ZAMA_RPC_URL", "http://127.0.0.1:8545"),
        optional_env("ZAMA_ENGINE", "0x0000000000000000000000000000000000000001"),
        optional_env(
            "ZAMA_REQUESTER_KEY",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ),
        optional_env(
            "ZAMA_SETTLER_KEY",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ),
        optional_env("ZAMA_CLIENT", "0x01"),
    );
    let canton = CantonClient::new(
        PathBuf::from("daml/bridge-tests/.daml/dist/canton-treasury-dvp-bridge-tests-0.1.0.dar"),
        PathBuf::from(
            std::env::var("CANTON_PARTICIPANTS")
                .unwrap_or_else(|_| "canton/.run-bridge/participants.json".to_string()),
        ),
    );
    let journal_dir = journal.unwrap_or_else(|| {
        PathBuf::from(optional_env("BRIDGE_JOURNAL_DIR", "bridge/.run/current"))
    });
    let store = OperationStore::open(journal_dir)?;
    let workflow = Workflow {
        rpc,
        relayer,
        zama,
        canton,
        payer: optional_keypair("BRIDGE_PAYER").unwrap_or_else(Keypair::new),
        attester_a: optional_keypair("ATTESTER_A").unwrap_or_else(Keypair::new),
        attester_b: optional_keypair("ATTESTER_B").unwrap_or_else(Keypair::new),
        attester_c: optional_keypair("ATTESTER_C").unwrap_or_else(Keypair::new),
        store,
        stop_after: stop_after
            .or_else(|| std::env::var("BRIDGE_STOP_AFTER").ok())
            .map(|name| Step::parse(&name))
            .transpose()?,
        expiry_recovery,
        reuse_from,
        omit_journal_save,
        halt_after_first_approval,
        inject_attester_disagreement,
        inject_unknown_attester,
        cancel_locked,
        reverse_endpoints,
    };
    let tokens = required("BRIDGE_AMOUNT").parse()?;
    if full {
        workflow.run(tokens)?;
    } else {
        workflow.prove_relayer(tokens)?;
    }
    Ok(())
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn optional_env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn optional_keypair(name: &str) -> Option<Keypair> {
    let path = std::env::var(name).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let secret: Vec<u8> = serde_json::from_slice(&bytes).ok()?;
    Keypair::try_from(secret.as_slice()).ok()
}
