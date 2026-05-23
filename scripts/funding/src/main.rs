//! Chapter 10 worked example — drives openhl-core's funding instructions.
//!
//! Modes (mutually exclusive):
//!   --init [--window-seconds N]   → CreateFundingState (default 3600s)
//!   --update --rate <i64>         → UpdateFunding
//!   (no mode)                     → dump cumulative_funding_index +
//!                                    last_update_ts + elapsed seconds
//!
//! Run two or three --update calls a few seconds apart to watch the
//! cumulative_funding_index grow under the prior interval's rate.

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::FUNDING_SEED;
use openhl_state::FundingState;
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
#[command(name = "funding", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    #[arg(long)]
    program: String,

    #[arg(long)]
    market: String,

    #[arg(long)]
    init: bool,
    #[arg(long, default_value_t = FundingState::DEFAULT_WINDOW_SECONDS)]
    window_seconds: u64,

    #[arg(long)]
    update: bool,
    #[arg(long, requires = "update")]
    rate: Option<i64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    let (funding_pda, bump) =
        Pubkey::find_program_address(&[FUNDING_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:         {}", cli.rpc);
    println!("payer:       {}", payer.pubkey());
    println!("program:     {program_id}");
    println!("market:      {market}");
    println!("funding PDA: {funding_pda}  (bump {bump})");
    println!();

    if cli.init && cli.update {
        bail!("--init and --update are mutually exclusive");
    }

    if cli.init {
        let mut data = Vec::with_capacity(1 + 8);
        data.push(14u8);
        data.extend_from_slice(&cli.window_seconds.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(funding_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.update {
        let rate = cli.rate.ok_or_else(|| anyhow::anyhow!("--rate required"))?;
        let mut data = Vec::with_capacity(1 + 8);
        data.push(15u8);
        data.extend_from_slice(&rate.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(funding_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    }

    // Always dump.
    println!();
    println!("funding state:");
    let account = client.get_account(&funding_pda).context("fetch funding account")?;
    let funding: &FundingState = bytemuck::from_bytes(&account.data[..FundingState::LEN]);
    println!("  market:                     {}", Pubkey::new_from_array(funding.market));
    println!("  cumulative_funding_index:   {} (scaled 1e9)", funding.cumulative_funding_index);
    println!("  current_rate_per_sec:       {} (scaled 1e9)", funding.current_rate_per_sec);
    println!("  window_seconds:             {}", funding.window_seconds);
    println!("  last_update_ts:             {}", funding.last_update_ts);
    println!("  last_update_slot:           {}", funding.last_update_slot);

    let current_slot = client.get_slot().context("fetch current slot")?;
    println!();
    println!("  current_slot:               {}", current_slot);
    println!("  slots_since_update:         {}", current_slot.saturating_sub(funding.last_update_slot));

    Ok(())
}

fn send(
    client: &RpcClient,
    payer: &solana_sdk::signature::Keypair,
    ix: Instruction,
) -> Result<()> {
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
    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer], blockhash);
    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send transaction")?;
    println!("signature: {sig}");
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
