//! Chapter 4 worked example — drives openhl-core's `Bench` instruction
//! and surfaces the compute-unit (CU) numbers the program logs at each
//! phase.
//!
//! What this is for:
//!   - Watching `sol_log_compute_units` output to see real CU costs.
//!   - Demonstrating that the *default* per-transaction CU limit is
//!     ~200,000 CU and that a sufficiently large `--rounds` blows it.
//!   - Showing how `ComputeBudgetInstruction::set_compute_unit_limit`
//!     raises the ceiling (up to ~1.4M CU).
//!   - Showing that `--heap-bytes` above 32 KiB returns null from the
//!     bump allocator, triggering an abort on the program side.
//!
//! Usage:
//!   bench --rpc http://127.0.0.1:8899 \
//!         --payer ~/.config/solana/id.json \
//!         --program <openhl-core program ID> \
//!         --rounds 100 \
//!         --heap-bytes 1024 \
//!         [--cu-limit 400000]

use anyhow::{Context, Result};
use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};

#[derive(Parser, Debug)]
#[command(name = "bench", version, about)]
struct Cli {
    #[arg(long, default_value = "http://127.0.0.1:8899")]
    rpc: String,

    #[arg(long, default_value = "~/.config/solana/id.json")]
    payer: String,

    /// Deployed openhl-core program ID (base58).
    #[arg(long)]
    program: String,

    /// Number of sha256 iterations on the heap buffer.
    #[arg(long, default_value_t = 50)]
    rounds: u32,

    /// Size of the Vec<u8> the program allocates from the bump heap.
    /// 32 KiB (32768) is the default heap limit; values above will OOM.
    #[arg(long, default_value_t = 1024)]
    heap_bytes: u32,

    /// Optional per-tx CU ceiling. If set, a ComputeBudgetInstruction is
    /// prepended that raises the limit from the default 200,000.
    #[arg(long)]
    cu_limit: Option<u32>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:        {}", cli.rpc);
    println!("payer:      {}", payer.pubkey());
    println!("program:    {program_id}");
    println!("rounds:     {}", cli.rounds);
    println!("heap_bytes: {}", cli.heap_bytes);
    println!("cu_limit:   {}", cli.cu_limit.map(|n| n.to_string()).unwrap_or_else(|| "default (200k)".into()));
    println!();

    // Bench payload: [rounds u32 LE][heap_bytes u32 LE]
    let mut data = Vec::with_capacity(1 + 8);
    data.push(2u8); // tag = Bench
    data.extend_from_slice(&cli.rounds.to_le_bytes());
    data.extend_from_slice(&cli.heap_bytes.to_le_bytes());

    let bench_ix = Instruction {
        program_id,
        accounts: vec![], // bench touches no accounts
        data,
    };

    // Optionally prepend a CU-limit-raising instruction. ComputeBudget
    // instructions are processed by the runtime before any user program
    // runs, regardless of their position in the tx — but conventionally
    // they go first for readability.
    let mut instructions: Vec<Instruction> = Vec::new();
    if let Some(limit) = cli.cu_limit {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
    }
    instructions.push(bench_ix);

    let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    // Simulate first — it runs the same VM, returns the same logs, and
    // costs us nothing if it would fail. Logs are identical between sim
    // and on-chain execution for deterministic instructions like Bench.
    let sim = client
        .simulate_transaction(&tx)
        .context("simulate bench tx")?;

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
    }
    println!();

    // Commit only if the simulation succeeded — no point burning a real
    // tx slot on a guaranteed failure.
    if sim.value.err.is_none() {
        match client.send_and_confirm_transaction(&tx) {
            Ok(sig) => println!("on-chain signature: {sig}"),
            Err(e) => println!("on-chain send failed (sim had passed): {e}"),
        }
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
