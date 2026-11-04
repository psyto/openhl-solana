//! Share math for a pooled vault: how many shares a deposit mints, how many
//! assets a withdrawal returns, and how a partial withdrawal reduces the
//! depositor's cost basis.
//!
//! The first deposit mints 1:1. After that, shares track
//! `total_assets / total_shares`, which the manager moves by reporting NAV.
//! Pure integer math, rounded down in the vault's favour, `no_std`. The
//! `openhl-core` program calls these from `VaultDeposit` and `VaultWithdraw`.

#![no_std]

/// Why a share calculation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultError {
    /// The result does not fit in a `u64`.
    Overflow,
    /// The deposit is too small relative to NAV and would mint 0 shares.
    ZeroShares,
    /// A withdrawal from a vault with no shares outstanding.
    NoShares,
}

/// Shares minted for depositing `assets`.
#[inline]
pub fn shares_for_deposit(
    assets: u64,
    total_shares: u64,
    total_assets: u64,
) -> Result<u64, VaultError> {
    let shares = if total_shares == 0 || total_assets == 0 {
        assets
    } else {
        let s = (assets as u128) * (total_shares as u128) / (total_assets as u128);
        if s > u64::MAX as u128 {
            return Err(VaultError::Overflow);
        }
        s as u64
    };
    if shares == 0 {
        return Err(VaultError::ZeroShares);
    }
    Ok(shares)
}

/// Assets returned for burning `shares`.
#[inline]
pub fn assets_for_withdraw(
    shares: u64,
    total_shares: u64,
    total_assets: u64,
) -> Result<u64, VaultError> {
    if total_shares == 0 {
        return Err(VaultError::NoShares);
    }
    let a = (shares as u128) * (total_assets as u128) / (total_shares as u128);
    if a > u64::MAX as u128 {
        return Err(VaultError::Overflow);
    }
    Ok(a as u64)
}

/// How much of `cost_basis` to remove when `burned` of `shares_before`
/// shares are withdrawn (proportional, rounded down). `shares_before` must
/// be non-zero.
#[inline]
pub fn cost_basis_reduction(burned: u64, cost_basis: u64, shares_before: u64) -> u64 {
    ((burned as u128) * (cost_basis as u128) / (shares_before as u128)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_deposit_is_one_to_one() {
        assert_eq!(shares_for_deposit(1_000, 0, 0), Ok(1_000));
    }

    #[test]
    fn later_deposits_follow_nav() {
        // NAV doubled: 1000 shares back 2000 assets
        assert_eq!(shares_for_deposit(500, 1_000, 2_000), Ok(250));
        assert_eq!(
            shares_for_deposit(1, 1_000, 2_001),
            Err(VaultError::ZeroShares)
        );
        assert_eq!(assets_for_withdraw(250, 1_250, 2_500), Ok(500));
        assert_eq!(assets_for_withdraw(1, 0, 0), Err(VaultError::NoShares));
    }

    #[test]
    fn basis_reduces_proportionally() {
        assert_eq!(cost_basis_reduction(250, 1_000, 1_000), 250);
        assert_eq!(cost_basis_reduction(1, 10, 3), 3);
    }
}
