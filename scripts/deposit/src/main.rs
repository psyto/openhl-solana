//! Chapter 6 worked example — drives openhl-core::Deposit.
//!
//! Transfers SPL tokens from the user's token account into the
//! market-owned vault. The user signs at the outer transaction level;
//! the program then re-emits the user in its CPI to SPL Token Transfer,
//! and the runtime carries the signer privilege through.
//!
//! Prerequisites you set up yourself:
//!   - `create-vault` has run for the given (market, mint)
//!   - `--user-token-account` already exists with at least --amount tokens
//!     of the same mint that backs the vault
//!
//! Usage:
//!   deposit --rpc http://127.0.0.1:8899 \
//!           --user ~/.config/solana/id.json \
//!           --program <openhl-core program ID> \
//!           --user-token-account <pubkey> \
//!           --vault <vault PDA from create-vault> \
//!           --amount 1000000

use anyhow::{Context, Result};
use clap::Parser;
use openhl_core::SPL_TOKEN_PROGRAM_ID;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};

#[derive(Parser, Debug)]
#[command(name = "deposit", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    /// Keypair that owns --user-token-account in SPL Token terms.
    #[arg(long, default_value = "~/.config/solana/id.json")]
    user: String,

    #[arg(long)]
    program: String,

    #[arg(long)]
    user_token_account: String,

    #[arg(long)]
    vault: String,

    /// Amount in the mint's base units (e.g., for USDC with 6 decimals,
    /// 1_000_000 = 1.00 USDC).
    #[arg(long)]
    amount: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let user = read_keypair_file(&expand_tilde(&cli.user))
        .map_err(|e| anyhow::anyhow!("read user keypair: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let user_token: Pubkey = cli.user_token_account.parse().context("parse --user-token-account")?;
    let vault: Pubkey = cli.vault.parse().context("parse --vault")?;

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:                 {}", cli.rpc);
    println!("user:                {}", user.pubkey());
    println!("program:             {program_id}");
    println!("user_token_account:  {user_token}");
    println!("vault:               {vault}");
    println!("amount:              {}", cli.amount);
    println!();

    let mut data = Vec::with_capacity(1 + 8);
    data.push(6u8); // tag = Deposit
    data.extend_from_slice(&cli.amount.to_le_bytes());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(user_token, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
        ],
        data,
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
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&user.pubkey()), &[&user], blockhash);

    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send deposit transaction")?;
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
