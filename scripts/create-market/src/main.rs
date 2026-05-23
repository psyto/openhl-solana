//! Chapter 3 worked example — derives a market PDA from
//! `[b"market", base_mint, quote_mint]` and calls openhl-core's
//! `CreateMarket` instruction, which allocates the PDA via a CPI to
//! System and initializes the layout in a single transaction.
//!
//! Differences from `init-market` (Chapter 2):
//!   - No ad-hoc market keypair. The address is computed deterministically.
//!   - No separate System::Assign step. The program calls create_account
//!     itself via `invoke_signed`, signing for the PDA with the seeds.
//!   - The transaction has exactly one instruction.
//!   - Re-running with the same (base_mint, quote_mint) fails with a System
//!     program error (address already in use), proving the address is fixed.
//!
//! Usage:
//!   create-market \
//!     --rpc http://127.0.0.1:8899 \
//!     --payer ~/.config/solana/id.json \
//!     --program <openhl-core program ID> \
//!     --base-mint <pubkey> \
//!     --quote-mint <pubkey>

use anyhow::{Context, Result};
use clap::Parser;
use openhl_core::MARKET_SEED;
use openhl_state::Market;
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
#[command(name = "create-market", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    /// Deployed openhl-core program ID (base58).
    #[arg(long)]
    program: String,

    /// Authority pubkey (defaults to payer).
    #[arg(long)]
    authority: Option<String>,

    /// Base mint pubkey. Defaults to zero pubkey for ch.3 demo.
    #[arg(long)]
    base_mint: Option<String>,

    /// Quote mint pubkey. Defaults to zero pubkey for ch.3 demo.
    #[arg(long)]
    quote_mint: Option<String>,

    #[arg(long, default_value_t = 100)]
    tick_size: u64,

    #[arg(long, default_value_t = 1)]
    lot_size: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;

    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let authority: Pubkey = cli
        .authority
        .as_deref()
        .map(|s| s.parse())
        .transpose()
        .context("parse --authority")?
        .unwrap_or_else(|| payer.pubkey());
    let base_mint: Pubkey = parse_pubkey_or_default(cli.base_mint.as_deref())?;
    let quote_mint: Pubkey = parse_pubkey_or_default(cli.quote_mint.as_deref())?;

    // Client-side PDA derivation. The program will do the same derivation
    // on-chain and reject the call if the passed `market` account doesn't
    // match. The two derivations *must* agree, byte for byte — that's what
    // makes the address predictable.
    let (market_pda, bump) = Pubkey::find_program_address(
        &[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref()],
        &program_id,
    );

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:            {}", cli.rpc);
    println!("payer:          {}", payer.pubkey());
    println!("program:        {program_id}");
    println!("base_mint:      {base_mint}");
    println!("quote_mint:     {quote_mint}");
    println!("market PDA:     {market_pda}  (bump {bump})");
    println!("authority:      {authority}");
    println!("tick_size:      {}", cli.tick_size);
    println!("lot_size:       {}", cli.lot_size);
    println!();

    // Instruction data: [tag=1][authority 32][base_mint 32][quote_mint 32]
    //   [tick_size u64 LE][lot_size u64 LE]
    let mut data = Vec::with_capacity(1 + 32 + 32 + 32 + 8 + 8);
    data.push(1u8); // tag = CreateMarket
    data.extend_from_slice(authority.as_ref());
    data.extend_from_slice(base_mint.as_ref());
    data.extend_from_slice(quote_mint.as_ref());
    data.extend_from_slice(&cli.tick_size.to_le_bytes());
    data.extend_from_slice(&cli.lot_size.to_le_bytes());

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(market_pda, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    };

    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send create_market transaction")?;
    println!("create_market signature: {sig}");
    println!();

    let account = client
        .get_account(&market_pda)
        .context("fetch newly created market")?;

    println!("account metadata:");
    println!("  owner:        {}", account.owner);
    println!("  lamports:     {}", account.lamports);
    println!("  executable:   {}", account.executable);
    println!("  data length:  {}", account.data.len());
    println!();
    println!("account data (raw bytes, annotated against openhl_state::Market):");
    println!();
    dump_market_bytes(&account.data);

    let market_view: &Market = bytemuck::from_bytes(&account.data[..Market::LEN]);
    println!();
    println!("decoded Market view:");
    println!(
        "  discriminator: {:?}  (ASCII: {:?})",
        market_view.discriminator,
        std::str::from_utf8(&market_view.discriminator).unwrap_or("<non-utf8>")
    );
    println!("  version:       {}", market_view.version);
    println!("  bump:          {}  (matches client-side derivation: {})", market_view.bump, bump);
    println!("  authority:     {}", Pubkey::new_from_array(market_view.authority));
    println!("  base_mint:     {}", Pubkey::new_from_array(market_view.base_mint));
    println!("  quote_mint:    {}", Pubkey::new_from_array(market_view.quote_mint));
    println!("  tick_size:     {}", market_view.tick_size);
    println!("  lot_size:      {}", market_view.lot_size);

    Ok(())
}

fn parse_pubkey_or_default(s: Option<&str>) -> Result<Pubkey> {
    match s {
        Some(s) => Ok(s.parse().context("parse pubkey")?),
        None => Ok(Pubkey::default()),
    }
}

fn dump_market_bytes(data: &[u8]) {
    let regions: &[(usize, usize, &str)] = &[
        (0, 8, "discriminator      [u8; 8]    expected: MARKET\\0\\0"),
        (8, 1, "version            u8"),
        (9, 1, "bump               u8         expected: PDA bump"),
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
