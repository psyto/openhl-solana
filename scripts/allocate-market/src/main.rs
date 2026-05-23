//! Chapter 1 worked example — "The Account Model from the Bytes Up".
//!
//! Allocates a `Market`-sized account on a Solana cluster via the
//! System program, then fetches and hex-dumps its bytes with offsets
//! annotated against the `openhl_state::Market` layout.
//!
//! The account is **owned by the System program** after creation. We do
//! not write any data — the data field is all zeros. The point of this
//! chapter is the account model itself (owner, lamports, data, rent_epoch);
//! actually *writing* to the data requires our own program, which is
//! Chapter 2's worked example.
//!
//! Usage:
//!   allocate-market --rpc http://127.0.0.1:8899 --payer ~/.config/solana/id.json

use anyhow::{Context, Result};
use clap::Parser;
use openhl_state::Market;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::instruction as system_instruction;

#[derive(Parser, Debug)]
#[command(name = "allocate-market", version, about)]
struct Cli {
    /// RPC endpoint to connect to.
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    /// Path to the payer keypair (will fund rent + tx fee).
    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer_path = expand_tilde(&cli.payer);
    let payer = read_keypair_file(&payer_path)
        .map_err(|e| anyhow::anyhow!("read payer keypair {}: {}", payer_path, e))?;

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    // The account is identified by a fresh ad-hoc keypair. Chapter 3 will
    // replace this with a PDA derived from `find_program_address`, but
    // the account model itself is independent of how the Pubkey is chosen.
    let market = Keypair::new();
    let market_pubkey = market.pubkey();

    let space = Market::LEN as u64;
    let rent_lamports = client
        .get_minimum_balance_for_rent_exemption(Market::LEN)
        .context("fetch rent-exempt minimum")?;

    println!("rpc:            {}", cli.rpc);
    println!("payer:          {}", payer.pubkey());
    println!("market pubkey:  {market_pubkey}");
    println!("space:          {space} bytes");
    println!("rent lamports:  {rent_lamports}  ({:.6} SOL)", rent_lamports as f64 / 1e9);
    println!();

    // The owner stays as the System program. We're not assigning to a
    // custom program because we don't have one yet — that's Chapter 2.
    let owner: Pubkey = solana_sdk::system_program::ID;

    let create_ix = system_instruction::create_account(
        &payer.pubkey(),
        &market_pubkey,
        rent_lamports,
        space,
        &owner,
    );

    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&payer.pubkey()),
        &[&payer, &market],
        blockhash,
    );

    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send create_account transaction")?;
    println!("create_account signature: {sig}");
    println!();

    // Fetch the account and dump.
    let account = client
        .get_account(&market_pubkey)
        .context("fetch newly created account")?;

    println!("account metadata:");
    println!("  owner:        {}", account.owner);
    println!("  lamports:     {}", account.lamports);
    println!("  executable:   {}", account.executable);
    println!("  rent_epoch:   {}", account.rent_epoch);
    println!("  data length:  {}", account.data.len());
    println!();
    println!("account data (raw bytes, annotated against openhl_state::Market):");
    println!();
    dump_market_bytes(&account.data);

    Ok(())
}

/// Hex-dump 256 bytes with offsets and Market field annotations.
fn dump_market_bytes(data: &[u8]) {
    let regions: &[(usize, usize, &str)] = &[
        (0, 8, "discriminator      [u8; 8]    expected: MARKET\\0\\0"),
        (8, 1, "version            u8"),
        (9, 1, "bump               u8"),
        (10, 6, "_pad0              [u8; 6]"),
        (16, 32, "authority          [u8; 32]"),
        (48, 32, "base_mint          [u8; 32]"),
        (80, 32, "quote_mint         [u8; 32]"),
        (112, 8, "tick_size          u64"),
        (120, 8, "lot_size           u64"),
        (128, 128, "_reserved          [u8; 128]"),
    ];

    for &(off, len, label) in regions {
        let end = (off + len).min(data.len());
        let bytes = &data[off..end];
        println!("  0x{off:04x}  {label}");
        for chunk_start in (0..bytes.len()).step_by(16) {
            let chunk_end = (chunk_start + 16).min(bytes.len());
            let chunk = &bytes[chunk_start..chunk_end];
            let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
            println!("          {hex}");
        }
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{stripped}");
        }
    }
    path.to_string()
}
