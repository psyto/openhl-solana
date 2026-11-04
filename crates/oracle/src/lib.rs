//! Oracle checks for a Solana program, from raw account bytes.
//!
//! - [`check_fresh`]: refuse a price published more than `max_age` slots ago.
//! - [`sanity_band`]: the `[low, high]` range an order price must sit in,
//!   `band_bps` either side of the mark.
//! - [`pyth_v1::parse`]: read the aggregate from a Pyth v1 `PriceAccount`.
//! - [`pyth_v2::parse`]: read a Pyth `PriceUpdateV2` (the receiver program's
//!   output), for either verification level.
//!
//! Nothing here reads the clock or an `AccountInfo`: the caller passes the
//! current slot and the account data, so the same code runs on-chain, in a
//! client, and in tests. `no_std`. Byte offsets come from `openhl-state`.

#![no_std]

/// `10_000` basis points = 100%.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// A price older than the allowed age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stale {
    /// `now_slot - publish_slot` (0 if the publish slot is in the future).
    pub age: u64,
    pub max_age: u64,
}

/// Refuse a price published more than `max_age` slots before `now_slot`.
#[inline]
pub fn check_fresh(publish_slot: u64, now_slot: u64, max_age: u64) -> Result<(), Stale> {
    let age = now_slot.saturating_sub(publish_slot);
    if age > max_age {
        Err(Stale { age, max_age })
    } else {
        Ok(())
    }
}

/// `[mark - band, mark + band]` with `band = mark * band_bps / 10_000`,
/// saturating at the `u64` bounds.
#[inline]
pub fn sanity_band(mark: u64, band_bps: u64) -> (u64, u64) {
    let band = mark.saturating_mul(band_bps) / BPS_DENOMINATOR;
    (mark.saturating_sub(band), mark.saturating_add(band))
}

/// `true` when `price` lies inside [`sanity_band`] (inclusive).
#[inline]
pub fn within_band(price: u64, mark: u64, band_bps: u64) -> bool {
    let (low, high) = sanity_band(mark, band_bps);
    price >= low && price <= high
}

#[inline]
fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

#[inline]
fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(b)
}

/// Pyth v1 `PriceAccount` (the push-oracle layout).
pub mod pyth_v1 {
    use super::{u32_at, u64_at};
    use openhl_state::{
        PYTH_V1_ACCOUNT_TYPE_PRICE, PYTH_V1_MAGIC, PYTH_V1_MIN_LEN, PYTH_V1_OFFSET_AGG_CONF,
        PYTH_V1_OFFSET_AGG_PRICE, PYTH_V1_OFFSET_AGG_PUB_SLOT, PYTH_V1_OFFSET_AGG_STATUS,
        PYTH_V1_OFFSET_EXPO, PYTH_V1_STATUS_TRADING, PYTH_V1_VERSION,
    };

    /// The aggregate price of a `PriceAccount` whose status is Trading.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Aggregate {
        /// Mantissa; the value is `price * 10^expo`. May be <= 0: Pyth
        /// sometimes publishes 0 to mean "no good price".
        pub price: i64,
        pub conf: u64,
        pub expo: i32,
        /// Slot the aggregate was published in; feed it to
        /// [`check_fresh`](crate::check_fresh).
        pub pub_slot: u64,
    }

    /// Why the bytes are not a usable `PriceAccount`, checked in this order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        TooSmall { len: usize, need: usize },
        BadMagic(u32),
        UnsupportedVersion(u32),
        NotPriceAccount { atype: u32 },
        NotTrading { status: u32 },
    }

    /// Check magic, version, account type and aggregate status, then read
    /// the aggregate. Staleness and the sign of the price are the caller's.
    pub fn parse(data: &[u8]) -> Result<Aggregate, Error> {
        if data.len() < PYTH_V1_MIN_LEN {
            return Err(Error::TooSmall {
                len: data.len(),
                need: PYTH_V1_MIN_LEN,
            });
        }
        let magic = u32_at(data, 0);
        if magic != PYTH_V1_MAGIC {
            return Err(Error::BadMagic(magic));
        }
        let ver = u32_at(data, 4);
        if ver != PYTH_V1_VERSION {
            return Err(Error::UnsupportedVersion(ver));
        }
        let atype = u32_at(data, 8);
        if atype != PYTH_V1_ACCOUNT_TYPE_PRICE {
            return Err(Error::NotPriceAccount { atype });
        }
        let status = u32_at(data, PYTH_V1_OFFSET_AGG_STATUS);
        if status != PYTH_V1_STATUS_TRADING {
            return Err(Error::NotTrading { status });
        }
        Ok(Aggregate {
            price: u64_at(data, PYTH_V1_OFFSET_AGG_PRICE) as i64,
            conf: u64_at(data, PYTH_V1_OFFSET_AGG_CONF),
            expo: u32_at(data, PYTH_V1_OFFSET_EXPO) as i32,
            pub_slot: u64_at(data, PYTH_V1_OFFSET_AGG_PUB_SLOT),
        })
    }
}

