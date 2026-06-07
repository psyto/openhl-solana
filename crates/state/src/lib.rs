//! Account layouts for openhl-solana.
//!
//! Every public struct here is `repr(C)` + `bytemuck::Pod`. The byte layout
//! is the contract: programs write these bytes, clients read them, and
//! Chapter 1 of the SolDojo internals track inspects them raw via
//! `solana account <pubkey>`.
//!
//! We deliberately store `Pubkey` fields as `[u8; 32]` rather than
//! `solana_program::pubkey::Pubkey`. Two reasons:
//!   1. `Pubkey` does not implement `bytemuck::Pod` upstream (no derive,
//!      no manual impl exposed), so using it here would force `unsafe` or
//!      a wrapper newtype.
//!   2. The pedagogy of Chapter 1 is "a Pubkey is 32 bytes." Storing it as
//!      `[u8; 32]` makes that visible at the type level. Chapter 2 will
//!      introduce conversion helpers when the first program needs them.

#![no_std]

use bytemuck::{Pod, Zeroable};

/// Fixed 8-byte tag identifying a `Market` account.
///
/// Chosen so the discriminator is human-readable in a hex dump:
/// `4d 41 52 4b 45 54 00 00` → "MARKET\0\0".
pub const MARKET_DISCRIMINATOR: [u8; 8] = *b"MARKET\0\0";

/// A perp market.
///
/// Layout (offsets in decimal | hex):
/// ```text
///   0  | 0x00  discriminator     [u8; 8]   — MARKET\0\0
///   8  | 0x08  version           u8        — schema version, currently 1
///   9  | 0x09  bump              u8        — PDA bump (used from ch.3 onward)
///  10  | 0x0a  _pad0             [u8; 6]   — alignment padding to next 8-byte field
///  16  | 0x10  authority         [u8; 32]  — admin Pubkey
///  48  | 0x30  base_mint         [u8; 32]  — base asset SPL mint
///  80  | 0x50  quote_mint        [u8; 32]  — quote asset SPL mint (typically USDC)
/// 112  | 0x70  tick_size         u64       — min price increment (quote units)
/// 120  | 0x78  lot_size          u64       — min size increment (base units)
/// 128  | 0x80  _reserved         [u8; 128] — forward-compat slack
/// 256                                       — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Market {
    pub discriminator: [u8; 8],
    pub version: u8,
    pub bump: u8,
    pub _pad0: [u8; 6],
    pub authority: [u8; 32],
    pub base_mint: [u8; 32],
    pub quote_mint: [u8; 32],
    pub tick_size: u64,
    pub lot_size: u64,
    pub _reserved: [u8; 128],
}

impl Market {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const VERSION: u8 = 1;
}

/// Fixed 8-byte tag identifying a `Stats` account.
pub const STATS_DISCRIMINATOR: [u8; 8] = *b"STATS\0\0\0";

/// Singleton program-wide counter. Introduced in Chapter 5 as the worked
/// counter-example to parallelism: a single PDA that every "global"
/// operation would need to write, which serializes those operations
/// through the Sealevel scheduler.
///
/// Layout (56 bytes):
/// ```text
///   0  | 0x00  discriminator   [u8; 8]   — STATS\0\0\0
///   8  | 0x08  bump            u8        — PDA bump for [b"stats"]
///   9  | 0x09  _pad0           [u8; 7]
///  16  | 0x10  market_count    u64       — total CreateMarket calls observed
///  24  | 0x18  _reserved       [u8; 32]
///  56                                     — total size
/// ```
///
/// `_reserved` is 32 bytes (not 40 or 48) because bytemuck's `Pod` derive
/// supports `[T; N]` for a fixed set of N values — 32 is in the set, 40 is
/// not. The constraint is pedagogically honest: if you derive `Pod`, you
/// pick array sizes the trait already knows about.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Stats {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market_count: u64,
    pub _reserved: [u8; 32],
}

