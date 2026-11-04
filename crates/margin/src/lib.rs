//! Margin and liquidation math for a perp: equity, initial and maintenance
//! margin, the liquidation penalty split, and how much the insurance fund
//! covers when a position closes underwater.
//!
//! Pure integer math in quote units: no account layouts, no Solana types,
//! `no_std`. Ratios are basis points (`10_000` = 100%). The `openhl-core`
//! program calls these from `OpenPosition`, `ClosePosition` and `Liquidate`
//! with its own parameters (10% initial, 5% maintenance, 1% penalty, half
//! of it to the insurance fund).

#![no_std]

pub use openhl_funding::funding_pnl;

/// `10_000` basis points = 100%.
pub const BPS_DENOMINATOR: u128 = 10_000;

/// `|size| * mark`.
#[inline]
pub fn notional(size: i64, mark: u64) -> u128 {
    (size.unsigned_abs() as u128) * (mark as u128)
}

/// Equity of a position: collateral + price PnL + funding PnL, signed
/// (negative means underwater).
///
/// `size` is signed (long > 0, short < 0); `funding_snapshot` is the
/// cumulative funding index the position recorded when it opened.
#[inline]
pub fn equity(
    collateral: u64,
    size: i64,
    entry_price: u64,
    mark: u64,
    funding_snapshot: i64,
    funding_index_now: i64,
) -> i128 {
    let price_pnl = (size as i128) * ((mark as i128) - (entry_price as i128));
    let funding_pnl = funding_pnl(size, funding_snapshot, funding_index_now);
    (collateral as i128) + price_pnl + funding_pnl
}

/// Collateral needed to open a position of this notional.
#[inline]
pub fn initial_margin(notional: u128, initial_margin_bps: u64) -> u128 {
    notional * (initial_margin_bps as u128) / BPS_DENOMINATOR
}

/// Equity below which the position can be liquidated.
#[inline]
pub fn maintenance_margin(notional: u128, maint_margin_bps: u64) -> i128 {
    (notional * (maint_margin_bps as u128) / BPS_DENOMINATOR) as i128
}

/// `true` when equity is strictly below the maintenance requirement.
#[inline]
pub fn is_liquidatable(equity: i128, maintenance_margin: i128) -> bool {
    equity < maintenance_margin
}

/// Where the equity of a liquidated position goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidationSplit {
    /// Total penalty: `penalty_bps` of notional, never more than the
    /// positive equity (zero when equity <= 0).
    pub penalty: i128,
    /// Penalty minus the insurance fund's slice.
    pub to_liquidator: u64,
    /// `fund_share_bps` of the penalty, rounded down.
    pub to_insurance_fund: u64,
    /// What is left of positive equity after the penalty.
    pub to_owner: u64,
}

/// Split a liquidated position's equity between liquidator, insurance fund
/// and owner.
///
/// Rounding favours the liquidator: the fund's slice is floored, so the
/// liquidator gets at least half the penalty at a 5000 bps fund share.
#[inline]
pub fn liquidation_split(
    equity: i128,
    notional: u128,
    penalty_bps: u64,
    fund_share_bps: u64,
) -> LiquidationSplit {
    let raw_penalty =
        (notional * (penalty_bps as u128) / BPS_DENOMINATOR).min(i64::MAX as u128) as i128;
    let equity_positive = if equity < 0 { 0 } else { equity };
    let penalty = raw_penalty.min(equity_positive);
    let owner_remainder = (equity_positive - penalty).max(0);

    let fund_slice = ((penalty as u128) * (fund_share_bps as u128) / BPS_DENOMINATOR) as i128;
    let liquidator_slice = penalty - fund_slice;

    LiquidationSplit {
        penalty,
        to_liquidator: liquidator_slice as u64,
        to_insurance_fund: fund_slice as u64,
        to_owner: owner_remainder as u64,
    }
}

/// What a voluntary close pays the owner: the equity, or 0 if underwater.
#[inline]
pub fn close_payout(equity: i128) -> u64 {
    if equity < 0 {
        0
    } else {
        equity as u64
    }
}

/// How much the insurance fund pays in when a position closes with negative
/// equity: the loss, capped at the fund's balance. Zero when equity >= 0.
#[inline]
pub fn insurance_draw(equity: i128, fund_balance: u64) -> u64 {
    if equity < 0 {
        let shortfall = (-equity).min(u64::MAX as i128) as u64;
        shortfall.min(fund_balance)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equity_long_and_short() {
        // long 10 @ 100, mark 90, collateral 200 → 200 - 100
        assert_eq!(equity(200, 10, 100, 90, 0, 0), 100);
        // short 10 @ 100, mark 90 → 200 + 100
        assert_eq!(equity(200, -10, 100, 90, 0, 0), 300);
        // funding: index +1e9 on a long of 10 → +10
        assert_eq!(equity(200, 10, 100, 100, 0, 1_000_000_000), 210);
    }

    #[test]
    fn margins() {
        let n = notional(-10, 100);
        assert_eq!(n, 1_000);
        assert_eq!(initial_margin(n, 1_000), 100);
        assert_eq!(maintenance_margin(n, 500), 50);
        assert!(is_liquidatable(49, 50));
        assert!(!is_liquidatable(50, 50));
    }

    #[test]
    fn split_caps_penalty_at_equity() {
        // notional 10_000, 1% penalty = 100, equity 40 → penalty 40
        let s = liquidation_split(40, 10_000, 100, 5_000);
        assert_eq!(
            s,
            LiquidationSplit {
                penalty: 40,
                to_liquidator: 20,
                to_insurance_fund: 20,
                to_owner: 0
            }
        );
        // equity 400 → penalty 100, owner keeps 300
        let s = liquidation_split(400, 10_000, 100, 5_000);
        assert_eq!((s.penalty, s.to_owner), (100, 300));
        // odd penalty: fund floors, liquidator gets the extra unit
        let s = liquidation_split(401, 10_100, 100, 5_000);
        assert_eq!(
            (s.penalty, s.to_insurance_fund, s.to_liquidator),
            (101, 50, 51)
        );
        // underwater → nothing to split
        let s = liquidation_split(-5, 10_000, 100, 5_000);
        assert_eq!(
            (s.penalty, s.to_liquidator, s.to_insurance_fund, s.to_owner),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn close_and_draw() {
        assert_eq!(close_payout(-1), 0);
        assert_eq!(close_payout(7), 7);
        assert_eq!(insurance_draw(-30, 100), 30);
        assert_eq!(insurance_draw(-300, 100), 100);
        assert_eq!(insurance_draw(5, 100), 0);
    }
}