/// Pyth `PriceUpdateV2`, written by `pyth-solana-receiver` after it has
/// verified the Wormhole VAA. This module does not verify anything itself.
///
/// The account is Borsh: `verification_level` takes 2 bytes when Partial
/// (tag + signature count) and 1 byte when Full, so every later field sits
/// one byte earlier in a Full update. The `PYTH_V2_OFFSET_*` constants in
/// `openhl-state` are the Partial offsets; [`parse`] handles both.
pub mod pyth_v2 {
    use super::{u32_at, u64_at};
    use openhl_state::{
        PYTH_V2_MIN_LEN, PYTH_V2_OFFSET_CONF, PYTH_V2_OFFSET_EXPONENT, PYTH_V2_OFFSET_POSTED_SLOT,
        PYTH_V2_OFFSET_PRICE, PYTH_V2_OFFSET_PUBLISH_TIME, PYTH_V2_OFFSET_VERIFICATION_LEVEL,
        PYTH_V2_OFFSET_VERIFICATION_NUM_SIGS, PYTH_V2_PRICE_UPDATE_DISCRIMINATOR,
        PYTH_V2_VERIFICATION_FULL, PYTH_V2_VERIFICATION_PARTIAL,
    };

    /// How many Wormhole guardian signatures the receiver checked.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VerificationLevel {
        Partial { num_signatures: u8 },
        Full,
    }

    /// A parsed price update.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PriceUpdate {
        pub verification_level: VerificationLevel,
        /// Mantissa; the value is `price * 10^exponent`.
        pub price: i64,
        pub conf: u64,
        pub exponent: i32,
        /// Unix time the publishers signed the price.
        pub publish_time: i64,
        /// Solana slot the receiver wrote the account in; feed it to
        /// [`check_fresh`](crate::check_fresh).
        pub posted_slot: u64,
    }

    /// Why the bytes are not a usable `PriceUpdateV2`, checked in this order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        TooSmall { len: usize, need: usize },
        BadDiscriminator,
        UnknownVerificationLevel(u8),
    }

    /// Check the Anchor discriminator and verification level, then read the
    /// price message and posted slot. Staleness, the sign of the price and
    /// any minimum verification level are the caller's.
    pub fn parse(data: &[u8]) -> Result<PriceUpdate, Error> {
        if data.len() < PYTH_V2_MIN_LEN {
            return Err(Error::TooSmall {
                len: data.len(),
                need: PYTH_V2_MIN_LEN,
            });
        }
        if data[0..8] != PYTH_V2_PRICE_UPDATE_DISCRIMINATOR {
            return Err(Error::BadDiscriminator);
        }
        let tag = data[PYTH_V2_OFFSET_VERIFICATION_LEVEL];
        // `shift` is how many bytes earlier the fields after the level sit
        // than the Partial offsets in openhl-state.
        let (verification_level, shift) = if tag == PYTH_V2_VERIFICATION_PARTIAL {
            let n = data[PYTH_V2_OFFSET_VERIFICATION_NUM_SIGS];
            (VerificationLevel::Partial { num_signatures: n }, 0)
        } else if tag == PYTH_V2_VERIFICATION_FULL {
            (VerificationLevel::Full, 1)
        } else {
            return Err(Error::UnknownVerificationLevel(tag));
        };
        Ok(PriceUpdate {
            verification_level,
            price: u64_at(data, PYTH_V2_OFFSET_PRICE - shift) as i64,
            conf: u64_at(data, PYTH_V2_OFFSET_CONF - shift),
            exponent: u32_at(data, PYTH_V2_OFFSET_EXPONENT - shift) as i32,
            publish_time: u64_at(data, PYTH_V2_OFFSET_PUBLISH_TIME - shift) as i64,
            posted_slot: u64_at(data, PYTH_V2_OFFSET_POSTED_SLOT - shift),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhl_state::*;

    #[test]
    fn freshness() {
        assert_eq!(check_fresh(100, 125, 25), Ok(()));
        assert_eq!(
            check_fresh(100, 126, 25),
            Err(Stale {
                age: 26,
                max_age: 25
            })
        );
        assert_eq!(check_fresh(200, 100, 25), Ok(()));
    }

    #[test]
    fn band() {
        assert_eq!(sanity_band(100_000, 2_000), (80_000, 120_000));
        assert!(within_band(80_000, 100_000, 2_000));
        assert!(!within_band(79_999, 100_000, 2_000));
        assert_eq!(sanity_band(u64::MAX, 2_000).1, u64::MAX);
    }

    fn v1_account(status: u32) -> [u8; PYTH_V1_MIN_LEN] {
        let mut d = [0u8; PYTH_V1_MIN_LEN];
        d[0..4].copy_from_slice(&PYTH_V1_MAGIC.to_le_bytes());
        d[4..8].copy_from_slice(&PYTH_V1_VERSION.to_le_bytes());
        d[8..12].copy_from_slice(&PYTH_V1_ACCOUNT_TYPE_PRICE.to_le_bytes());
        d[PYTH_V1_OFFSET_EXPO..][..4].copy_from_slice(&(-8i32).to_le_bytes());
        d[PYTH_V1_OFFSET_AGG_PRICE..][..8].copy_from_slice(&6_000_000i64.to_le_bytes());
        d[PYTH_V1_OFFSET_AGG_CONF..][..8].copy_from_slice(&1_500u64.to_le_bytes());
        d[PYTH_V1_OFFSET_AGG_STATUS..][..4].copy_from_slice(&status.to_le_bytes());
        d[PYTH_V1_OFFSET_AGG_PUB_SLOT..][..8].copy_from_slice(&77u64.to_le_bytes());
        d
    }

    #[test]
    fn v1_parse() {
        let a = pyth_v1::parse(&v1_account(PYTH_V1_STATUS_TRADING)).unwrap();
        assert_eq!(
            a,
            pyth_v1::Aggregate {
                price: 6_000_000,
                conf: 1_500,
                expo: -8,
                pub_slot: 77
            }
        );
        assert_eq!(
            pyth_v1::parse(&v1_account(2)),
            Err(pyth_v1::Error::NotTrading { status: 2 })
        );
        assert_eq!(
            pyth_v1::parse(&[0u8; 10]),
            Err(pyth_v1::Error::TooSmall {
                len: 10,
                need: PYTH_V1_MIN_LEN
            })
        );
    }

    /// Borsh-encode a PriceUpdateV2 the way pyth-solana-receiver does.
    fn v2_account(full: bool) -> [u8; PYTH_V2_MIN_LEN] {
        let mut d = [0u8; PYTH_V2_MIN_LEN];
        d[0..8].copy_from_slice(&PYTH_V2_PRICE_UPDATE_DISCRIMINATOR);
        let mut at = 40;
        if full {
            d[at] = PYTH_V2_VERIFICATION_FULL;
            at += 1;
        } else {
            d[at] = PYTH_V2_VERIFICATION_PARTIAL;
            d[at + 1] = 13;
            at += 2;
        }
        at += 32; // feed_id
        d[at..at + 8].copy_from_slice(&6_000_000i64.to_le_bytes());
        d[at + 8..at + 16].copy_from_slice(&1_500u64.to_le_bytes());
        d[at + 16..at + 20].copy_from_slice(&(-8i32).to_le_bytes());
        d[at + 20..at + 28].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        at += 52; // publish_time, prev_publish_time, ema_price, ema_conf
        d[at..at + 8].copy_from_slice(&99u64.to_le_bytes());
        d
    }

    #[test]
    fn v2_parse_both_levels() {
        for (full, level) in [
            (
                false,
                pyth_v2::VerificationLevel::Partial { num_signatures: 13 },
            ),
            (true, pyth_v2::VerificationLevel::Full),
        ] {
            let u = pyth_v2::parse(&v2_account(full)).unwrap();
            assert_eq!(
                u,
                pyth_v2::PriceUpdate {
                    verification_level: level,
                    price: 6_000_000,
                    conf: 1_500,
                    exponent: -8,
                    publish_time: 1_700_000_000,
                    posted_slot: 99,
                }
            );
        }
    }

    #[test]
    fn v2_rejects() {
        let mut d = v2_account(true);
        d[40] = 7;
        assert_eq!(
            pyth_v2::parse(&d),
            Err(pyth_v2::Error::UnknownVerificationLevel(7))
        );
        d[0] ^= 1;
        assert_eq!(pyth_v2::parse(&d), Err(pyth_v2::Error::BadDiscriminator));
    }
}
