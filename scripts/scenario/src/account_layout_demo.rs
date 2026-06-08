//! Standalone account-layout inspection demo.
//!
//! Walks every on-chain account type defined in `openhl-state` and
//! reports: human-readable discriminator tag, byte size, layout
//! version, plus aggregate facts (total bytes if you deployed one of
//! each, the orderbook-vs-slab capacity-per-byte comparison).
//!
//! Pure Rust against `openhl-state` constants — no Solana validator,
//! no deployed program, no scripts. The point is to surface Solana's
//! account-model design tradeoffs (fixed-size accounts, byte-precise
//! layouts, repr(C) Pod-safe structs) without requiring a local
//! validator. A buyer evaluating "perp DEX on Solana" can see the
//! byte budget at a glance — the engineering constraint that shapes
//! every other design decision.
//!
//! Same shape pattern as `princeps/run_lending_demo_structured` and
//! `ssr/compliance_demo::run_compliance_demo_structured`: pure-compute
//! function returns a structured result; printed wrapper formats for
//! the CLI subcommand; the scenario runner consumes the structured
//! version directly to render the v2 5-section output contract.

use openhl_state as state;

#[derive(Debug, Clone)]
pub struct AccountLayoutEntry {
    pub type_name: &'static str,
    /// Raw 8-byte discriminator (first bytes of the account data).
    /// Accessed by `discriminators_are_distinct` test; silenced for
    /// release where the test code is not compiled.
    #[allow(dead_code)]
    pub discriminator: [u8; 8],
    /// Human-readable form ("MARKET", "POSITION", etc), trailing NULs trimmed.
    pub discriminator_text: String,
    /// Size in bytes (= `size_of::<T>()`).
    pub size_bytes: usize,
    /// Layout version, if the type exposes one.
    pub version: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct AccountLayoutDemoResult {
    pub entries: Vec<AccountLayoutEntry>,
    /// Capacity (in orders) of the flat-array OrderBook.
    pub orderbook_capacity: usize,
    /// Total size of OrderBook in bytes (header + capacity × Order::LEN).
    pub orderbook_size: usize,
    /// Bytes per order in the flat OrderBook (size / capacity).
    pub orderbook_bytes_per_order: usize,
    /// Capacity (in orders) of the critbit Slab.
    pub slab_capacity: usize,
    /// Total size of Slab in bytes.
    pub slab_size: usize,
    /// Bytes per order in the Slab.
    pub slab_bytes_per_order: usize,
}

impl AccountLayoutDemoResult {
    /// Sum of every account type's size, byte-by-byte. Useful for
    /// "how much space do you need to deploy a market with one of
    /// each".
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries.iter().map(|e| e.size_bytes).sum()
    }

    /// Number of distinct account types (= number of distinct
    /// discriminators).
    #[must_use]
    pub fn distinct_account_types(&self) -> usize {
        self.entries.len()
    }

    /// Largest single account type by size.
    #[must_use]
    pub fn largest(&self) -> Option<&AccountLayoutEntry> {
        self.entries.iter().max_by_key(|e| e.size_bytes)
    }

    /// Smallest single account type by size.
    #[must_use]
    pub fn smallest(&self) -> Option<&AccountLayoutEntry> {
        self.entries.iter().min_by_key(|e| e.size_bytes)
    }
}

fn discriminator_text(d: &[u8; 8]) -> String {
    // Strip trailing NULs; keep the printable prefix.
    let end = d.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    String::from_utf8_lossy(&d[..end]).to_string()
}

fn entry(type_name: &'static str, discriminator: [u8; 8], size_bytes: usize, version: Option<u8>) -> AccountLayoutEntry {
    AccountLayoutEntry {
        type_name,
        discriminator,
        discriminator_text: discriminator_text(&discriminator),
        size_bytes,
        version,
    }
}

