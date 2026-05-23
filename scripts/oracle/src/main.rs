//! Chapter 9 worked example — drives openhl-core's oracle instructions.
//!
//! Modes (mutually exclusive):
//!   --init                          → CreateOracle (tag 11)
//!   --set --price <i64>
//!         --conf  <u64>
//!         --expo  <i32>             → SetOraclePrice (tag 12)
//!   (no mode)                       → fetch + dump current Oracle state
//!
//! In all modes the script fetches the oracle account afterward and
//! prints price / conf / expo / publish_slot so staleness scenarios
//! are observable across multiple invocations.
//!
//! Usage:
//!   oracle --rpc http://127.0.0.1:8899 \
//!          --payer ~/.config/solana/id.json \
//!          --program <openhl-core program ID> \
//!          --market <market PDA> \
//!          [--init | --set --price 100 --conf 1 --expo 0]

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::ORACLE_SEED;
use openhl_state::Oracle;
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
#[command(name = "oracle", version, about)]
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

    #[arg(long)]
    set: bool,
    #[arg(long, requires = "set")]
    price: Option<i64>,
    #[arg(long, requires = "set")]
    conf: Option<u64>,
    #[arg(long, requires = "set")]
    expo: Option<i32>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    let (oracle_pda, bump) =
        Pubkey::find_program_address(&[ORACLE_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:        {}", cli.rpc);
    println!("payer:      {}", payer.pubkey());
    println!("program:    {program_id}");
    println!("market:     {market}");
    println!("oracle PDA: {oracle_pda}  (bump {bump})");
    println!();

    if cli.init && cli.set {
        bail!("--init and --set are mutually exclusive");
    }

    if cli.init {
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(oracle_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: vec![11u8],
        };
        send(&client, &payer, ix)?;
    } else if cli.set {
        let price = cli.price.ok_or_else(|| anyhow::anyhow!("--price required"))?;
        let conf = cli.conf.unwrap_or(0);
        let expo = cli.expo.unwrap_or(0);

        let mut data = Vec::with_capacity(1 + 8 + 8 + 4);
        data.push(12u8);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&conf.to_le_bytes());
        data.extend_from_slice(&expo.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(oracle_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    }

    // Always dump current state.
    println!();
    println!("oracle current state:");
    let account = client.get_account(&oracle_pda).context("fetch oracle account")?;
    let oracle: &Oracle = bytemuck::from_bytes(&account.data[..Oracle::LEN]);
    println!("  market:        {}", Pubkey::new_from_array(oracle.market));
    println!("  price:         {}", oracle.price);
    println!("  conf:          {}", oracle.conf);
    println!("  expo:          {}", oracle.expo);
    println!("  publish_slot:  {}", oracle.publish_slot);
    println!("  bump:          {}", oracle.bump);
    println!("  discriminator: {:?}", &oracle.discriminator);

    let current_slot = client.get_slot().context("fetch current slot")?;
    let age = current_slot.saturating_sub(oracle.publish_slot);
    println!();
    println!("  current_slot:  {}", current_slot);
    println!("  age (slots):   {}", age);

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
