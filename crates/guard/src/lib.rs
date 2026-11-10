//! A rolling exposure budget for a thin market: no stretch of `window_secs`
//! ever settles more than `cap` of notional (more than the largest cap in
//! force during it, if [`reconfigure`] lowered it).
//!
//! On a thin market the cheapest attack is to push the price and trade
//! against the move in the same minute. Capping what can settle per window
//! bounds what one push can take, whichever price the program reads. Ported
//! from `ExposureBudget.Rolling` in fabrknt/hl-price-guard, where replaying
//! the 2026-10-10 Papertrade incident showed a per-minute cap, not the price
//! source, was what made the attack unprofitable.
//!
//! The window is split into [`SLICES`] buckets. A check counts the current
//! bucket and the `SLICES` before it, so it sees between `window_secs` and
//! `window_secs + window_secs / SLICES` of history: strict, and conservative
//! by under one slice.
//!
//! Pure integer math on a `repr(C)` + `bytemuck::Pod` struct you keep in an
//! account. Your program passes the clock's unix timestamp in. A timestamp
//! earlier than the newest bucket is read as that bucket's time: a clock that
//! steps back can't overwrite a newer bucket or hide what it holds.

#![no_std]

use bytemuck::{Pod, Zeroable};

/// Buckets per window.
pub const SLICES: u64 = 6;

/// Why a spend or a change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardError {
    /// `used + notional` would exceed `cap`.
    BudgetExceeded { used: u64, requested: u64, cap: u64 },
    /// `window_secs` is not a positive multiple of [`SLICES`].
    BadWindow,
    /// Spending against settings other than the ones the budget was set up with.
    ConfigChanged,
    /// A change that has to wait until nothing spent under the old settings is in either window.
    NotDrained { ready_at: u64 },
}

/// One bucket: the epoch (timestamp / slice length) it counts, and how much it holds.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Slot {
    pub epoch: u64,
    pub amount: u64,
}

/// The budget's state. Zeroed means "not set up"; the first [`spend`] or
/// [`reconfigure`] binds `cap` and `window_secs`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Rolling {
    pub window_secs: u64,
    pub cap: u64,
    /// Timestamp of the last non-zero spend; meaningful only when `spent` is 1.
    pub last_spend_at: u64,
    pub spent: u64,
    pub slots: [Slot; SLICES as usize + 1],
}

impl Rolling {
    pub const LEN: usize = core::mem::size_of::<Rolling>();
}

/// Largest exposure `E` for which moving the price by `move_bps` pays less
/// than it costs: `E * move_bps / 10_000 * safety_factor <= cost_to_move`.
#[inline]
pub fn safe_cap(cost_to_move: u64, move_bps: u64, safety_factor: u64) -> u64 {
    let d = (move_bps as u128) * (safety_factor as u128);
    if d == 0 {
        return u64::MAX;
    }
    ((cost_to_move as u128) * 10_000 / d).min(u64::MAX as u128) as u64
}

fn check(window_secs: u64) -> Result<(), GuardError> {
    if window_secs < SLICES || window_secs % SLICES != 0 {
        return Err(GuardError::BadWindow);
    }
    Ok(())
}

fn live(s: &Slot) -> bool {
    !(s.epoch == 0 && s.amount == 0)
}

/// The epoch for `now`, never earlier than the newest bucket.
fn epoch(r: &Rolling, now: u64) -> u64 {
    let newest = r.slots.iter().filter(|s| live(s)).map(|s| s.epoch).max().unwrap_or(0);
    (now / (r.window_secs / SLICES)).max(newest)
}

fn sum(r: &Rolling, e: u64) -> u64 {
    let first = e.saturating_sub(SLICES);
    r.slots
        .iter()
        .filter(|s| s.epoch >= first && s.epoch <= e && live(s))
        .fold(0u64, |acc, s| acc.saturating_add(s.amount))
}

/// Notional counted against the cap at `now`.
pub fn used(r: &Rolling, now: u64) -> u64 {
    if r.window_secs == 0 {
        return 0;
    }
    sum(r, epoch(r, now))
}

/// Count `notional` against the budget at `now`, or refuse it. `cap` and
/// `window_secs` must be the ones the budget was set up with (the first
/// call sets them up); change them with [`reconfigure`].
pub fn spend(r: &mut Rolling, now: u64, notional: u64, cap: u64, window_secs: u64) -> Result<(), GuardError> {
    if r.window_secs == 0 {
        check(window_secs)?;
        r.cap = cap;
        r.window_secs = window_secs;
    } else if r.cap != cap || r.window_secs != window_secs {
        return Err(GuardError::ConfigChanged);
    }
    let e = epoch(r, now);
    let s = sum(r, e);
    if s > cap || notional > cap - s {
        return Err(GuardError::BudgetExceeded { used: s, requested: notional, cap });
    }
    // Nothing to record: writing an empty bucket could only erase one.
    if notional == 0 {
        return Ok(());
    }
    let i = (e % (SLICES + 1)) as usize;
    let prev = if r.slots[i].epoch == e { r.slots[i].amount } else { 0 };
    // prev + notional <= cap, so this can't overflow.
    r.slots[i] = Slot { epoch: e, amount: prev + notional };
    if notional != 0 {
        r.last_spend_at = now;
        r.spent = 1;
    }
    Ok(())
}

