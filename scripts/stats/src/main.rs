//! Chapter 5 worked example — drives the openhl-core program's Stats
//! singleton.
//!
//! Two modes:
//!   --init   →  call CreateStats once to create the singleton Stats PDA
//!   default  →  call BumpStats to increment market_count
//!
//! The interesting thing about this script is not what it *does* but
//! what its `AccountMeta` arrays *declare*. Chapter 5 dissects each
//! variant: how many writable accounts, how many readonly accounts, and
//! which of them are program-wide singletons vs. per-call PDAs. That
//! declaration is what Sealevel sees when it decides whether two
//! transactions can run in parallel.
//!
//! Usage:
//!   stats --rpc http://127.0.0.1:8899 \
//!         --payer ~/.config/solana/id.json \
//!         --program <openhl-core program ID> \
//!         [--init]

use anyhow::{Context, Result};
use clap::Parser;
use openhl_core::STATS_SEED;
use openhl_state::Stats;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;

#[derive(Parser, Debug)]
#[command(name = "stats", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    /// Deployed openhl-core program ID.
    #[arg(long)]
    program: String,

    /// One-shot creation of the singleton Stats PDA. Run this once per
    /// program deployment before any BumpStats call.
    #[arg(long)]
    init: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;

    let (stats_pda, bump) = Pubkey::find_program_address(&[STATS_SEED], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:        {}", cli.rpc);
    println!("payer:      {}", payer.pubkey());
    println!("program:    {program_id}");
    println!("stats PDA:  {stats_pda}  (bump {bump})");
    println!("mode:       {}", if cli.init { "CreateStats (init)" } else { "BumpStats" });
    println!();

    let ix = if cli.init {
        // CreateStats — three accounts: payer (W,S), stats (W), system (R).
        // Stats is a fixed singleton — every CreateStats call writes the
        // same pubkey, so two concurrent CreateStats transactions would
        // serialize at the scheduler. This is fine for a one-shot
        // initialization that runs once per deployment.
        Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(stats_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: vec![3u8], // tag = CreateStats, empty payload
        }
    } else {
        // BumpStats — one account: stats (W). No payer signer needed because
        // BumpStats moves no lamports. Every BumpStats writes the same
        // pubkey — Sealevel's view: any two BumpStats transactions
        // serialize. This is the antipattern Chapter 5 critiques.
        Instruction {
            program_id,
            accounts: vec![AccountMeta::new(stats_pda, false)],
            data: vec![4u8], // tag = BumpStats, empty payload
        }
    };

    println!("AccountMeta declared:");
    for (i, m) in ix.accounts.iter().enumerate() {
        let mode = match (m.is_writable, m.is_signer) {
            (true, true) => "WRITE + SIGNER",
            (true, false) => "WRITE",
            (false, true) => "SIGNER",
            (false, false) => "READ",
        };
        println!("  [{i}] {} {}", m.pubkey, mode);
    }
    println!();

    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send transaction")?;
    println!("signature: {sig}");

    // Echo the post-state so the user can see market_count tick.
    let account = client
        .get_account(&stats_pda)
        .context("fetch stats account")?;
    let stats: &Stats = bytemuck::from_bytes(&account.data[..Stats::LEN]);
    println!();
    println!("stats post-state:");
    println!("  market_count: {}", stats.market_count);
    println!("  bump:         {}", stats.bump);

    Ok(())
}

fn expand_tilde(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{stripped}");
        }
    }
    path.to_string()
}