impl Stats {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Side of an order — buy or sell. Encoded as a single byte so the layout
/// stays Pod-friendly. `0 = bid`, `1 = ask`. Any other value is invalid
/// and the program rejects orders carrying it.
pub mod side {
    pub const BID: u8 = 0;
    pub const ASK: u8 = 1;
}

/// One resting order in the book. 64 bytes, Pod, repr(C).
///
/// `size == 0` is the sentinel for "this slot is unused." The program
/// scans linearly for the first slot with size 0 when placing a new
/// order; cancellation zeroes the slot in place. There is no compaction.
///
/// Layout:
/// ```text
///   0  | 0x00  order_id   u64       — assigned by OrderBook.next_order_id
///   8  | 0x08  price      u64       — quote units per base unit
///  16  | 0x10  size       u64       — base units remaining (0 = slot empty)
///  24  | 0x18  owner      [u8; 32]  — user pubkey that placed the order
///  56  | 0x38  side       u8        — 0 = bid, 1 = ask
///  57  | 0x39  _pad       [u8; 7]
///  64                                — total
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Order {
    pub order_id: u64,
    pub price: u64,
    pub size: u64,
    pub owner: [u8; 32],
    pub side: u8,
    pub _pad: [u8; 7],
}

impl Order {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Maximum resting orders the on-chain book can hold at once.
///
/// 32 is a deliberately small number chosen for pedagogy: it is large
/// enough that the linear-scan CU cost is measurable in `place_order`
/// and `cancel_order`, and small enough that the book account fits well
/// under the per-instruction data-load budget. A real perp DEX would use
/// 256–1024 slots and a more complex data structure (slab, critbit) —
/// Chapter 7 discusses the trade-off.
pub const ORDER_CAPACITY: usize = 32;

/// Fixed 8-byte tag identifying an `OrderBook` account.
pub const ORDER_BOOK_DISCRIMINATOR: [u8; 8] = *b"BOOK\0\0\0\0";

/// On-chain order book. One per market, 2112 bytes.
///
/// The book is a flat array of `Order` slots. Bid and ask orders share
/// the same array, distinguished by the `side` byte. This is the simplest
/// possible CLOB layout — production designs use sorted price levels with
/// FIFO queues per level (slab / critbit tree), but the flat array is
/// pedagogically honest about what "linear scan" really costs.
///
/// Layout:
/// ```text
///    0 | 0x000  discriminator   [u8; 8]   — BOOK\0\0\0\0
///    8 | 0x008  bump            u8        — PDA bump for [b"book", market]
///    9 | 0x009  _pad0           [u8; 7]
///   16 | 0x010  market          [u8; 32]  — the market this book belongs to
///   48 | 0x030  next_order_id   u64       — monotonic counter
///   56 | 0x038  active_count    u32       — slots with size != 0
///   60 | 0x03c  _pad1           [u8; 4]
///   64 | 0x040  slots           [Order; 32]
/// 2112                                     — total
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct OrderBook {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub next_order_id: u64,
    pub active_count: u32,
    pub _pad1: [u8; 4],
    pub slots: [Order; ORDER_CAPACITY],
}

impl OrderBook {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Fixed 8-byte tag identifying an `Oracle` account.
pub const ORACLE_DISCRIMINATOR: [u8; 8] = *b"ORACLE\0\0";

/// Per-market price oracle. Mirrors the shape of a Pyth price account
/// closely enough that the techniques Chapter 9 teaches transfer
/// directly — `price` + `conf` + `expo` is exactly Pyth's PriceFeed
/// surface, and `publish_slot` plays the role of Pyth's
/// `publish_time`/`prev_publish_time` for staleness detection.
///
/// Layout (112 bytes):
/// ```text
///   0  | 0x00  discriminator   [u8; 8]   — ORACLE\0\0
///   8  | 0x08  bump            u8        — PDA bump for [b"oracle", market]
///   9  | 0x09  _pad0           [u8; 7]
///  16  | 0x10  market          [u8; 32]  — the market this oracle prices
///  48  | 0x30  price           i64       — mantissa, signed
///  56  | 0x38  conf            u64       — confidence interval, same units
///  64  | 0x40  expo            i32       — base-10 exponent (negative = decimals)
///  68  | 0x44  _pad1           [u8; 4]
///  72  | 0x48  publish_slot    u64       — slot when this price was set
///  80  | 0x50  _reserved       [u8; 32]
/// 112                                     — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Oracle {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub price: i64,
    pub conf: u64,
    pub expo: i32,
    pub _pad1: [u8; 4],
    pub publish_slot: u64,
    pub _reserved: [u8; 32],
}

impl Oracle {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Fixed 8-byte tag identifying a `FundingState` account.
pub const FUNDING_DISCRIMINATOR: [u8; 8] = *b"FUNDING\0";

/// Per-market funding-rate accumulator. The single `cumulative_funding_index`
/// field is the load-bearing invariant: it grows monotonically with time
/// (signed; can decrease if shorts pay longs), and a position's funding
/// settlement is just `(current_index - position_snapshot_index) × size`.
///
/// All fixed-point values use a `1e9` scaling factor:
///   - `current_rate_per_sec`: signed nanos of funding paid per second per
///     unit of base notional. A rate of `+50_000` means 0.000_050 / sec,
///     about 0.43% per day, ~158% annualized.
///   - `cumulative_funding_index`: signed nanos of cumulative funding paid
///     per unit of base notional since this market's funding began.
///
/// Layout (120 bytes):
/// ```text
///   0  | 0x00  discriminator              [u8; 8]   — FUNDING\0
///   8  | 0x08  bump                       u8        — PDA bump
///   9  | 0x09  _pad0                      [u8; 7]
///  16  | 0x10  market                     [u8; 32]
///  48  | 0x30  cumulative_funding_index   i64       — scaled by 1e9
///  56  | 0x38  last_update_ts             i64       — Clock.unix_timestamp
///  64  | 0x40  last_update_slot           u64       — Clock.slot
///  72  | 0x48  current_rate_per_sec       i64       — scaled by 1e9
///  80  | 0x50  window_seconds             u64       — funding window length
///  88  | 0x58  _reserved                  [u8; 32]
/// 120                                                — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct FundingState {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub cumulative_funding_index: i64,
    pub last_update_ts: i64,
    pub last_update_slot: u64,
    pub current_rate_per_sec: i64,
    pub window_seconds: u64,
    pub _reserved: [u8; 32],
}

impl FundingState {
    pub const LEN: usize = core::mem::size_of::<Self>();
    /// Default funding window: 1 hour. Production perps usually use 1h or 8h.
    pub const DEFAULT_WINDOW_SECONDS: u64 = 3600;
}

/// Fixed 8-byte tag identifying a `Position` account.
pub const POSITION_DISCRIMINATOR: [u8; 8] = *b"POSITION";

/// Per-user-per-market position. The convergence type — every other Phase B
/// primitive (oracle, funding, vault, matcher) eventually touches a Position
/// to commit its effects. Chapter 11 introduces it and the three lifecycle
/// instructions (Open / Close / Liquidate).
///
/// Layout (144 bytes):
/// ```text
///    0 | 0x00  discriminator              [u8; 8]   — POSITION
///    8 | 0x08  bump                       u8        — PDA bump
///    9 | 0x09  _pad0                      [u8; 7]
///   16 | 0x10  user                       [u8; 32]
///   48 | 0x30  market                     [u8; 32]
///   80 | 0x50  size                       i64       — base units; signed (long > 0, short < 0)
///   88 | 0x58  entry_price                u64       — quote per base at open
///   96 | 0x60  collateral                 u64       — quote units posted as margin
///  104 | 0x68  funding_snapshot_index     i64       — FundingState.cumulative at last touch
///  112 | 0x70  _reserved                  [u8; 32]
///  144                                                — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Position {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub user: [u8; 32],
    pub market: [u8; 32],
    pub size: i64,
    pub entry_price: u64,
    pub collateral: u64,
    pub funding_snapshot_index: i64,
    pub _reserved: [u8; 32],
}

impl Position {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Fixed 8-byte tag identifying a `TradingVault` account.
pub const TRADING_VAULT_DISCRIMINATOR: [u8; 8] = *b"TVAULT\0\0";

/// Pooled trading vault — depositors share PnL via shares/assets ratio.
/// One per (market, manager) pair. The math is ERC-4626 in spirit:
/// `shares_minted = deposit * total_shares / total_assets` on subsequent
/// deposits, 1:1 on the first.
///
/// Layout (160 bytes):
/// ```text
///    0 | 0x00  discriminator   [u8; 8]   — TVAULT\0\0
///    8 | 0x08  bump            u8        — PDA bump
///    9 | 0x09  _pad0           [u8; 7]
///   16 | 0x10  market          [u8; 32]
///   48 | 0x30  manager         [u8; 32]  — places trades on behalf of pool
///   80 | 0x50  mint            [u8; 32]  — asset mint (e.g., USDC quote)
///  112 | 0x70  total_shares    u64
///  120 | 0x78  total_assets    u64       — NAV (set by UpdateNAV)
///  128 | 0x80  _reserved       [u8; 32]
///  160                                    — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct TradingVault {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub manager: [u8; 32],
    pub mint: [u8; 32],
    pub total_shares: u64,
    pub total_assets: u64,
    pub _reserved: [u8; 32],
}

impl TradingVault {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Fixed 8-byte tag identifying a `VaultShare` account.
pub const VAULT_SHARE_DISCRIMINATOR: [u8; 8] = *b"VSHARE\0\0";

/// Per-depositor share ledger. One PDA per (vault, owner) pair. Holds the
/// depositor's share count and their cumulative cost basis (for P&L
/// reporting; not used by program logic).
///
/// Layout (128 bytes):
/// ```text
///    0 | 0x00  discriminator   [u8; 8]   — VSHARE\0\0
///    8 | 0x08  bump            u8
///    9 | 0x09  _pad0           [u8; 7]
///   16 | 0x10  vault           [u8; 32]  — the TradingVault PDA
///   48 | 0x30  owner           [u8; 32]
///   80 | 0x50  shares          u64
///   88 | 0x58  cost_basis      u64       — total assets deposited
///   96 | 0x60  _reserved       [u8; 32]
///  128                                    — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct VaultShare {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub vault: [u8; 32],
    pub owner: [u8; 32],
    pub shares: u64,
    pub cost_basis: u64,
    pub _reserved: [u8; 32],
}

impl VaultShare {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Fixed 8-byte tag identifying a `BuilderProfile` account.
pub const BUILDER_PROFILE_DISCRIMINATOR: [u8; 8] = *b"BUILDER\0";

/// Per-builder fee-accrual profile. A "builder" is a frontend or
/// aggregator that routes user orders to this program; in exchange the
/// program accrues a configurable slice of protocol fees to them.
///
/// Layout (104 bytes):
/// ```text
///    0 | 0x00  discriminator         [u8; 8]   — BUILDER\0
///    8 | 0x08  bump                  u8        — PDA bump
///    9 | 0x09  _pad0                 [u8; 7]
///   16 | 0x10  builder               [u8; 32]  — builder's pubkey
///   48 | 0x30  max_fee_share_bps     u64       — self-cap on share-of-fee
///   56 | 0x38  accumulated_fees      u64       — fees earned, awaiting claim
///   64 | 0x40  total_volume          u64       — base volume routed through this builder
///   72 | 0x48  _reserved             [u8; 32]
///  104                                          — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BuilderProfile {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub builder: [u8; 32],
    pub max_fee_share_bps: u64,
    pub accumulated_fees: u64,
    pub total_volume: u64,
    pub _reserved: [u8; 32],
}

impl BuilderProfile {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

/// Fixed 8-byte tag identifying an `InsuranceFund` account.
pub const INSURANCE_FUND_DISCRIMINATOR: [u8; 8] = *b"INSFUND\0";

/// Per-market insurance fund. Holds the bookkeeping side of a separate
/// SPL Token account at `[INSURANCE_FUND_TOKEN_SEED, market, mint]` that
/// receives the protocol's slice of liquidation penalties and drains to
/// cover underwater-close shortfalls. The token-account authority is the
/// shared per-market vault_authority PDA (the same one signing position
/// vault transfers in Chapter 11), so no new authority PDA is needed.
///
/// Layout (136 bytes):
/// ```text
///    0 | 0x00  discriminator         [u8; 8]   — INSFUND\0
///    8 | 0x08  bump                  u8        — PDA bump
///    9 | 0x09  _pad0                 [u8; 7]
///   16 | 0x10  market                [u8; 32]  — owning market
///   48 | 0x30  mint                  [u8; 32]  — quote mint
///   80 | 0x50  balance               u64       — mirrors the fund token balance
///   88 | 0x58  total_deposits        u64       — observability: total ever credited
///   96 | 0x60  total_drawdowns       u64       — observability: total ever drained
///  104 | 0x68  _reserved             [u8; 32]
///  136                                          — total size
/// ```
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct InsuranceFund {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub mint: [u8; 32],
    pub balance: u64,
    pub total_deposits: u64,
    pub total_drawdowns: u64,
    pub _reserved: [u8; 32],
}

impl InsuranceFund {
    pub const LEN: usize = core::mem::size_of::<Self>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_size_is_256_bytes() {
        assert_eq!(Market::LEN, 256);
    }

