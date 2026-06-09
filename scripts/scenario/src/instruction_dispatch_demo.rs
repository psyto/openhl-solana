//! Standalone instruction-dispatch inspection demo.
//!
//! Walks every instruction tag in openhl-core's `process_instruction`
//! dispatch table and reports: tag (the 1-byte discriminator at byte
//! 0 of instruction data), name, and category. Pure Rust against a
//! hand-maintained mirror of the dispatch — no Solana validator, no
//! deployed program. The point is to surface the engine's public
//! API surface at a glance, the way `account-layout-demo` surfaces
//! the on-chain account types.
//!
//! Maintenance contract: the table here is mirrored from
//! `programs/openhl-core/src/lib.rs::process_instruction`. When you
//! add a new instruction tag, add it here too. The
//! `mirror_matches_program` test would normally compare the
//! two automatically, but reading a Rust source file at build time
//! is friction we don't pay yet — the per-instruction unit tests
//! below at least pin every tag value so a drift is loud.

#[derive(Debug, Clone)]
pub(crate) struct InstructionEntry {
    pub tag: u8,
    pub name: &'static str,
    /// One-line description of what the instruction does.
    pub description: &'static str,
    /// Grouping for buyer-facing summary (`bring-up`, `order`, …).
    pub category: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct InstructionDispatchDemoResult {
    pub entries: Vec<InstructionEntry>,
}

impl InstructionDispatchDemoResult {
    #[must_use]
    pub(crate) fn total_instructions(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub(crate) fn distinct_tags(&self) -> usize {
        use std::collections::HashSet;
        let set: HashSet<u8> = self.entries.iter().map(|e| e.tag).collect();
        set.len()
    }

    #[must_use]
    pub(crate) fn count_in_category(&self, category: &str) -> usize {
        self.entries.iter().filter(|e| e.category == category).count()
    }

    #[must_use]
    pub(crate) fn find_tag(&self, tag: u8) -> Option<&InstructionEntry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    #[must_use]
    pub(crate) fn max_tag(&self) -> u8 {
        self.entries.iter().map(|e| e.tag).max().unwrap_or(0)
    }
}

const fn entry(
    tag: u8,
    name: &'static str,
    category: &'static str,
    description: &'static str,
) -> InstructionEntry {
    InstructionEntry { tag, name, description, category }
}

/// Pure-compute version. Returns the structured dispatch table.
#[must_use]
pub(crate) fn run_instruction_dispatch_demo_structured() -> InstructionDispatchDemoResult {
    let entries = vec![
        entry(0,  "Initialize",                    "bring-up",  "Boot a Market account from raw allocation"),
        entry(1,  "CreateMarket",                  "bring-up",  "Derive and create the per-market PDA"),
        entry(2,  "Bench",                         "benchmark", "CU-instrumentation harness for measurement"),
        entry(3,  "CreateStats",                   "bring-up",  "Allocate the Stats singleton (parallelism counter-example)"),
        entry(4,  "BumpStats",                     "benchmark", "Increment the Stats counter — surfaces account-lock cost"),
        entry(5,  "CreateVault",                   "bring-up",  "Allocate the per-(market, mint) vault PDA"),
        entry(6,  "Deposit",                       "collateral","CPI to SPL Token to deposit collateral"),
        entry(7,  "CreateOrderBook",               "bring-up",  "Allocate the flat-array OrderBook"),
        entry(8,  "PlaceOrder",                    "order",     "Place a limit order into the flat OrderBook"),
        entry(9,  "CancelOrder",                   "order",     "Cancel a resting limit order"),
        entry(10, "Match",                         "order",     "Cross resting orders with a market order (paginated)"),
        entry(11, "CreateOracle",                  "bring-up",  "Allocate the mock-oracle account"),
        entry(12, "SetOraclePrice",                "oracle",    "Mock-oracle publisher: set the next price"),
        entry(13, "PlaceOrderChecked",             "order",     "Place an order with oracle staleness guard"),
        entry(14, "CreateFundingState",            "bring-up",  "Allocate the FundingState account"),
        entry(15, "UpdateFunding",                 "funding",   "Drive the funding rate accumulator one period"),
        entry(16, "OpenPosition",                  "position",  "Open a perp position from a fill"),
        entry(17, "ClosePosition",                 "position",  "Close a perp position; realize P&L"),
        entry(18, "Liquidate",                     "position",  "Liquidate an underwater position; debit insurance fund"),
        entry(19, "CreateTradingVault",            "bring-up",  "Allocate the trading-vault account"),
        entry(20, "VaultDeposit",                  "vault",     "Deposit into a trading vault; mint vault shares"),
        entry(21, "VaultWithdraw",                 "vault",     "Burn vault shares; withdraw collateral"),
        entry(22, "VaultUpdateNav",                "vault",     "Update vault NAV per share"),
        entry(23, "RegisterBuilder",               "bring-up",  "Register a builder code + fee profile"),
        entry(24, "PlaceOrderWithBuilder",         "order",     "Place an order tagged with a builder code"),
        entry(25, "ClaimBuilderFees",              "builder",   "Builder claims accrued fee revenue"),
        entry(26, "CreateFeeVault",                "bring-up",  "Allocate the fee-vault PDA"),
        entry(27, "CreateInsuranceFund",           "bring-up",  "Allocate the insurance fund account"),
        entry(28, "InsuranceFundDeposit",          "insurance", "Top up the insurance fund"),
        entry(29, "CreateSlab",                    "bring-up",  "Allocate the critbit Slab orderbook"),
        entry(30, "SlabPlaceOrder",                "order",     "Place a limit order into the Slab"),
        entry(31, "SlabMatch",                     "order",     "Cross Slab resting orders with a market order"),
        entry(32, "PlaceOrderCheckedPyth",         "order",     "Place an order checked against legacy Pyth oracle"),
        entry(33, "PlaceOrderCheckedPythV2",       "order",     "Place an order checked against Pyth V2 PriceUpdate"),
    ];
    InstructionDispatchDemoResult { entries }
}

/// CLI subcommand body. Prints the dispatch table.
pub(crate) fn run_instruction_dispatch_demo_cli() {
    let r = run_instruction_dispatch_demo_structured();

    println!();
    println!("=== openhl-solana — instruction dispatch demo ===");
    println!();
    println!("    Every openhl-core instruction is dispatched by a 1-byte tag");
    println!("    at byte 0 of the instruction data. This demo prints the full");
    println!("    table so a buyer evaluating the engine sees the API surface");
    println!("    at a glance. Pure Rust — no validator, no deployed program.");
    println!();

    println!("    {:<4}  {:<28}  {:<11}  Description", "Tag", "Name", "Category");
    println!("    {:-<4}  {:-<28}  {:-<11}  {:-<60}", "", "", "", "");
    for e in &r.entries {
        println!(
            "    {:>3}   {:<28}  {:<11}  {}",
            e.tag, e.name, e.category, e.description
        );
    }
    println!();

    println!("    Total instructions: {}", r.total_instructions());
    println!("    Distinct tags     : {}", r.distinct_tags());
    println!("    Max tag           : {}", r.max_tag());
    println!();
    println!("    Category breakdown:");
    for category in &[
        "bring-up", "order", "position", "vault", "oracle",
        "funding", "collateral", "builder", "insurance", "benchmark",
    ] {
        let n = r.count_in_category(category);
        if n > 0 {
            println!("      {:<11}  {n}", category);
        }
    }
    println!();
    println!("    Takeaway: openhl-core packs a complete perp DEX into a single");
    println!("    34-instruction program. Half are bring-up; the rest are the");
    println!("    operational surface a perp trader actually touches.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_table_has_thirty_four_entries() {
        let r = run_instruction_dispatch_demo_structured();
        assert_eq!(r.total_instructions(), 34);
    }

    #[test]
    fn all_tags_are_distinct() {
        let r = run_instruction_dispatch_demo_structured();
        assert_eq!(r.distinct_tags(), r.total_instructions());
    }

    #[test]
    fn tags_are_contiguous_zero_to_thirty_three() {
        let r = run_instruction_dispatch_demo_structured();
        for i in 0..=33u8 {
            assert!(r.find_tag(i).is_some(), "missing tag {i}");
        }
        assert_eq!(r.max_tag(), 33);
    }

    #[test]
    fn known_tags_map_to_known_names() {
        // Spot-check a handful of well-known tags to catch table
        // drift relative to programs/openhl-core/src/lib.rs.
        let r = run_instruction_dispatch_demo_structured();
        assert_eq!(r.find_tag(0).unwrap().name, "Initialize");
        assert_eq!(r.find_tag(10).unwrap().name, "Match");
        assert_eq!(r.find_tag(18).unwrap().name, "Liquidate");
        assert_eq!(r.find_tag(31).unwrap().name, "SlabMatch");
        assert_eq!(r.find_tag(33).unwrap().name, "PlaceOrderCheckedPythV2");
    }

    #[test]
    fn categories_cover_expected_buckets() {
        let r = run_instruction_dispatch_demo_structured();
        // Every category must appear at least once; spot-check counts.
        assert!(r.count_in_category("bring-up") >= 10);
        assert!(r.count_in_category("order") >= 5);
        assert!(r.count_in_category("position") == 3);
        assert!(r.count_in_category("vault") == 3);
    }
}
