//! Chapter 8 worked example — drives openhl-core::Match.
//!
//! The matcher's CU cost grows roughly as O(fills × active_count).
//! This script:
//!   - Always simulates first so a CU-budget failure doesn't burn a
//!     transaction slot.
//!   - Surfaces `units_consumed` alongside the raw program logs so the
//!     fill costs are readable.
//!   - Optionally prepends a `ComputeBudgetInstruction::set_compute_unit_limit`
//!     to raise the per-tx ceiling above the default 200K.
//!
//! Usage:
//!   match-cli --rpc http://127.0.0.1:8899 \
//!             --taker ~/.config/solana/id.json \
//!             --program <openhl-core program ID> \
//!             --market <market PDA> \
//!             --side bid|ask \
//!             --limit-price <u64> \
//!             --size <u64> \
//!             --max-fills <u8> \
//!             [--cu-limit <u32>]

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::BOOK_SEED;
use openhl_state::{side, OrderBook};
use solana_client::rpc_client::RpcClient;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};

#[derive(Parser, Debug)]
#[command(name = "match-cli", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    taker: String,

    #[arg(long)]
    program: String,

    #[arg(long)]
    market: String,

    #[arg(long)]
    side: String,

    #[arg(long)]
    limit_price: u64,

    #[arg(long)]
    size: u64,

    #[arg(long)]
    max_fills: u8,

    #[arg(long)]
    cu_limit: Option<u32>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let taker = read_keypair_file(&expand_tilde(&cli.taker))
        .map_err(|e| anyhow::anyhow!("read taker: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    let side_byte = match cli.side.as_str() {
        "bid" => side::BID,
        "ask" => side::ASK,
        other => bail!("--side must be 'bid' or 'ask', got {:?}", other),
    };

    let (book_pda, _bump) =
        Pubkey::find_program_address(&[BOOK_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:         {}", cli.rpc);
    println!("taker:       {}", taker.pubkey());
    println!("program:     {program_id}");
    println!("market:      {market}");
    println!("book PDA:    {book_pda}");
    println!("side:        {} ({})", cli.side, side_byte);
    println!("limit_price: {}", cli.limit_price);
    println!("size:        {}", cli.size);
    println!("max_fills:   {}", cli.max_fills);
    println!("cu_limit:    {}", cli.cu_limit.map(|n| n.to_string()).unwrap_or_else(|| "default (200k)".into()));
    println!();

    let mut data = Vec::with_capacity(1 + 1 + 8 + 8 + 1);
    data.push(10u8);
    data.push(side_byte);
    data.extend_from_slice(&cli.limit_price.to_le_bytes());
    data.extend_from_slice(&cli.size.to_le_bytes());
    data.push(cli.max_fills);

    let match_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(taker.pubkey(), true),
            AccountMeta::new(book_pda, false),
        ],
        data,
    };

    let mut instructions: Vec<Instruction> = Vec::new();
    if let Some(limit) = cli.cu_limit {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
    }
    instructions.push(match_ix);

    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(&instructions, Some(&taker.pubkey()), &[&taker], blockhash);

    // Simulate first.
    let sim = client.simulate_transaction(&tx).context("simulate match tx")?;
    println!("simulation:");
    println!("  units_consumed: {}", sim.value.units_consumed.unwrap_or(0));
    if let Some(err) = &sim.value.err {
        println!("  err:            {err:?}");
    } else {
        println!("  err:            (none)");
    }
    println!();

    if let Some(logs) = sim.value.logs {
        println!("program logs:");
        for line in logs {
            println!("  {line}");
        }
        println!();
    }

    if sim.value.err.is_none() {
        match client.send_and_confirm_transaction(&tx) {
            Ok(sig) => println!("on-chain signature: {sig}"),
            Err(e) => println!("on-chain send failed (sim had passed): {e}"),
        }

        // Dump post-state.
        let account = client.get_account(&book_pda).context("fetch book post-state")?;
        let book: &OrderBook = bytemuck::from_bytes(&account.data[..OrderBook::LEN]);
        println!();
        println!("book post-state: active_count={}, next_order_id={}", book.active_count, book.next_order_id);
    } else {
        println!("(skipping on-chain send because simulation failed)");
    }

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