    #[test]
    fn market_alignment_is_8() {
        assert_eq!(core::mem::align_of::<Market>(), 8);
    }

    #[test]
    fn zeroed_market_has_zero_discriminator() {
        let m = Market::zeroed();
        let bytes = bytemuck::bytes_of(&m);
        assert_eq!(&bytes[..8], &[0u8; 8]);
        assert_eq!(bytes.len(), 256);
    }

    #[test]
    fn discriminator_is_human_readable() {
        assert_eq!(&MARKET_DISCRIMINATOR, b"MARKET\0\0");
    }

    #[test]
    fn stats_size_is_56_bytes() {
        assert_eq!(Stats::LEN, 56);
    }

    #[test]
    fn stats_discriminator_is_human_readable() {
        assert_eq!(&STATS_DISCRIMINATOR, b"STATS\0\0\0");
    }

    #[test]
    fn order_size_is_64_bytes() {
        assert_eq!(Order::LEN, 64);
    }

    #[test]
    fn order_book_size_matches_layout() {
        // 64 byte header + 32 * 64 byte slots = 2112
        assert_eq!(OrderBook::LEN, 64 + ORDER_CAPACITY * Order::LEN);
        assert_eq!(OrderBook::LEN, 2112);
    }

    #[test]
    fn order_book_discriminator_is_human_readable() {
        assert_eq!(&ORDER_BOOK_DISCRIMINATOR, b"BOOK\0\0\0\0");
    }

