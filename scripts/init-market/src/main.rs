//! Chapter 2 worked example — drives the `openhl-core` program through
//! its first instruction.
//!
//! Given a `Market` account previously allocated by Chapter 1's
//! `allocate-market` (owner = System, data = 256 zero bytes), this client:
//!
//!   1. Submits a System::Assign instruction to transfer ownership of
//!      the market account to our deployed `openhl-core` program. Assign
//!      requires the market account itself to sign — which is why we
//!      need the market keypair from Chapter 1, not just its pubkey.
//!
//!   2. Submits openhl-core's `Initialize { authority, base_mint,
//!      quote_mint, tick_size, lot_size }` instruction. Once the account
//!      is owned by openhl-core, the program is permitted to write to
//!      its data field.
//!
//! Both instructions ride in a single transaction so the assignment and
//! the initialization are atomic — there is no observable state where
//! openhl-core owns an uninitialized 256-zero-byte market.
//!
//! Usage:
//!   init-market \
//!     --rpc http://127.0.0.1:8899 \
//!     --payer ~/.config/solana/id.json \
//!     --market ./market.json \
//!     --program <openhl-core program ID>

use anyhow::{Context, Result};
use clap::Parser;
use openhl_state::Market;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use solana_system_interface::instruction as system_instruction;

#[derive(Parser, Debug)]
#[command(name = "init-market", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    /// Path to the payer keypair (signs + pays tx fee).
    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    /// Path to the keypair for the Market account allocated in Chapter 1.
    /// Required because System::Assign needs the account itself to sign.
    #[arg(long)]
    market: String,

    /// Deployed openhl-core program ID (base58).
    #[arg(long)]
    program: String,

    /// Authority pubkey for the market (defaults to payer).
    #[arg(long)]
    authority: Option<String>,

    /// Base mint pubkey (defaults to a stand-in zero pubkey for ch.2 demo).
    #[arg(long)]
    base_mint: Option<String>,

    /// Quote mint pubkey (defaults to a stand-in zero pubkey for ch.2 demo).
    #[arg(long)]
    quote_mint: Option<String>,

    /// Minimum price increment (quote units).
    #[arg(long, default_value_t = 100)]
    tick_size: u64,

    /// Minimum size increment (base units).
    #[arg(long, default_value_t = 1)]
    lot_size: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let market = read_keypair_file(&expand_tilde(&cli.market))
        .map_err(|e| anyhow::anyhow!("read market keypair: {}", e))?;

    let program_id: Pubkey = cli.program.parse().context("parse --program pubkey")?;
    let authority: Pubkey = cli
        .authority
        .as_deref()
        .map(|s| s.parse())
        .transpose()
        .context("parse --authority")?
        .unwrap_or_else(|| payer.pubkey());
    let base_mint: Pubkey = parse_pubkey_or_default(cli.base_mint.as_deref())?;
    let quote_mint: Pubkey = parse_pubkey_or_default(cli.quote_mint.as_deref())?;

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:            {}", cli.rpc);
    println!("payer:          {}", payer.pubkey());
    println!("market:         {}", market.pubkey());
    println!("program:        {program_id}");
    println!("authority:      {authority}");
    println!("base_mint:      {base_mint}");
    println!("quote_mint:     {quote_mint}");
    println!("tick_size:      {}", cli.tick_size);
    println!("lot_size:       {}", cli.lot_size);
    println!();

    // (1) System::Assign — transfer ownership to our program. Market
    // account must sign because the runtime requires proof that the
    // current owner of the keypair consents to the ownership change.
    let assign_ix = system_instruction::assign(&market.pubkey(), &program_id);

    // (2) openhl-core::Initialize — see programs/openhl-core/src/lib.rs.
    // Instruction data layout: [tag=0][authority 32][base_mint 32]
    //   [quote_mint 32][tick_size u64 LE][lot_size u64 LE].
    let mut init_data = Vec::with_capacity(1 + 32 + 32 + 32 + 8 + 8);
    init_data.push(0u8); // tag = Initialize
    init_data.extend_from_slice(authority.as_ref());
    init_data.extend_from_slice(base_mint.as_ref());
    init_data.extend_from_slice(quote_mint.as_ref());
    init_data.extend_from_slice(&cli.tick_size.to_le_bytes());
    init_data.extend_from_slice(&cli.lot_size.to_le_bytes());

    let init_ix = Instruction {
        program_id,
        accounts: vec![AccountMeta::new(market.pubkey(), false)],
        data: init_data,
    };

    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(
        &[assign_ix, init_ix],
        Some(&payer.pubkey()),
        &[&payer, &market],
        blockhash,
    );

    let sig = client
        .send_and_confirm_transaction(&tx)
        .context("send assign+initialize transaction")?;
    println!("assign+initialize signature: {sig}");
    println!();

    // Fetch and dump.
    let account = client
        .get_account(&market.pubkey())
        .context("fetch initialized market")?;

    println!("account metadata:");
    println!("  owner:        {}", account.owner);
    println!("  lamports:     {}", account.lamports);
    println!("  executable:   {}", account.executable);
    println!("  data length:  {}", account.data.len());
    println!();
    println!("account data (raw bytes, annotated against openhl_state::Market):");
    println!();
    dump_market_bytes(&account.data);

    // Cross-check by viewing the bytes as a Market.
    let market_view: &Market = bytemuck::from_bytes(&account.data[..Market::LEN]);
    println!();
    println!("decoded Market view:");
    println!("  discriminator: {:?}  (ASCII: {:?})",
        market_view.discriminator,
        std::str::from_utf8(&market_view.discriminator).unwrap_or("<non-utf8>"));
    println!("  version:       {}", market_view.version);
    println!("  bump:          {}", market_view.bump);
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
        None => Ok(Pubkey::default()), // 32 zero bytes — stand-in for ch.2 demo
    }
}

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
