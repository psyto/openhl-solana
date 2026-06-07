//! Chapter 15 worked example — drives openhl-core's slab order book.
//!
//! Modes (mutually exclusive):
//!   --create                        → CreateSlab (tag 29)
//!   --place --side bid|ask
//!           --price <u64>
//!           --size  <u64>           → SlabPlaceOrder (tag 30)
//!   --match --taker-side bid|ask
//!           --price <u64>           (limit; BID taker = max price, ASK taker = min)
//!           --size  <u64>
//!           --max-fills <u8>        → SlabMatch (tag 31)
//!   (no mode)                       → dump slab summary
//!                                     (active_count + best bid + best ask)
//!
//! Usage:
//!   slab --rpc http://127.0.0.1:8899 \
//!        --payer ~/.config/solana/id.json \
//!        --program <openhl-core program ID> \
//!        --market <market PDA> \
//!        [--create | --place ... | --match ...]

use anyhow::{bail, Context, Result};
use clap::Parser;
use openhl_core::SLAB_SEED;
use openhl_state::{side, tree_tag, CritbitTree, Slab, SLAB_NONE_INDEX};
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
#[command(name = "slab", version, about)]
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
    create: bool,

    #[arg(long)]
    place: bool,
    #[arg(long, requires = "place")]
    side: Option<String>,

    #[arg(long)]
    r#match: bool,
    #[arg(long, requires = "r#match")]
    taker_side: Option<String>,
    #[arg(long, requires = "r#match")]
    max_fills: Option<u8>,

    /// price for both --place (limit) and --match (taker price cap)
    #[arg(long)]
    price: Option<u64>,

    /// size for both --place (resting size) and --match (taker remaining size)
    #[arg(long)]
    size: Option<u64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let payer = read_keypair_file(&expand_tilde(&cli.payer))
        .map_err(|e| anyhow::anyhow!("read payer: {}", e))?;
    let program_id: Pubkey = cli.program.parse().context("parse --program")?;
    let market: Pubkey = cli.market.parse().context("parse --market")?;

    let (slab_pda, bump) =
        Pubkey::find_program_address(&[SLAB_SEED, market.as_ref()], &program_id);

    let client = RpcClient::new_with_commitment(cli.rpc.clone(), CommitmentConfig::confirmed());

    println!("rpc:        {}", cli.rpc);
    println!("payer/user: {}", payer.pubkey());
    println!("program:    {program_id}");
    println!("market:     {market}");
    println!("slab PDA:   {slab_pda}  (bump {bump})");
    println!();

    let modes_set = [cli.create, cli.place, cli.r#match].iter().filter(|b| **b).count();
    if modes_set > 1 {
        bail!("--create, --place, --match are mutually exclusive");
    }

    if cli.create {
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(slab_pda, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data: vec![29u8],
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
        data.push(30u8);
        data.push(side_byte);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(slab_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    } else if cli.r#match {
        let taker_byte = match cli.taker_side.as_deref() {
            Some("bid") => side::BID,
            Some("ask") => side::ASK,
            other => bail!("--taker-side must be 'bid' or 'ask', got {:?}", other),
        };
        let price = cli.price.ok_or_else(|| anyhow::anyhow!("--price required"))?;
        let size = cli.size.ok_or_else(|| anyhow::anyhow!("--size required"))?;
        let max_fills = cli
            .max_fills
            .ok_or_else(|| anyhow::anyhow!("--max-fills required"))?;
        let mut data = Vec::with_capacity(1 + 1 + 8 + 8 + 1);
        data.push(31u8);
        data.push(taker_byte);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());
        data.push(max_fills);
        let ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(slab_pda, false),
            ],
            data,
        };
        send(&client, &payer, ix)?;
    }

    // Always dump.
    println!();
    println!("current slab state:");
    match client.get_account(&slab_pda) {
        Ok(account) if account.data.len() >= Slab::LEN => {
            let slab: &Slab = bytemuck::from_bytes(&account.data[..Slab::LEN]);
            print_slab(slab);
        }
        Ok(_) => println!("  (slab account exists but is wrong size)"),
        Err(_) => println!("  (slab account does not exist yet)"),
    }

    Ok(())
}

fn print_slab(slab: &Slab) {
    println!("  next_order_id: {}", slab.next_order_id);
    println!("  active_count:  {}", slab.active_count);
    println!("  bump:          {}", slab.bump);
    println!("  market:        {}", Pubkey::new_from_array(slab.market));
    println!("  best bid:      {}", describe_best(&slab.bid_tree, true));
    println!("  best ask:      {}", describe_best(&slab.ask_tree, false));
    let bid_leaves = count_leaves(&slab.bid_tree);
    let ask_leaves = count_leaves(&slab.ask_tree);
    println!(
        "  bid levels:    {} (tree-node pool free head {})",
        bid_leaves, slab.bid_tree.free_head
    );
    println!(
        "  ask levels:    {} (tree-node pool free head {})",
        ask_leaves, slab.ask_tree.free_head
    );
    println!("  order-pool free head: {}", slab.pool_free_head);
}

fn describe_best(tree: &CritbitTree, want_max: bool) -> String {
    if tree.root == SLAB_NONE_INDEX {
        return "(empty)".into();
    }
    let mut cur = tree.root;
    loop {
        let node = &tree.nodes[cur as usize];
        if node.tag == tree_tag::LEAF {
            return format!("price={} orders={}", node.price, node.order_count);
        }
        cur = if want_max {
            node.right_child
        } else {
            node.left_child
        };
    }
}

fn count_leaves(tree: &CritbitTree) -> usize {
    tree.nodes
        .iter()
        .filter(|n| n.tag == tree_tag::LEAF)
        .count()
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