    #[test]
    fn oracle_size_is_112_bytes() {
        assert_eq!(Oracle::LEN, 112);
    }

    #[test]
    fn oracle_discriminator_is_human_readable() {
        assert_eq!(&ORACLE_DISCRIMINATOR, b"ORACLE\0\0");
    }

    #[test]
    fn funding_state_size_is_120_bytes() {
        assert_eq!(FundingState::LEN, 120);
    }

    #[test]
    fn funding_discriminator_is_human_readable() {
        assert_eq!(&FUNDING_DISCRIMINATOR, b"FUNDING\0");
    }

    #[test]
    fn position_size_is_144_bytes() {
        assert_eq!(Position::LEN, 144);
    }

    #[test]
    fn position_discriminator_is_human_readable() {
        assert_eq!(&POSITION_DISCRIMINATOR, b"POSITION");
    }

    #[test]
    fn trading_vault_size_is_160_bytes() {
        assert_eq!(TradingVault::LEN, 160);
    }

    #[test]
    fn trading_vault_discriminator_is_human_readable() {
        assert_eq!(&TRADING_VAULT_DISCRIMINATOR, b"TVAULT\0\0");
    }

    #[test]
    fn vault_share_size_is_128_bytes() {
        assert_eq!(VaultShare::LEN, 128);
    }

    #[test]
    fn vault_share_discriminator_is_human_readable() {
        assert_eq!(&VAULT_SHARE_DISCRIMINATOR, b"VSHARE\0\0");
    }

    #[test]
    fn builder_profile_size_is_104_bytes() {
        assert_eq!(BuilderProfile::LEN, 104);
    }

    #[test]
    fn builder_profile_discriminator_is_human_readable() {
        assert_eq!(&BUILDER_PROFILE_DISCRIMINATOR, b"BUILDER\0");
    }

    #[test]
    fn insurance_fund_size_is_136_bytes() {
        assert_eq!(InsuranceFund::LEN, 136);
    }

    #[test]
    fn insurance_fund_discriminator_is_human_readable() {
        assert_eq!(&INSURANCE_FUND_DISCRIMINATOR, b"INSFUND\0");
    }
}
