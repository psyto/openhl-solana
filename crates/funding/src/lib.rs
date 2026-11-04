//! Funding for a perp: clamp the rate a keeper proposes, then accrue the
//! cumulative funding index over the time since the last update.
//!
//! The index is a step function. Each update commits the rate that was in
//! effect during the interval just ended, then installs the new rate for the
//! next one. A position snapshots the index when it opens; its funding PnL is
//! `(index_now - snapshot) * size / FUNDING_INDEX_SCALE`
//! ([`funding_pnl`]).
//!
//! Pure integer math: no Solana types, no I/O, `no_std`. The `openhl-core`
//! program calls these from `UpdateFunding` and the position handlers.

#![no_std]

/// Fixed-point scale of the cumulative index: one unit of index per second
/// of a rate of 1 is worth `1 / FUNDING_INDEX_SCALE` quote units per base
/// unit of position.
pub const FUNDING_INDEX_SCALE: i128 = 1_000_000_000;

/// Clamp a keeper's proposed per-second rate into `[-max_abs, max_abs]`.
///
/// A keeper bug or compromise should not produce arbitrarily large funding
/// payments. `max_abs` must be non-negative.
#[inline]
pub fn clamp_rate(proposed: i64, max_abs: i64) -> i64 {
    proposed.max(-max_abs).min(max_abs)
}

/// The result of one accrual step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accrual {
    /// Seconds since the last update.
    pub elapsed: u64,
    /// `prior_rate * elapsed`, before saturation.
    pub delta: i128,
    /// The new cumulative index, saturated to the `i64` range.
    pub cumulative: i64,
}

/// Why an accrual step was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingError {
    /// `now_ts` is earlier than `last_update_ts`.
    ClockWentBackwards { elapsed: i64 },
}

/// Accrue `prior_rate_per_sec` over `[last_update_ts, now_ts]` onto
/// `cumulative`.
///
/// Saturates at the `i64` bounds instead of wrapping; with realistic rates a
/// book takes years to approach them.
#[inline]
pub fn accrue(
    cumulative: i64,
    prior_rate_per_sec: i64,
    last_update_ts: i64,
    now_ts: i64,
) -> Result<Accrual, FundingError> {
    let elapsed = now_ts.saturating_sub(last_update_ts);
    if elapsed < 0 {
        return Err(FundingError::ClockWentBackwards { elapsed });
    }
    let elapsed_u = elapsed as u64;

    let delta = (prior_rate_per_sec as i128) * (elapsed_u as i128);
    let new_cumulative = (cumulative as i128).saturating_add(delta);
    let clamped: i64 = if new_cumulative > i64::MAX as i128 {
        i64::MAX
    } else if new_cumulative < i64::MIN as i128 {
        i64::MIN
    } else {
        new_cumulative as i64
    };

    Ok(Accrual {
        elapsed: elapsed_u,
        delta,
        cumulative: clamped,
    })
}

/// Funding PnL of a position of signed `size` that snapshotted the index at
/// `snapshot` and is now valued at `index_now`, in quote units.
///
/// Positive means the position receives funding. Division truncates toward
/// zero.
#[inline]
pub fn funding_pnl(size: i64, snapshot: i64, index_now: i64) -> i128 {
    let delta = (index_now as i128) - (snapshot as i128);
    delta * (size as i128) / FUNDING_INDEX_SCALE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_caps_both_sides() {
        assert_eq!(clamp_rate(5, 3), 3);
        assert_eq!(clamp_rate(-5, 3), -3);
        assert_eq!(clamp_rate(2, 3), 2);
    }

    #[test]
    fn accrue_uses_prior_rate_times_elapsed() {
        let a = accrue(100, 7, 1_000, 1_010).unwrap();
        assert_eq!(
            a,
            Accrual {
                elapsed: 10,
                delta: 70,
                cumulative: 170
            }
        );
    }

    #[test]
    fn accrue_refuses_backwards_clock() {
        assert_eq!(
            accrue(0, 1, 10, 9),
            Err(FundingError::ClockWentBackwards { elapsed: -1 })
        );
    }

    #[test]
    fn accrue_saturates() {
        assert_eq!(accrue(i64::MAX - 1, 10, 0, 1).unwrap().cumulative, i64::MAX);
        assert_eq!(
            accrue(i64::MIN + 1, -10, 0, 1).unwrap().cumulative,
            i64::MIN
        );
    }

    #[test]
    fn pnl_sign_follows_size() {
        assert_eq!(funding_pnl(3, 0, 2_000_000_000), 6);
        assert_eq!(funding_pnl(-3, 0, 2_000_000_000), -6);
        // truncates toward zero
        assert_eq!(funding_pnl(1, 0, 1_500_000_000), 1);
        assert_eq!(funding_pnl(-1, 0, 1_500_000_000), -1);
    }
}
