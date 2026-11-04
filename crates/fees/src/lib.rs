//! Protocol fee and builder-code share for a perp.
//!
//! Every order pays `fee_bps` of its notional (`price * size`) as the
//! protocol fee. When the order arrives through a builder (a front end or
//! aggregator), the builder is credited a share of that fee: the share it
//! registered, capped by the protocol.
//!
//! Pure integer math, `no_std`. The `openhl-core` program calls these from
//! the place-order instructions (10 bps fee, builder share capped at 50%).

#![no_std]

/// `10_000` basis points = 100%.
pub const BPS_DENOMINATOR: u128 = 10_000;

/// `price * size`, in quote units.
#[inline]
pub fn order_notional(price: u64, size: u64) -> u128 {
    (price as u128) * (size as u128)
}

/// `fee_bps` of `notional`, rounded down.
#[inline]
pub fn protocol_fee(notional: u128, fee_bps: u64) -> u64 {
    (notional * (fee_bps as u128) / BPS_DENOMINATOR) as u64
}

/// The share a builder is paid: what it registered, at most `cap_bps`.
#[inline]
pub fn builder_share_bps(registered_bps: u64, cap_bps: u64) -> u64 {
    if registered_bps > cap_bps {
        cap_bps
    } else {
        registered_bps
    }
}

/// The builder's cut of `protocol_fee` at `share_bps`, rounded down. The
/// rest stays with the protocol.
#[inline]
pub fn builder_share(protocol_fee: u64, share_bps: u64) -> u64 {
    ((protocol_fee as u128) * (share_bps as u128) / BPS_DENOMINATOR) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_and_share() {
        let n = order_notional(50_000, 3);
        assert_eq!(n, 150_000);
        let fee = protocol_fee(n, 10);
        assert_eq!(fee, 150);
        assert_eq!(builder_share_bps(8_000, 5_000), 5_000);
        assert_eq!(builder_share_bps(2_000, 5_000), 2_000);
        assert_eq!(builder_share(fee, 5_000), 75);
        assert_eq!(builder_share(151, 5_000), 75);
    }
}
