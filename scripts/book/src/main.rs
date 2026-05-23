//! Chapter 7 worked example — drives openhl-core's on-chain order book.
//!
//! Modes (mutually exclusive):
//!   --init                          → CreateOrderBook (tag 7)
//!   --place --side bid|ask
//!           --price <u64>
//!           --size  <u64>           → PlaceOrder (tag 8)
//!   --cancel --order-id <u64>       → CancelOrder (tag 9)
//!   (no mode)                       → fetch + print the current book
//!
//! In all mutating modes the script fetches the book afterward and prints
//! the active orders so the linear-scan behavior is visible.
//!
//! Usage:
//!   book --rpc http://127.0.0.1:8899 \
//!        --payer ~/.config/solana/id.json \
//!        --program <openhl-core program ID> \
//!        --market <market PDA> \
//!        [--init | --place ... | --cancel ...]

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::BOOK_SEED;
use openhl_state::{side, Order, OrderBook};
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
#[command(name = "book", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    #[arg(long)]
    program: String,

    #[arg(long)]
    market: String,

    /// CreateOrderBook (run once per market).
    #[arg(long)]
    init: bool,

    /// PlaceOrder mode.
    #[arg(long)]
    place: bool,
    #[arg(long, requires = "place")]
    side: Option<String>,
    #[arg(long, requires = "place")]
    price: Option<u64>,
    #[arg(long, requires = "place")]
    size: Option<u64>,

    /// CancelOrder mode.
    #[arg(long)]
    cancel: bool,
    #[arg(long, requires = "cancel")]
    order_id: Option<u64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    let (book_pda, bump) =
        Pubkey::find_program_address(&[BOOK_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:        {}", cli.rpc);
    println!("payer/user: {}", payer.pubkey());
    println!("program:    {program_id}");
    println!("market:     {market}");
    println!("book PDA:   {book_pda}  (bump {bump})");
    println!();

    let modes_set =
        [cli.init, cli.place, cli.cancel].iter().filter(|b| **b).count();
    if modes_set > 1 {
        bail!("--init, --place, --cancel are mutually exclusive");
    }

    if cli.init {
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(book_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: vec![7u8],
        };
        send(&client, &payer, ix)?;
    } else if cli.place {
        let side_byte = match cli.side.as_deref() {
            Some("bid") => side::BID,
            Some("ask") => side::ASK,
            other => bail!("--side must be 'bid' or 'ask', got {:?}", other),
        };
        let price = cli.price.ok_or_else(|| anyhow::anyhow!("--price required"))?;
        let size = cli.size.ok_or_else(|| anyhow::anyhow!("--size required"))?;

        let mut data = Vec::with_capacity(1 + 1 + 8 + 8);
        data.push(8u8);
        data.push(side_byte);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(book_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.cancel {
        let order_id =
            cli.order_id.ok_or_else(|| anyhow::anyhow!("--order-id required"))?;

        let mut data = Vec::with_capacity(1 + 8);
        data.push(9u8);
        data.extend_from_slice(&order_id.to_le_bytes());

        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(book_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    }

    // Always fetch + dump current book state.
    println!();
    println!("current book state:");
    let account = client
        .get_account(&book_pda)
        .context("fetch book account")?;
    let book: &OrderBook = bytemuck::from_bytes(&account.data[..OrderBook::LEN]);
    print_book(book);

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

fn print_book(book: &OrderBook) {
    println!("  next_order_id: {}", book.next_order_id);
    println!("  active_count:  {}", book.active_count);
    println!("  bump:          {}", book.bump);
    println!("  market:        {}", Pubkey::new_from_array(book.market));
    println!("  slots:");
    let mut shown = 0u32;
    for (i, slot) in book.slots.iter().enumerate() {
        if slot.size == 0 {
            continue;
        }
        let side_str = if slot.side == side::BID { "BID" } else { "ASK" };
        println!(
            "    [{:02}] {} order_id={} price={} size={} owner={}",
            i,
            side_str,
            slot.order_id,
            slot.price,
            slot.size,
            Pubkey::new_from_array(slot.owner),
        );
        shown += 1;
    }
    if shown == 0 {
        println!("    (empty)");
    }
    let _ = std::mem::size_of::<Order>(); // silence unused-import warning if any
}

fn expand_tilde(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{stripped}");
        }
    }
    path.to_string()
}