/// Change cap or window. Lowering the cap alone takes effect at once for new
/// spends (it does not undo what already settled, so a stretch spanning the
/// change is bounded by the higher cap); anything else waits until nothing
/// settled under the old settings is inside either window.
pub fn reconfigure(r: &mut Rolling, now: u64, cap: u64, window_secs: u64) -> Result<(), GuardError> {
    check(window_secs)?;
    if r.window_secs == window_secs && cap <= r.cap {
        r.cap = cap;
        return Ok(());
    }
    if r.window_secs != 0 {
        let longer = window_secs.max(r.window_secs);
        let ready_at = r.last_spend_at.saturating_add(longer).saturating_add(1);
        if r.spent == 1 && now < ready_at {
            return Err(GuardError::NotDrained { ready_at });
        }
        r.slots = [Slot::default(); SLICES as usize + 1];
    }
    r.cap = cap;
    r.window_secs = window_secs;
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const W: u64 = 60;

    #[test]
    fn a_full_cap_burst_waits_for_the_window() {
        let mut r = Rolling::default();
        spend(&mut r, 1_000, 100, 100, W).unwrap();
        assert!(matches!(spend(&mut r, 1_000, 1, 100, W), Err(GuardError::BudgetExceeded { .. })));
        assert!(spend(&mut r, 1_059, 1, 100, W).is_err());
        // Conservative by under one slice: free again once the slice holding the burst is out of view.
        spend(&mut r, 1_000 + W + W / SLICES, 100, 100, W).unwrap();
    }

    #[test]
    fn no_stretch_of_the_window_settles_more_than_the_cap() {
        // Deterministic pseudo-random spends; check every window-length stretch afterwards.
        let cap = 1_000u64;
        let mut r = Rolling::default();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut ok: Vec<(u64, u64)> = Vec::new();
        let mut t = 10_000u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            t += seed % 7;
            let n = seed % 400;
            if spend(&mut r, t, n, cap, W).is_ok() {
                ok.push((t, n));
            }
        }
        let mut j = 0;
        let mut acc = 0u64;
        for i in 0..ok.len() {
            acc += ok[i].1;
            while ok[i].0 - ok[j].0 >= W {
                acc -= ok[j].1;
                j += 1;
            }
            assert!(acc <= cap, "stretch ending {} settled {}", ok[i].0, acc);
        }
        assert!(ok.len() > 1_000);
    }

    #[test]
    fn settings_are_bound_and_changes_wait_for_the_old_window() {
        let mut r = Rolling::default();
        assert_eq!(spend(&mut r, 0, 10, 100, 61), Err(GuardError::BadWindow));
        spend(&mut r, 500, 50, 100, W).unwrap();
        assert_eq!(spend(&mut r, 500, 1, 200, W), Err(GuardError::ConfigChanged));
        // Lowering the cap is immediate.
        reconfigure(&mut r, 500, 80, W).unwrap();
        assert!(spend(&mut r, 500, 31, 80, W).is_err());
        // Raising it waits until the last spend is out of both windows.
        assert_eq!(reconfigure(&mut r, 520, 200, W), Err(GuardError::NotDrained { ready_at: 561 }));
        reconfigure(&mut r, 561, 200, W).unwrap();
        spend(&mut r, 561, 200, 200, W).unwrap();
    }

    #[test]
    fn a_clock_that_steps_back_cannot_erase_a_newer_bucket() {
        // Seven slices back lands on the same ring slot as the newest spend.
        let mut r = Rolling::default();
        let slice = W / SLICES;
        let t = 10_000 * slice;
        spend(&mut r, t, 100, 100, W).unwrap();
        let back = t - (SLICES + 1) * slice;
        // Neither a zero spend nor a real one from the earlier clock frees the budget.
        spend(&mut r, back, 0, 100, W).unwrap();
        assert_eq!(used(&r, back), 100);
        assert!(spend(&mut r, back, 1, 100, W).is_err());
        assert_eq!(used(&r, t), 100);
        assert!(spend(&mut r, t, 1, 100, W).is_err());
        // Time moves on past the window as usual.
        spend(&mut r, t + W + slice, 100, 100, W).unwrap();
    }

    #[test]
    fn a_zero_spend_records_nothing() {
        let mut r = Rolling::default();
        spend(&mut r, 1_000, 0, 100, W).unwrap();
        assert_eq!((r.cap, r.window_secs, r.spent), (100, W, 0));
        assert!(r.slots.iter().all(|s| *s == Slot::default()));
    }

    #[test]
    fn spend_at_time_zero_counts() {
        let mut r = Rolling::default();
        spend(&mut r, 0, 100, 100, W).unwrap();
        assert!(spend(&mut r, 5, 1, 100, W).is_err());
        assert_eq!(used(&r, 5), 100);
    }

    #[test]
    fn safe_cap_matches_the_solidity_formula() {
        // cost $130,873 to move 4 bps, safety 1 → $327M; with safety 3 → $109M.
        assert_eq!(safe_cap(130_873, 4, 1), 327_182_500);
        assert_eq!(safe_cap(130_873, 4, 3), 109_060_833);
    }
}
