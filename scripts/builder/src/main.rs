//! Chapter 13 worked example — drives openhl-core's builder-code
//! instructions, now with real SPL Token fee escrow.
//!
//! Modes (mutually exclusive):
//!   --create-fee-vault --mint <pubkey>        → CreateFeeVault (tag 26)
//!                                               One-shot bootstrap per
//!                                               quote_mint. Run once per
//!                                               cluster before any
//!                                               place-order variant.
//!   --register --max-share-bps <u64>          → RegisterBuilder (tag 23)
//!                                               (registers --payer as builder)
//!   --place-with-builder --builder <pubkey>
//!                        --market <pubkey>
//!                        --mint <pubkey>
//!                        --user-token-account <pubkey>
//!                        --side bid|ask
//!                        --price <u64>
//!                        --size <u64>          → PlaceOrderWithBuilder (tag 24)
//!   --claim --mint <pubkey>
//!           --builder-token-account <pubkey>  → ClaimBuilderFees (tag 25)
//!                                               (claims --payer's accumulated)
//!   (no mode)                                 → dump BuilderProfile for --payer
//!                                               (or --builder if provided)

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::{
    BOOK_SEED, BUILDER_PROFILE_SEED, FEE_VAULT_AUTH_SEED, FEE_VAULT_SEED, ORACLE_SEED,
    SPL_TOKEN_PROGRAM_ID,
};
use openhl_state::{side, BuilderProfile};
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
#[command(name = "builder", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    #[arg(long)]
    program: String,

    /// For --register / --claim, this is the builder being acted on
    /// (defaults to payer). For dump-default, names which profile to show.
    /// For --place-with-builder, names the builder whose fee share is
    /// being credited.
    #[arg(long)]
    builder: Option<String>,

    #[arg(long)]
    create_fee_vault: bool,

    #[arg(long)]
    register: bool,
    #[arg(long, requires = "register")]
    max_share_bps: Option<u64>,

    #[arg(long)]
    place_with_builder: bool,
    #[arg(long)]
    market: Option<String>,
    #[arg(long)]
    side: Option<String>,
    #[arg(long)]
    price: Option<u64>,
    #[arg(long)]
    size: Option<u64>,

    #[arg(long)]
    claim: bool,

    /// Quote mint. Required for --create-fee-vault, --place-with-builder,
    /// --claim.
    #[arg(long)]
    mint: Option<String>,

    /// Trader's quote-token account. Required for --place-with-builder.
    #[arg(long)]
    user_token_account: Option<String>,

    /// Builder's quote-token account (claim destination). Required for --claim.
    #[arg(long)]
    builder_token_account: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;

    let builder_pk: Pubkey = match cli.builder.as_deref() {
        Some(s) => s.parse().context("parse --builder")?,
        None => payer.pubkey(),
    };

    let (profile_pda, _) = Pubkey::find_program_address(
        &[BUILDER_PROFILE_SEED, builder_pk.as_ref()],
        &program_id,
    );

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:         {}", cli.rpc);
    println!("payer:       {}", payer.pubkey());
    println!("program:     {program_id}");
    println!("builder:     {builder_pk}");
    println!("profile PDA: {profile_pda}");
    println!();

    let modes = [
        cli.create_fee_vault,
        cli.register,
        cli.place_with_builder,
        cli.claim,
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if modes > 1 {
        bail!("--create-fee-vault, --register, --place-with-builder, --claim are mutually exclusive");
    }

    if cli.create_fee_vault {
        let mint = require_mint(&cli)?;
        let (fee_vault, _) =
            Pubkey::find_program_address(&[FEE_VAULT_SEED, mint.as_ref()], &program_id);
        let (fee_vault_auth, _) =
            Pubkey::find_program_address(&[FEE_VAULT_AUTH_SEED, mint.as_ref()], &program_id);

        println!("fee_vault PDA:        {fee_vault}");
        println!("fee_vault_auth PDA:   {fee_vault_auth}");

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(fee_vault, false),
                AccountMeta::new_readonly(fee_vault_auth, false),
                AccountMeta::new_readonly(system_program::ID, false),
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
            ],
            data: vec![26u8],
        };
        send(&client, &payer, ix)?;
    } else if cli.register {
        let max = cli.max_share_bps.unwrap();
        let mut data = Vec::with_capacity(1 + 8);
        data.push(23u8);
        data.extend_from_slice(&max.to_le_bytes());
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(profile_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.place_with_builder {
        let market: Pubkey = cli
            .market
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--market required for --place-with-builder"))?
            .parse()
            .context("parse --market")?;
        let mint = require_mint(&cli)?;
        let user_token: Pubkey = cli
            .user_token_account
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--user-token-account required for --place-with-builder"))?
            .parse()
            .context("parse --user-token-account")?;
        let side_byte = match cli.side.as_deref() {
            Some("bid") => side::BID,
            Some("ask") => side::ASK,
            _ => bail!("--side bid|ask required"),
        };
        let price = cli.price.ok_or_else(|| anyhow::anyhow!("--price required"))?;
        let size = cli.size.ok_or_else(|| anyhow::anyhow!("--size required"))?;

        let (book_pda, _) =
            Pubkey::find_program_address(&[BOOK_SEED, market.as_ref()], &program_id);
        let (oracle_pda, _) =
            Pubkey::find_program_address(&[ORACLE_SEED, market.as_ref()], &program_id);
        let (fee_vault, _) =
            Pubkey::find_program_address(&[FEE_VAULT_SEED, mint.as_ref()], &program_id);

        let mut data = Vec::with_capacity(1 + 1 + 8 + 8);
        data.push(24u8);
        data.push(side_byte);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(book_pda, false),
                AccountMeta::new_readonly(oracle_pda, false),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(user_token, false),
                AccountMeta::new(fee_vault, false),
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                AccountMeta::new(profile_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.claim {
        let mint = require_mint(&cli)?;
        let builder_token: Pubkey = cli
            .builder_token_account
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--builder-token-account required for --claim"))?
            .parse()
            .context("parse --builder-token-account")?;

        let (fee_vault, _) =
            Pubkey::find_program_address(&[FEE_VAULT_SEED, mint.as_ref()], &program_id);
        let (fee_vault_auth, _) =
            Pubkey::find_program_address(&[FEE_VAULT_AUTH_SEED, mint.as_ref()], &program_id);

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(profile_pda, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(fee_vault, false),
                AccountMeta::new_readonly(fee_vault_auth, false),
                AccountMeta::new(builder_token, false),
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
            ],
            data: vec![25u8],
        };
        send(&client, &payer, ix)?;
    }

    // Always dump.
    println!();
    println!("builder profile:");
    match client.get_account(&profile_pda) {
        Ok(account) if account.data.len() >= BuilderProfile::LEN => {
            let profile: &BuilderProfile =
                bytemuck::from_bytes(&account.data[..BuilderProfile::LEN]);
            println!("  builder:            {}", Pubkey::new_from_array(profile.builder));
            println!("  max_fee_share_bps:  {}", profile.max_fee_share_bps);
            println!("  accumulated_fees:   {}", profile.accumulated_fees);
            println!("  total_volume:       {}", profile.total_volume);
        }
        _ => println!("  (profile does not exist yet)"),
    }

    Ok(())
}

fn require_mint(cli: &Cli) -> Result<Pubkey> {
    cli.mint
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--mint required for this mode"))?
        .parse()
        .context("parse --mint")
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
