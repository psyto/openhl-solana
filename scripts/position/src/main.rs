//! Chapter 11 worked example — drives openhl-core's position lifecycle.
//!
//! Modes (mutually exclusive):
//!   --open --size <i64> --collateral <u64>   → OpenPosition (tag 16)
//!   --close                                  → ClosePosition (tag 17)
//!   --liquidate --target-user <pubkey>       → Liquidate (tag 18)
//!   (no mode)                                → dump position + computed
//!                                              equity/notional/maint margin
//!
//! All three mutating modes now require --mint (the quote-asset SPL Mint
//! the market vault holds) and SPL Token accounts. The vault PDA and
//! vault-authority PDA at [b"vault", market, mint] / [b"vault_auth",
//! market] must already exist — create them with `create-vault` from
//! Chapter 6 if needed.
//!
//! Usage:
//!   position --rpc http://127.0.0.1:8899 \
//!            --payer ~/.config/solana/id.json \
//!            --program <openhl-core program ID> \
//!            --market <market PDA> \
//!            --mint <quote SPL mint> \
//!            [--open --size 5 --collateral 100
//!                   --user-token-account <user's quote token account> |
//!             --close
//!                   --user-token-account <pubkey> |
//!             --liquidate --target-user <pubkey>
//!                         --owner-token-account <pubkey>
//!                         --liquidator-token-account <pubkey>]

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::{
    FUNDING_SEED, LIQUIDATION_PENALTY_BPS, MAINT_MARGIN_BPS, ORACLE_SEED, POSITION_SEED,
    SPL_TOKEN_PROGRAM_ID, VAULT_AUTH_SEED, VAULT_SEED,
};
use openhl_state::{Oracle, Position};
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
#[command(name = "position", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    #[arg(long)]
    program: String,

    #[arg(long)]
    market: String,

    /// Quote-asset SPL Mint. Required for --open / --close / --liquidate.
    #[arg(long)]
    mint: Option<String>,

    #[arg(long)]
    open: bool,
    #[arg(long, requires = "open")]
    size: Option<i64>,
    #[arg(long, requires = "open")]
    collateral: Option<u64>,

    #[arg(long)]
    close: bool,

    #[arg(long)]
    liquidate: bool,
    #[arg(long, requires = "liquidate")]
    target_user: Option<String>,

    /// User's SPL Token Account holding the quote mint. Required for
    /// --open (source of collateral) and --close (destination of payout).
    #[arg(long)]
    user_token_account: Option<String>,

    /// Position owner's SPL Token Account (gets the remainder after
    /// liquidation penalty). Required for --liquidate.
    #[arg(long)]
    owner_token_account: Option<String>,

    /// Liquidator's SPL Token Account (gets the liquidation penalty).
    /// Required for --liquidate. Typically the liquidator's own ATA.
    #[arg(long)]
    liquidator_token_account: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    // Determine whose position we're targeting.
    let position_user: Pubkey = if cli.liquidate {
        cli.target_user
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--target-user required with --liquidate"))?
            .parse()
            .context("parse --target-user")?
    } else {
        payer.pubkey()
    };

    let (position_pda, bump) = Pubkey::find_program_address(
        &[POSITION_SEED, position_user.as_ref(), market.as_ref()],
        &program_id,
    );
    let (oracle_pda, _) =
        Pubkey::find_program_address(&[ORACLE_SEED, market.as_ref()], &program_id);
    let (funding_pda, _) =
        Pubkey::find_program_address(&[FUNDING_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:           {}", cli.rpc);
    println!("payer:         {}", payer.pubkey());
    println!("program:       {program_id}");
    println!("market:        {market}");
    println!("position user: {position_user}");
    println!("position PDA:  {position_pda}  (bump {bump})");
    println!("oracle PDA:    {oracle_pda}");
    println!("funding PDA:   {funding_pda}");

    if cli.open || cli.close || cli.liquidate {
        let mint: Pubkey = cli
            .mint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--mint required for --open/--close/--liquidate"))?
            .parse()
            .context("parse --mint")?;
        let (vault_pda, _) = Pubkey::find_program_address(
            &[VAULT_SEED, market.as_ref(), mint.as_ref()],
            &program_id,
        );
        let (vault_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_AUTH_SEED, market.as_ref()],
            &program_id,
        );
        println!("mint:          {mint}");
        println!("vault PDA:     {vault_pda}");
        println!("vault_auth:    {vault_auth_pda}");
        println!();

        let modes_set = [cli.open, cli.close, cli.liquidate].iter().filter(|b| **b).count();
        if modes_set > 1 {
            bail!("--open, --close, --liquidate are mutually exclusive");
        }

        if cli.open {
            let size = cli.size.ok_or_else(|| anyhow::anyhow!("--size required"))?;
            let collateral =
                cli.collateral.ok_or_else(|| anyhow::anyhow!("--collateral required"))?;
            let user_token: Pubkey = cli
                .user_token_account
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--user-token-account required for --open"))?
                .parse()
                .context("parse --user-token-account")?;

            let mut data = Vec::with_capacity(1 + 8 + 8);
            data.push(16u8);
            data.extend_from_slice(&size.to_le_bytes());
            data.extend_from_slice(&collateral.to_le_bytes());

            let ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new_readonly(market, false),
                    AccountMeta::new(position_pda, false),
                    AccountMeta::new_readonly(oracle_pda, false),
                    AccountMeta::new_readonly(funding_pda, false),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new(user_token, false),
                    AccountMeta::new(vault_pda, false),
                    AccountMeta::new_readonly(system_program::ID, false),
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                ],
                data,
            };
            send(&client, &payer, ix)?;
        } else if cli.close {
            let user_token: Pubkey = cli
                .user_token_account
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--user-token-account required for --close"))?
                .parse()
                .context("parse --user-token-account")?;

            let ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new(position_pda, false),
                    AccountMeta::new_readonly(oracle_pda, false),
                    AccountMeta::new_readonly(funding_pda, false),
                    AccountMeta::new_readonly(market, false),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new(user_token, false),
                    AccountMeta::new(vault_pda, false),
                    AccountMeta::new_readonly(vault_auth_pda, false),
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                ],
                data: vec![17u8],
            };
            send(&client, &payer, ix)?;
        } else if cli.liquidate {
            let owner_token: Pubkey = cli
                .owner_token_account
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--owner-token-account required"))?
                .parse()
                .context("parse --owner-token-account")?;
            let liq_token: Pubkey = cli
                .liquidator_token_account
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--liquidator-token-account required"))?
                .parse()
                .context("parse --liquidator-token-account")?;

            let ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new(position_pda, false),
                    AccountMeta::new_readonly(oracle_pda, false),
                    AccountMeta::new_readonly(funding_pda, false),
                    AccountMeta::new_readonly(market, false),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new(owner_token, false),
                    AccountMeta::new(liq_token, false),
                    AccountMeta::new(vault_pda, false),
                    AccountMeta::new_readonly(vault_auth_pda, false),
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                ],
                data: vec![18u8],
            };
            send(&client, &payer, ix)?;
        }
    } else {
        println!();
    }

    // Always dump position post-state.
    println!();
    println!("position state:");
    match client.get_account(&position_pda) {
        Ok(account) if account.data.len() >= Position::LEN => {
            let position: &Position = bytemuck::from_bytes(&account.data[..Position::LEN]);
            println!("  user:                   {}", Pubkey::new_from_array(position.user));
            println!("  market:                 {}", Pubkey::new_from_array(position.market));
            println!("  size:                   {}", position.size);
            println!("  entry_price:            {}", position.entry_price);
            println!("  collateral:             {}", position.collateral);
            println!("  funding_snapshot_index: {}", position.funding_snapshot_index);

            // Compute derived metrics (same math the program does).
            if let Ok(oracle_account) = client.get_account(&oracle_pda) {
                if oracle_account.data.len() >= Oracle::LEN {
                    let oracle: &Oracle = bytemuck::from_bytes(&oracle_account.data[..Oracle::LEN]);
                    let mark = oracle.price as i128;
                    let size = position.size as i128;
                    let entry = position.entry_price as i128;
                    let notional = (size.unsigned_abs()) * (mark as u128);
                    let price_pnl = size * (mark - entry);
                    let equity = (position.collateral as i128) + price_pnl;
                    let maint = notional * (MAINT_MARGIN_BPS as u128) / 10_000;
                    let liq_pen = notional * (LIQUIDATION_PENALTY_BPS as u128) / 10_000;
                    println!();
                    println!("  mark:                   {}", oracle.price);
                    println!("  notional:               {notional}");
                    println!("  price_pnl:              {price_pnl} (funding pnl skipped here)");
                    println!("  equity (ex-funding):    {equity}");
                    println!("  maintenance margin:     {maint}");
                    println!(
                        "  liquidatable:           {}",
                        if equity < (maint as i128) { "YES" } else { "no" }
                    );
                    println!("  liquidation penalty:    {liq_pen}");
                }
            }
        }
        Ok(_) => println!("  (position account exists but is wrong size)"),
        Err(_) => println!("  (position account does not exist yet)"),
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
