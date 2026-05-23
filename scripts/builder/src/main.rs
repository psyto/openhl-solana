//! Chapter 13 worked example — drives openhl-core's builder-code
//! instructions.
//!
//! Modes (mutually exclusive):
//!   --register --max-share-bps <u64>          → RegisterBuilder (tag 23)
//!                                               (registers --payer as builder)
//!   --place-with-builder --builder <pubkey>
//!                        --market <pubkey>
//!                        --side bid|ask
//!                        --price <u64>
//!                        --size <u64>          → PlaceOrderWithBuilder (tag 24)
//!   --claim                                   → ClaimBuilderFees (tag 25)
//!                                               (claims --payer's accumulated)
//!   (no mode)                                 → dump BuilderProfile for --payer
//!                                               (or --builder if provided)
//!
//! Note: --market is only required for --place-with-builder mode; other
//! modes don't need it.

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::{BOOK_SEED, BUILDER_PROFILE_SEED, ORACLE_SEED};
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
    register: bool,
    #[arg(long, requires = "register")]
    max_share_bps: Option<u64>,

    #[arg(long)]
    place_with_builder: bool,
    #[arg(long, requires = "place_with_builder")]
    market: Option<String>,
    #[arg(long, requires = "place_with_builder")]
    side: Option<String>,
    #[arg(long, requires = "place_with_builder")]
    price: Option<u64>,
    #[arg(long, requires = "place_with_builder")]
    size: Option<u64>,

    #[arg(long)]
    claim: bool,
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

    let modes = [cli.register, cli.place_with_builder, cli.claim]
        .iter()
        .filter(|b| **b)
        .count();
    if modes > 1 {
        bail!("--register, --place-with-builder, --claim are mutually exclusive");
    }

    if cli.register {
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
        let market: Pubkey = cli.market.as_deref().unwrap().parse().context("parse --market")?;
        let side_byte = match cli.side.as_deref() {
            Some("bid") => side::BID,
            Some("ask") => side::ASK,
            _ => bail!("--side bid|ask required"),
        };
        let price = cli.price.unwrap();
        let size = cli.size.unwrap();

        let (book_pda, _) =
            Pubkey::find_program_address(&[BOOK_SEED, market.as_ref()], &program_id);
        let (oracle_pda, _) =
            Pubkey::find_program_address(&[ORACLE_SEED, market.as_ref()], &program_id);

        let mut data = Vec::with_capacity(1 + 1 + 8 + 8);
        data.push(24u8);
        data.push(side_byte);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(book_pda, false),
                AccountMeta::new_readonly(oracle_pda, false),
                AccountMeta::new(profile_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.claim {
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(profile_pda, false),
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
