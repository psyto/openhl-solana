//! Account-binding checks: an account the caller passes must belong to the
//! other accounts it is used with. Each case runs the real handler natively
//! (CPIs and the clock are stubbed) and shows the mismatched call is refused
//! while the matching call goes through.

use bytemuck::Zeroable;
use openhl_core::{
    process_instruction, FUNDING_SEED, INSURANCE_FUND_TOKEN_SEED, ORACLE_SEED, SPL_TOKEN_PROGRAM_ID,
    TRADING_VAULT_TOKEN_SEED, VAULT_AUTH_SEED, VAULT_SEED,
};
use openhl_state::*;
use solana_program::{
    account_info::AccountInfo, clock::Clock, entrypoint::ProgramResult, instruction::Instruction,
    program_error::ProgramError, program_stubs, pubkey::Pubkey,
};

const NOW_SLOT: u64 = 1_000;

/// Clock at NOW_SLOT; every CPI (SPL Token transfers) succeeds.
struct Stubs;
impl program_stubs::SyscallStubs for Stubs {
    fn sol_get_clock_sysvar(&self, var_addr: *mut u8) -> u64 {
        let clock = Clock {
            slot: NOW_SLOT,
            ..Clock::default()
        };
        unsafe { *(var_addr as *mut Clock) = clock };
        0
    }
    fn sol_invoke_signed(
        &self,
        _ix: &Instruction,
        _infos: &[AccountInfo],
        _seeds: &[&[&[u8]]],
    ) -> ProgramResult {
        Ok(())
    }
}

fn stubs() {
    program_stubs::set_syscall_stubs(Box::new(Stubs));
}

struct Acc {
    key: Pubkey,
    owner: Pubkey,
    lamports: u64,
    data: Vec<u8>,
    signer: bool,
}

fn acc(key: Pubkey, owner: Pubkey, data: Vec<u8>) -> Acc {
    Acc {
        key,
        owner,
        lamports: 1_000_000,
        data,
        signer: false,
    }
}

fn signer(key: Pubkey) -> Acc {
    Acc {
        signer: true,
        ..acc(key, Pubkey::default(), vec![])
    }
}

fn pod<T: bytemuck::Pod>(v: T) -> Vec<u8> {
    bytemuck::bytes_of(&v).to_vec()
}

/// An SPL Token account's leading fields: mint (32) + owner (32).
fn token_account(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d
}

fn run(program_id: &Pubkey, accs: &mut [Acc], ix: &[u8]) -> ProgramResult {
    let infos: Vec<AccountInfo> = accs
        .iter_mut()
        .map(|a| {
            AccountInfo::new(
                &a.key,
                a.signer,
                true,
                &mut a.lamports,
                &mut a.data,
                &a.owner,
                false,
                0,
            )
        })
        .collect();
    process_instruction(program_id, &infos, ix)
}

fn pda(seeds: &[&[u8]], program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program_id).0
}

// ---- VaultWithdraw (tag 21) -------------------------------------------------

fn withdraw_accounts(program_id: &Pubkey, share_vault: Pubkey) -> Vec<Acc> {
    let (owner, vault, market, mint) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let mut v = TradingVault::zeroed();
    v.discriminator = TRADING_VAULT_DISCRIMINATOR;
    v.market = market.to_bytes();
    v.mint = mint.to_bytes();
    v.total_shares = 1_000;
    v.total_assets = 1_000;
    let mut s = VaultShare::zeroed();
    s.discriminator = VAULT_SHARE_DISCRIMINATOR;
    s.vault = if share_vault == Pubkey::default() {
        vault.to_bytes()
    } else {
        share_vault.to_bytes()
    };
    s.owner = owner.to_bytes();
    s.shares = 500;
    s.cost_basis = 500;
    vec![
        signer(owner),
        acc(vault, *program_id, pod(v)),
        acc(Pubkey::new_unique(), *program_id, pod(s)),
        acc(market, Pubkey::default(), vec![]),
        acc(mint, SPL_TOKEN_PROGRAM_ID, vec![]),
        acc(
            Pubkey::new_unique(),
            SPL_TOKEN_PROGRAM_ID,
            token_account(&mint, &owner),
        ),
        acc(
            pda(&[TRADING_VAULT_TOKEN_SEED, vault.as_ref()], program_id),
            SPL_TOKEN_PROGRAM_ID,
            vec![],
        ),
        acc(SPL_TOKEN_PROGRAM_ID, Pubkey::default(), vec![]),
    ]
}

fn withdraw_ix(shares: u64) -> Vec<u8> {
    let mut ix = vec![21u8];
    ix.extend_from_slice(&shares.to_le_bytes());
    ix
}

#[test]
fn vault_withdraw_refuses_a_share_of_another_vault() {
    stubs();
    let program_id = Pubkey::new_unique();

    let mut ok = withdraw_accounts(&program_id, Pubkey::default());
    assert_eq!(run(&program_id, &mut ok, &withdraw_ix(500)), Ok(()));

    let mut other = withdraw_accounts(&program_id, Pubkey::new_unique());
    assert_eq!(
        run(&program_id, &mut other, &withdraw_ix(500)),
        Err(ProgramError::InvalidAccountData)
    );
    let vault: &TradingVault = bytemuck::from_bytes(&other[1].data);
    assert_eq!(vault.total_shares, 1_000, "nothing burned");
}