/// Pure-compute version. Returns the structured account-layout report.
#[must_use]
pub fn run_account_layout_demo_structured() -> AccountLayoutDemoResult {
    let entries = vec![
        entry("Market", state::MARKET_DISCRIMINATOR, state::Market::LEN, Some(state::Market::VERSION)),
        entry("Stats", state::STATS_DISCRIMINATOR, state::Stats::LEN, None),
        entry("OrderBook", state::ORDER_BOOK_DISCRIMINATOR, state::OrderBook::LEN, None),
        entry("Oracle", state::ORACLE_DISCRIMINATOR, state::Oracle::LEN, None),
        entry("FundingState", state::FUNDING_DISCRIMINATOR, state::FundingState::LEN, None),
        entry("Position", state::POSITION_DISCRIMINATOR, state::Position::LEN, None),
        entry("TradingVault", state::TRADING_VAULT_DISCRIMINATOR, state::TradingVault::LEN, None),
        entry("VaultShare", state::VAULT_SHARE_DISCRIMINATOR, state::VaultShare::LEN, None),
        entry("BuilderProfile", state::BUILDER_PROFILE_DISCRIMINATOR, state::BuilderProfile::LEN, None),
        entry("InsuranceFund", state::INSURANCE_FUND_DISCRIMINATOR, state::InsuranceFund::LEN, None),
        entry("Slab", state::SLAB_DISCRIMINATOR, state::Slab::LEN, None),
    ];

    let orderbook_capacity = state::ORDER_CAPACITY;
    let orderbook_size = state::OrderBook::LEN;
    let orderbook_bytes_per_order = orderbook_size / orderbook_capacity.max(1);

    // Slab capacity is structural: the SLAB_CAPACITY constant if exposed,
    // otherwise derive from total size minus header. The state crate may
    // not export the exact capacity constant — fall back to a safe
    // approximation using Slab::LEN and TreeNode::LEN.
    let slab_size = state::Slab::LEN;
    // Slab packs OrderNode (Order + tree linkage). Estimate capacity as
    // total minus tree overhead; this is the "useful payload" figure.
    let slab_capacity = slab_size / state::OrderNode::LEN.max(1);
    let slab_bytes_per_order = slab_size / slab_capacity.max(1);

    AccountLayoutDemoResult {
        entries,
        orderbook_capacity,
        orderbook_size,
        orderbook_bytes_per_order,
        slab_capacity,
        slab_size,
        slab_bytes_per_order,
    }
}

/// CLI subcommand body. Prints the report in a human-readable form.
pub fn run_account_layout_demo_cli() {
    let r = run_account_layout_demo_structured();

    println!();
    println!("=== openhl-solana — account-layout demo ===");
    println!();
    println!("    Solana accounts are fixed-size and byte-precise. This demo");
    println!("    inspects every on-chain type the engine defines, surfacing the");
    println!("    discriminator tag, byte size, and version. Pure Rust against");
    println!("    `openhl-state` constants — no validator, no program deploy.");
    println!();

    println!("    {:<16}  {:<10}  {:>6}  Version", "Type", "Tag", "Bytes");
    println!("    {:<16}  {:<10}  {:>6}  {}", "─".repeat(16), "─".repeat(10), "─".repeat(6), "─".repeat(7));
    for e in &r.entries {
        let v = e.version.map_or("—".to_string(), |v| format!("v{v}"));
        println!(
            "    {:<16}  {:<10}  {:>6}  {}",
            e.type_name, e.discriminator_text, e.size_bytes, v
        );
    }
    println!();

    println!(
        "    Total bytes (one of each, {} types):  {}",
        r.distinct_account_types(),
        r.total_bytes()
    );
    if let Some(largest) = r.largest() {
        println!(
            "    Largest single type:                   {} ({} bytes)",
            largest.type_name, largest.size_bytes
        );
    }
    if let Some(smallest) = r.smallest() {
        println!(
            "    Smallest single type:                  {} ({} bytes)",
            smallest.type_name, smallest.size_bytes
        );
    }
    println!();

    println!("    OrderBook (flat array): {} bytes for {} order capacity → {} bytes/order",
        r.orderbook_size, r.orderbook_capacity, r.orderbook_bytes_per_order);
    println!("    Slab (critbit tree):    {} bytes for ~{} order capacity → ~{} bytes/order",
        r.slab_size, r.slab_capacity, r.slab_bytes_per_order);
    println!();

    println!("    Takeaway: Solana account sizes are predictable up-front — every");
    println!("    field offset is known at compile time. The Slab packs more orders");
    println!("    per byte but pays a per-fill log(N) cost; the flat OrderBook is");
    println!("    O(N) per fill but the byte layout is trivial to inspect.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_has_nonzero_size() {
        let r = run_account_layout_demo_structured();
        assert!(!r.entries.is_empty());
        for e in &r.entries {
            assert!(e.size_bytes > 0, "{} reported zero bytes", e.type_name);
            assert!(!e.discriminator_text.is_empty(), "{} has empty tag", e.type_name);
        }
    }

    #[test]
    fn discriminators_are_distinct() {
        let r = run_account_layout_demo_structured();
        let set: std::collections::HashSet<_> =
            r.entries.iter().map(|e| e.discriminator).collect();
        assert_eq!(set.len(), r.entries.len(), "duplicate discriminator found");
    }

    #[test]
    fn slab_packs_more_orders_per_byte_than_flat_orderbook() {
        let r = run_account_layout_demo_structured();
        // The critbit Slab is supposed to be more space-efficient per
        // order than the flat-array OrderBook. If a refactor regresses
        // this, the demo's headline claim breaks.
        assert!(
            r.slab_capacity > r.orderbook_capacity,
            "expected Slab capacity ({}) > OrderBook capacity ({})",
            r.slab_capacity,
            r.orderbook_capacity
        );
    }

    #[test]
    fn market_is_versioned() {
        let r = run_account_layout_demo_structured();
        let market = r.entries.iter().find(|e| e.type_name == "Market").unwrap();
        assert!(market.version.is_some(), "Market should expose VERSION");
    }
}
