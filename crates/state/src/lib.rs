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
}
