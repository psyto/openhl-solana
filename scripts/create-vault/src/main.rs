//! Chapter 6 worked example — drives openhl-core::CreateVault.
//!
//! Given an existing market PDA (from Chapter 3) and an SPL Mint, derives
//! the (market, mint) vault token-account PDA and the (market) vault
//! authority PDA, then submits a single instruction with seven accounts.
//! The program does two CPIs inside: System::create_account (invoke_signed)
//! and SPL Token InitializeAccount3 (invoke).
//!
//! Usage:
//!   create-vault --rpc http://127.0.0.1:8899 \
//!                --payer ~/.config/solana/id.json \
//!                --program <openhl-core program ID> \
//!                --market <market PDA> \
//!                --mint <SPL mint>

use anyhow::{Context, Result};
use clap::Parser;
use openhl_core::{SPL_TOKEN_PROGRAM_ID, VAULT_AUTH_SEED, VAULT_SEED};
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
#[command(name = "create-vault", version, about)]
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
    mint: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;
    let mint: Pubkey = cli.mint.parse().context("parse --mint")?;

    let (vault_pda, vault_bump) = Pubkey::find_program_address(
        &[VAULT_SEED, market.as_ref(), mint.as_ref()],
        &program_id,
    );
    let (vault_auth_pda, auth_bump) =
        Pubkey::find_program_address(&[VAULT_AUTH_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:              {}", cli.rpc);
    println!("payer:            {}", payer.pubkey());
    println!("program:          {program_id}");
    println!("market:           {market}");
    println!("mint:             {mint}");
    println!("vault PDA:        {vault_pda}  (bump {vault_bump})");
    println!("vault_auth PDA:   {vault_auth_pda}  (bump {auth_bump})");
    println!();

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(vault_pda, false),
            AccountMeta::new_readonly(vault_auth_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
        ],
        data: vec![5u8], // tag = CreateVault, empty payload
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
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);

    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send create_vault transaction")?;
    println!("signature: {sig}");

    // Echo post-state: fetch the new token account and dump its first few bytes.
    let account = client
        .get_account(&vault_pda)
        .context("fetch new vault token account")?;
    println!();
    println!("vault token account post-state:");
    println!("  owner:        {}  (expected SPL Token)", account.owner);
    println!("  lamports:     {}", account.lamports);
    println!("  data length:  {}  (expected 165)", account.data.len());

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
