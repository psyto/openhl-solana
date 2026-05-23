//! Chapter 12 worked example — drives openhl-core's trading vault.
//!
//! Modes (mutually exclusive):
//!   --init --mint <pubkey>          → CreateTradingVault (tag 19)
//!                                     (manager == --payer in this script)
//!   --deposit --amount <u64>        → VaultDeposit (tag 20)
//!   --withdraw --shares <u64>       → VaultWithdraw (tag 21)
//!   --update-nav --total-assets <u64> → VaultUpdateNAV (tag 22)
//!   (no mode)                       → dump vault + share state
//!
//! The manager identity for a vault is fixed at --init time. For
//! --deposit / --withdraw / --update-nav, --manager names the manager
//! that binds the target vault (defaults to --payer if omitted).
//!
//! Usage:
//!   vault --rpc http://127.0.0.1:8899 \
//!         --payer ~/.config/solana/id.json \
//!         --program <openhl-core ID> \
//!         --market <market PDA> \
//!         [--manager <pubkey>] \
//!         [--init --mint <pubkey> |
//!          --deposit --amount 1000 |
//!          --withdraw --shares 100 |
//!          --update-nav --total-assets 1200]

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::{TRADING_VAULT_SEED, VAULT_SHARE_SEED};
use openhl_state::{TradingVault, VaultShare};
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
#[command(name = "vault", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    #[arg(long)]
    program: String,

    #[arg(long)]
    market: String,

    /// Vault manager pubkey. Defaults to payer.
    #[arg(long)]
    manager: Option<String>,

    #[arg(long)]
    init: bool,
    #[arg(long, requires = "init")]
    mint: Option<String>,

    #[arg(long)]
    deposit: bool,
    #[arg(long, requires = "deposit")]
    amount: Option<u64>,

    #[arg(long)]
    withdraw: bool,
    #[arg(long, requires = "withdraw")]
    shares: Option<u64>,

    #[arg(long)]
    update_nav: bool,
    #[arg(long, requires = "update_nav")]
    total_assets: Option<u64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    let manager: Pubkey = match cli.manager.as_deref() {
        Some(s) => s.parse().context("parse --manager")?,
        None => payer.pubkey(),
    };

    let (vault_pda, vault_bump) = Pubkey::find_program_address(
        &[TRADING_VAULT_SEED, market.as_ref(), manager.as_ref()],
        &program_id,
    );
    let (share_pda, _) = Pubkey::find_program_address(
        &[VAULT_SHARE_SEED, vault_pda.as_ref(), payer.pubkey().as_ref()],
        &program_id,
    );

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:        {}", cli.rpc);
    println!("payer:      {}", payer.pubkey());
    println!("program:    {program_id}");
    println!("market:     {market}");
    println!("manager:    {manager}");
    println!("vault PDA:  {vault_pda}  (bump {vault_bump})");
    println!("share PDA:  {share_pda}  (for payer={})", payer.pubkey());
    println!();

    let modes = [cli.init, cli.deposit, cli.withdraw, cli.update_nav]
        .iter()
        .filter(|b| **b)
        .count();
    if modes > 1 {
        bail!("--init, --deposit, --withdraw, --update-nav are mutually exclusive");
    }

    if cli.init {
        let mint: Pubkey = cli.mint.as_deref().unwrap().parse().context("parse --mint")?;
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new_readonly(manager, true),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(vault_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: vec![19u8],
        };
        send(&client, &payer, ix)?;
    } else if cli.deposit {
        let amount = cli.amount.unwrap();
        let mut data = Vec::with_capacity(1 + 8);
        data.push(20u8);
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(vault_pda, false),
                AccountMeta::new(share_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.withdraw {
        let shares = cli.shares.unwrap();
        let mut data = Vec::with_capacity(1 + 8);
        data.push(21u8);
        data.extend_from_slice(&shares.to_le_bytes());
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(vault_pda, false),
                AccountMeta::new(share_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.update_nav {
        let total = cli.total_assets.unwrap();
        let mut data = Vec::with_capacity(1 + 8);
        data.push(22u8);
        data.extend_from_slice(&total.to_le_bytes());
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(vault_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    }

    // Always dump.
    println!();
    println!("vault state:");
    match client.get_account(&vault_pda) {
        Ok(account) if account.data.len() >= TradingVault::LEN => {
            let vault: &TradingVault =
                bytemuck::from_bytes(&account.data[..TradingVault::LEN]);
            println!("  market:         {}", Pubkey::new_from_array(vault.market));
            println!("  manager:        {}", Pubkey::new_from_array(vault.manager));
            println!("  mint:           {}", Pubkey::new_from_array(vault.mint));
            println!("  total_shares:   {}", vault.total_shares);
            println!("  total_assets:   {}", vault.total_assets);
            if vault.total_shares > 0 {
                // NAV per share, in basis points of 1 asset
                let nav_bp =
                    (vault.total_assets as u128) * 10_000 / (vault.total_shares as u128);
                println!("  NAV per share:  {} (assets per 10000 shares)", nav_bp);
            }
        }
        _ => println!("  (vault does not exist yet)"),
    }

    println!();
    println!("share state for payer:");
    match client.get_account(&share_pda) {
        Ok(account) if account.data.len() >= VaultShare::LEN => {
            let share: &VaultShare = bytemuck::from_bytes(&account.data[..VaultShare::LEN]);
            println!("  owner:        {}", Pubkey::new_from_array(share.owner));
            println!("  shares:       {}", share.shares);
            println!("  cost_basis:   {}", share.cost_basis);
        }
        _ => println!("  (no shares yet)"),
    }

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