// ---- ClosePosition (tag 17) / Liquidate (tag 18) ----------------------------

struct PositionSetup {
    program_id: Pubkey,
    user: Pubkey,
    position_market: Pubkey,
    market: Pubkey,
    mint: Pubkey,
}

impl PositionSetup {
    fn new(same_market: bool) -> Self {
        let market = Pubkey::new_unique();
        Self {
            program_id: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            position_market: if same_market {
                market
            } else {
                Pubkey::new_unique()
            },
            market,
            mint: Pubkey::new_unique(),
        }
    }

    /// position, oracle, funding — a 10 long @ 100 with `collateral`,
    /// oracle at `mark`.
    fn core(&self, collateral: u64, mark: i64) -> [Acc; 3] {
        let p = &self.program_id;
        let mut pos = Position::zeroed();
        pos.discriminator = POSITION_DISCRIMINATOR;
        pos.user = self.user.to_bytes();
        pos.market = self.position_market.to_bytes();
        pos.size = 10;
        pos.entry_price = 100;
        pos.collateral = collateral;
        let mut o = Oracle::zeroed();
        o.discriminator = ORACLE_DISCRIMINATOR;
        o.price = mark;
        o.publish_slot = NOW_SLOT;
        let mut f = FundingState::zeroed();
        f.discriminator = FUNDING_DISCRIMINATOR;
        [
            acc(Pubkey::new_unique(), *p, pod(pos)),
            acc(pda(&[ORACLE_SEED, self.market.as_ref()], p), *p, pod(o)),
            acc(pda(&[FUNDING_SEED, self.market.as_ref()], p), *p, pod(f)),
        ]
    }

    fn vault_and_fund(&self) -> [Acc; 5] {
        let (p, m, mint) = (&self.program_id, &self.market, &self.mint);
        let mut fund = InsuranceFund::zeroed();
        fund.discriminator = INSURANCE_FUND_DISCRIMINATOR;
        fund.market = m.to_bytes();
        fund.mint = mint.to_bytes();
        fund.balance = 1_000_000;
        [
            acc(
                pda(&[VAULT_SEED, m.as_ref(), mint.as_ref()], p),
                SPL_TOKEN_PROGRAM_ID,
                vec![],
            ),
            acc(
                pda(&[VAULT_AUTH_SEED, m.as_ref()], p),
                Pubkey::default(),
                vec![],
            ),
            acc(SPL_TOKEN_PROGRAM_ID, Pubkey::default(), vec![]),
            acc(Pubkey::new_unique(), *p, pod(fund)),
            acc(
                pda(&[INSURANCE_FUND_TOKEN_SEED, m.as_ref(), mint.as_ref()], p),
                SPL_TOKEN_PROGRAM_ID,
                vec![],
            ),
        ]
    }

    fn close(&self) -> ProgramResult {
        let [pos, oracle, funding] = self.core(1_000, 100);
        let [vt, va, tp, fund, ft] = self.vault_and_fund();
        let mut accs = vec![
            signer(self.user),
            pos,
            oracle,
            funding,
            acc(self.market, Pubkey::default(), vec![]),
            acc(self.mint, SPL_TOKEN_PROGRAM_ID, vec![]),
            acc(
                Pubkey::new_unique(),
                SPL_TOKEN_PROGRAM_ID,
                token_account(&self.mint, &self.user),
            ),
            vt,
            va,
            tp,
            fund,
            ft,
        ];
        run(&self.program_id, &mut accs, &[17])
    }

    /// Liquidate at mark 96 with 50 collateral: equity 10 < maintenance 48.
    fn liquidate(&self, owner_token_owner: Pubkey) -> ProgramResult {
        let [pos, oracle, funding] = self.core(50, 96);
        let [vt, va, tp, fund, ft] = self.vault_and_fund();
        let liquidator = Pubkey::new_unique();
        let mut accs = vec![
            signer(liquidator),
            pos,
            oracle,
            funding,
            acc(self.market, Pubkey::default(), vec![]),
            acc(self.mint, SPL_TOKEN_PROGRAM_ID, vec![]),
            acc(
                Pubkey::new_unique(),
                SPL_TOKEN_PROGRAM_ID,
                token_account(&self.mint, &owner_token_owner),
            ),
            acc(
                Pubkey::new_unique(),
                SPL_TOKEN_PROGRAM_ID,
                token_account(&self.mint, &liquidator),
            ),
            vt,
            va,
            tp,
            fund,
            ft,
        ];
        run(&self.program_id, &mut accs, &[18])
    }
}

#[test]
fn close_position_refuses_another_markets_accounts() {
    stubs();
    assert_eq!(PositionSetup::new(true).close(), Ok(()));
    assert_eq!(
        PositionSetup::new(false).close(),
        Err(ProgramError::InvalidAccountData)
    );
}

#[test]
fn liquidate_refuses_another_markets_accounts() {
    stubs();
    let same = PositionSetup::new(true);
    assert_eq!(same.liquidate(same.user), Ok(()));
    let other = PositionSetup::new(false);
    assert_eq!(
        other.liquidate(other.user),
        Err(ProgramError::InvalidAccountData)
    );
}

#[test]
fn liquidate_pays_the_remainder_only_to_the_owner() {
    stubs();
    let s = PositionSetup::new(true);
    assert_eq!(
        s.liquidate(Pubkey::new_unique()),
        Err(ProgramError::InvalidAccountData)
    );
}
