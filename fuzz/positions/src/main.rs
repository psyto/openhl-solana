//! solinv harness for Chapter 11 — `OpenPosition`, `ClosePosition` and
//! `Liquidate`.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). Two markets in the
//! same quote mint Q, each with its oracle, funding state, token account
//! and insurance fund: `m` at price 100 and `n` at price 200. On `m`, A
//! holds a long and B a short, both healthy at 100; C has no position yet.
//! On `n`, D holds a long, so `n`'s token account has tokens to take.
//! E opened a long on `m` at 110 with 130 collateral, so at 100 its equity
//! (30) is under maintenance margin (50) but above the penalty (10): the
//! one position anyone may liquidate, with a remainder owed to E.
//! The specs open C's position, close A's, and liquidate B's, and solinv
//! swaps in the other market's oracle and funding state, another user's
//! position, and token accounts the caller does not hold.

use crucible_fuzzer::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;
use std::sync::Arc;

use solinv_fuzz::{HasContext, HasInstructionSet, InstructionSpec};

const CORE_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/openhl_core.so");

const CORE_ID: Pubkey = Pubkey::new_from_array([3u8; 32]);
const SYSTEM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);
const TOKEN_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// openhl-core instruction tags (programs/openhl-core/src/lib.rs).
const IX_CREATE_MARKET: u8 = 1;
const IX_CREATE_VAULT: u8 = 5;
const IX_CREATE_ORACLE: u8 = 11;
const IX_SET_ORACLE_PRICE: u8 = 12;
const IX_CREATE_FUNDING_STATE: u8 = 14;
const IX_OPEN_POSITION: u8 = 16;
const IX_CLOSE_POSITION: u8 = 17;
const IX_LIQUIDATE: u8 = 18;
const IX_CREATE_INSURANCE_FUND: u8 = 27;

const START_BALANCE: u64 = 1_000_000_000;
const PRICE_M: i64 = 100;
const PRICE_N: i64 = 200;
const SIZE: i64 = 10;
const COLLATERAL: u64 = 200; // initial margin at 100 is 10% of 1,000
const B_COLLATERAL: u64 = 1_000; // healthy at 100, under maintenance margin at 200

fn token() -> Pubkey {
    Pubkey::from_str(TOKEN_ID).unwrap()
}
fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &CORE_ID).0
}
fn market_pda(base: &Pubkey, quote: &Pubkey) -> Pubkey {
    pda(&[b"market", base.as_ref(), quote.as_ref()])
}
fn oracle(m: &Pubkey) -> Pubkey {
    pda(&[b"oracle", m.as_ref()])
}
fn funding(m: &Pubkey) -> Pubkey {
    pda(&[b"funding", m.as_ref()])
}
fn position(user: &Pubkey, m: &Pubkey) -> Pubkey {
    pda(&[b"position", user.as_ref(), m.as_ref()])
}
fn vault_token(m: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[b"vault", m.as_ref(), mint.as_ref()])
}
fn vault_auth(m: &Pubkey) -> Pubkey {
    pda(&[b"vault_auth", m.as_ref()])
}
fn fund(m: &Pubkey) -> Pubkey {
    pda(&[b"insurance_fund", m.as_ref()])
}
fn fund_token(m: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[b"insurance_fund_token", m.as_ref(), mint.as_ref()])
}

/// An SPL Token mint (82 bytes).
fn mint_data(decimals: u8) -> Vec<u8> {
    let mut d = vec![0u8; 82];
    d[36..44].copy_from_slice(&(START_BALANCE * 8).to_le_bytes()); // supply
    d[44] = decimals;
    d[45] = 1; // is_initialized
    d
}

/// An SPL Token account (165 bytes).
fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1; // state = Initialized
    d
}

fn open_data(size: i64, collateral: u64) -> Vec<u8> {
    let mut d = vec![IX_OPEN_POSITION];
    d.extend_from_slice(&size.to_le_bytes());
    d.extend_from_slice(&collateral.to_le_bytes());
    d
}

#[derive(Clone)]
struct PositionsFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    a: Arc<Keypair>,
    b: Arc<Keypair>,
    c: Arc<Keypair>,
    l: Arc<Keypair>,
    d: Arc<Keypair>,
    e: Arc<Keypair>,
    q: Pubkey,
    m: Pubkey,
    n: Pubkey,
    a_q: Pubkey,
    b_q: Pubkey,
    c_q: Pubkey,
    l_q: Pubkey,
    d_q: Pubkey,
    e_q: Pubkey,
    /// An address that is not a PDA of this program.
    stranger: Pubkey,
}

impl PositionsFixture {
    fn send(&mut self, ix: Instruction, signers: &[&Keypair], what: &str) {
        let payer = Arc::clone(&self.payer);
        let mut all: Vec<&Keypair> = vec![&*payer];
        all.extend(signers.iter().copied().filter(|k| k.pubkey() != payer.pubkey()));
        let outcome = self
            .ctx
            .raw_call(ix)
            .fee_payer(&*payer)
            .signers(&all)
            .send()
            .unwrap_or_else(|e| panic!("{what}: send: {e:?}"));
        // send() is Ok when the program fails; check the outcome.
        assert!(outcome.is_success(), "{what} failed: {outcome:?}");
    }

    fn put(&mut self, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
        self.ctx
            .write_account(&key, Account { lamports: 10_000_000, data, owner, executable: false, rent_epoch: 0 })
            .expect("write account");
    }

    /// Sends `ix` and returns whether it succeeded, with the outcome's
    /// Debug text (it includes the program logs) to assert on.
    fn send_logged(&mut self, ix: Instruction, signer: Arc<Keypair>) -> (bool, String) {
        let payer = Arc::clone(&self.payer);
        match self.ctx.raw_call(ix).fee_payer(&*payer).signers(&[&*payer, &*signer]).send() {
            Ok(o) => (o.is_success(), format!("{o:?}")),
            Err(e) => (false, format!("{e:?}")),
        }
    }

    fn ix(&self, accounts: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
        Instruction { program_id: CORE_ID, accounts, data }
    }

    fn set_price(&mut self, market: Pubkey, price: i64) {
        let mut d = vec![IX_SET_ORACLE_PRICE];
        d.extend_from_slice(&price.to_le_bytes());
        d.extend_from_slice(&1u64.to_le_bytes());
        d.extend_from_slice(&0i32.to_le_bytes());
        let p = self.payer.pubkey();
        let ix = self.ix(vec![AccountMeta::new_readonly(p, true), AccountMeta::new(oracle(&market), false)], d);
        self.send(ix, &[], "set oracle price");
    }

    /// Market at its PDA, oracle at `price`, funding state, token account
    /// and insurance fund (Chapters 3, 6, 9, 10, 11).
    fn market(&mut self, market: Pubkey, base: Pubkey, price: i64) {
        let (p, q) = (self.payer.pubkey(), self.q);
        let mut d = vec![IX_CREATE_MARKET];
        d.extend_from_slice(p.as_ref());
        d.extend_from_slice(base.as_ref());
        d.extend_from_slice(q.as_ref());
        d.extend_from_slice(&1u64.to_le_bytes());
        d.extend_from_slice(&1u64.to_le_bytes());
        let ix = self.ix(
            vec![AccountMeta::new(p, true), AccountMeta::new(market, false), AccountMeta::new_readonly(SYSTEM_ID, false)],
            d,
        );
        self.send(ix, &[], "create market");
        let ix = self.ix(
            vec![
                AccountMeta::new(p, true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new_readonly(q, false),
                AccountMeta::new(vault_token(&market, &q), false),
                AccountMeta::new_readonly(vault_auth(&market), false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
                AccountMeta::new_readonly(token(), false),
            ],
            vec![IX_CREATE_VAULT],
        );
        self.send(ix, &[], "create vault");
        let ix = self.ix(
            vec![
                AccountMeta::new(p, true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(oracle(&market), false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
            ],
            vec![IX_CREATE_ORACLE],
        );
        self.send(ix, &[], "create oracle");
        self.set_price(market, price);
        let mut d = vec![IX_CREATE_FUNDING_STATE];
        d.extend_from_slice(&3_600u64.to_le_bytes());
        let ix = self.ix(
            vec![
                AccountMeta::new(p, true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(funding(&market), false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
            ],
            d,
        );
        self.send(ix, &[], "create funding state");
        let ix = self.ix(
            vec![
                AccountMeta::new(p, true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new_readonly(q, false),
                AccountMeta::new(fund(&market), false),
                AccountMeta::new(fund_token(&market, &q), false),
                AccountMeta::new_readonly(vault_auth(&market), false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
                AccountMeta::new_readonly(token(), false),
            ],
            vec![IX_CREATE_INSURANCE_FUND],
        );
        self.send(ix, &[], "create insurance fund");
    }

    fn open_metas(&self, user: &Pubkey, from: &Pubkey) -> Vec<AccountMeta> {
        self.open_metas_on(user, from, self.m)
    }

    fn open_metas_on(&self, user: &Pubkey, from: &Pubkey, m: Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(m, false),
            AccountMeta::new(position(user, &m), false),
            AccountMeta::new_readonly(oracle(&m), false),
            AccountMeta::new_readonly(funding(&m), false),
            AccountMeta::new_readonly(self.q, false),
            AccountMeta::new(*from, false),
            AccountMeta::new(vault_token(&m, &self.q), false),
            AccountMeta::new_readonly(SYSTEM_ID, false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    fn close_metas(&self, user: &Pubkey, to: &Pubkey) -> Vec<AccountMeta> {
        let (m, q) = (self.m, self.q);
        vec![
            AccountMeta::new_readonly(*user, true),
            AccountMeta::new(position(user, &m), false),
            AccountMeta::new_readonly(oracle(&m), false),
            AccountMeta::new_readonly(funding(&m), false),
            AccountMeta::new_readonly(m, false),
            AccountMeta::new_readonly(q, false),
            AccountMeta::new(*to, false),
            AccountMeta::new(vault_token(&m, &q), false),
            AccountMeta::new_readonly(vault_auth(&m), false),
            AccountMeta::new_readonly(token(), false),
            AccountMeta::new(fund(&m), false),
            AccountMeta::new(fund_token(&m, &q), false),
        ]
    }

    fn liquidate_metas(&self, owner: &Pubkey, owner_to: &Pubkey) -> Vec<AccountMeta> {
        let (m, q) = (self.m, self.q);
        vec![
            AccountMeta::new_readonly(self.l.pubkey(), true),
            AccountMeta::new(position(owner, &m), false),
            AccountMeta::new_readonly(oracle(&m), false),
            AccountMeta::new_readonly(funding(&m), false),
            AccountMeta::new_readonly(m, false),
            AccountMeta::new_readonly(q, false),
            AccountMeta::new(*owner_to, false),
            AccountMeta::new(self.l_q, false),
            AccountMeta::new(vault_token(&m, &q), false),
            AccountMeta::new_readonly(vault_auth(&m), false),
            AccountMeta::new_readonly(token(), false),
            AccountMeta::new(fund(&m), false),
            AccountMeta::new(fund_token(&m, &q), false),
        ]
    }
}

#[fuzz_fixture]
impl PositionsFixture {
    pub fn setup() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");

        let [payer, a, b, c, l, d, e] = [(); 7].map(|_| Arc::new(Keypair::new()));
        for kp in [&payer, &a, &b, &c, &l, &d, &e] {
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(SYSTEM_ID)
                .create()
                .unwrap();
        }
        let q = Keypair::new().pubkey();
        let (base_m, base_n) = (Keypair::new().pubkey(), Keypair::new().pubkey());
        let mut f = Self {
            ctx,
            payer: Arc::clone(&payer),
            a: Arc::clone(&a),
            b: Arc::clone(&b),
            c: Arc::clone(&c),
            l: Arc::clone(&l),
            d: Arc::clone(&d),
            e: Arc::clone(&e),
            q,
            m: market_pda(&base_m, &q),
            n: market_pda(&base_n, &q),
            a_q: Keypair::new().pubkey(),
            b_q: Keypair::new().pubkey(),
            c_q: Keypair::new().pubkey(),
            l_q: Keypair::new().pubkey(),
            d_q: Keypair::new().pubkey(),
            e_q: Keypair::new().pubkey(),
            stranger: Keypair::new().pubkey(),
        };
        f.put(q, token(), mint_data(6));
        for (acct, kp) in [(f.a_q, &a), (f.b_q, &b), (f.c_q, &c), (f.l_q, &l), (f.d_q, &d), (f.e_q, &e)] {
            f.put(acct, token(), token_account_data(&q, &kp.pubkey(), START_BALANCE));
        }
        let (m, n) = (f.m, f.n);
        f.market(m, base_m, PRICE_M);
        f.market(n, base_n, PRICE_N);

        // E long at 110 with 130 (initial margin is 110 of 1,100 notional);
        // back at 100 its equity is 30: under maintenance 50, above the 10
        // penalty, so liquidation owes E a remainder.
        f.set_price(m, 110);
        let ix = f.ix(f.open_metas(&e.pubkey(), &f.e_q), open_data(SIZE, 130));
        f.send(ix, &[&*e], "E opens long at 110");
        f.set_price(m, PRICE_M);

        // A long, B short, both on m at 100 and both healthy there.
        let ix = f.ix(f.open_metas(&a.pubkey(), &f.a_q), open_data(SIZE, COLLATERAL));
        f.send(ix, &[&*a], "A opens long");
        let ix = f.ix(f.open_metas(&b.pubkey(), &f.b_q), open_data(-SIZE, B_COLLATERAL));
        f.send(ix, &[&*b], "B opens short");
        let ix = f.ix(f.open_metas_on(&d.pubkey(), &f.d_q, n), open_data(SIZE, 5 * B_COLLATERAL));
        f.send(ix, &[&*d], "D opens long on n");
        f
    }

    /// Republish m's price at 98..=101, where A and B are healthy and E is
    /// liquidatable with a remainder: E's equity 10 × (p − 100) + 30 is
    /// above the 0.1 × p penalty and below the 0.5 × p maintenance margin.
    pub fn action_reprice(&mut self, #[range(98..102)] price: i64) -> bool {
        let m = self.m;
        self.set_price(m, price);
        true
    }
}

impl HasContext for PositionsFixture {
    fn ctx(&self) -> &TestContext {
        &self.ctx
    }
    fn ctx_mut(&mut self) -> &mut TestContext {
        &mut self.ctx
    }
    fn program_ids(&self) -> Vec<Pubkey> {
        vec![CORE_ID]
    }
    fn fee_payer(&self) -> Arc<Keypair> {
        Arc::clone(&self.payer)
    }
}

/// A spec with every optional detector off; each spec fills in what it
/// tests.
fn spec(name: &str, accounts: Vec<AccountMeta>, data: Vec<u8>, signer: Arc<Keypair>) -> InstructionSpec {
    let n = accounts.len();
    InstructionSpec {
        program_id: CORE_ID,
        name: name.into(),
        accounts,
        signer_indices: vec![0],
        optional_signer_indices: vec![],
        expected_owners: vec![None; n],
        expected_discriminators: vec![None; n],
        expected_pda_seeds: vec![None; n],
        creates_indices: vec![],
        swap_alternates: vec![vec![]; n],
        data_sample: data,
        signers: vec![signer],
        state_invariants: vec![],
        cu_budget: None,
        cpi_reentrancy: None,
        realloc_check: None,
        bump_seed_check: None,
        guards: vec![],
    }
}

impl HasInstructionSet for PositionsFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let (a, b, c, n) = (self.a.pubkey(), self.b.pubkey(), self.c.pubkey(), self.n);

        let mut open = spec("open_position", self.open_metas(&c, &self.c_q), open_data(SIZE, COLLATERAL), Arc::clone(&self.c));
        open.creates_indices = vec![2];
        open.swap_alternates[1] = vec![self.a_q]; // a "market" this program does not own
        open.swap_alternates[2] = vec![self.stranger]; // a position not at its PDA
        open.swap_alternates[3] = vec![oracle(&n)]; // another market's price
        open.swap_alternates[4] = vec![funding(&n)]; // another market's funding
        open.swap_alternates[6] = vec![self.a_q]; // pay with A's tokens
        open.swap_alternates[7] = vec![vault_token(&n, &self.q)]; // escrow into n's account

        let mut close = spec("close_position", self.close_metas(&a, &self.a_q), vec![IX_CLOSE_POSITION], Arc::clone(&self.a));
        close.swap_alternates[1] = vec![position(&b, &self.m)]; // close B's position as A
        close.swap_alternates[2] = vec![oracle(&n)];
        close.swap_alternates[3] = vec![funding(&n)];
        close.swap_alternates[7] = vec![vault_token(&n, &self.q)]; // pay from n's vault
        close.swap_alternates[10] = vec![fund(&n)];

        let mut liq = spec("liquidate", self.liquidate_metas(&b, &self.b_q), vec![IX_LIQUIDATE], Arc::clone(&self.l));
        liq.swap_alternates[2] = vec![oracle(&n)]; // B is underwater at n's price
        liq.swap_alternates[3] = vec![funding(&n)];
        liq.swap_alternates[6] = vec![self.l_q]; // B's remainder to the liquidator

        let mut liq_e = spec("liquidate_e", self.liquidate_metas(&self.e.pubkey(), &self.e_q), vec![IX_LIQUIDATE], Arc::clone(&self.l));
        liq_e.swap_alternates[6] = vec![self.l_q]; // E's remainder to the liquidator

        vec![open, close, liq, liq_e]
    }
}

#[invariant_test]
fn invariant_ch11(fixture: &mut PositionsFixture) {
    use solinv_core::invariants::account_swap::{self, SwapSet};
    solinv_core::invariants::signer_skip::check(fixture);
    account_swap::check(fixture);
    // A position on m, settled against n: n's market and everything
    // derived from it, swapped together.
    let (n, q) = (fixture.n, fixture.q);
    let whole_n = |slots: [usize; 7]| -> Vec<(usize, Pubkey)> {
        let accounts = [oracle(&n), funding(&n), n, vault_token(&n, &q), vault_auth(&n), fund(&n), fund_token(&n, &q)];
        slots.into_iter().zip(accounts).collect()
    };
    account_swap::check_sets(
        fixture,
        &[
            SwapSet { ix: "close_position".into(), swaps: whole_n([2, 3, 4, 7, 8, 10, 11]) },
            SwapSet { ix: "liquidate".into(), swaps: whole_n([2, 3, 4, 8, 9, 11, 12]) },
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closes its long on m while naming n's oracle, at 200.
    #[test]
    fn close_refuses_another_markets_oracle() {
        let mut f = PositionsFixture::setup();
        let (a, a_q, n) = (Arc::clone(&f.a), f.a_q, f.n);
        let before = f.ctx.token_balance(&a_q);
        let mut metas = f.close_metas(&a.pubkey(), &a_q);
        metas[2].pubkey = oracle(&n);
        let (ok, out) = f.send_logged(f.ix(metas, vec![IX_CLOSE_POSITION]), a);
        let got = f.ctx.token_balance(&a_q) - before;
        assert!(!ok, "A put in {COLLATERAL} and took {got} out by naming n's oracle");
        assert!(out.contains("oracle"), "refused for another reason: {out}");
    }

    /// Not a check: sends each spec once with its signer flag cleared and
    /// with each single-slot swap, and prints how it ended. Used for the
    /// "no" rows in §11.7 (`cargo test -- --ignored --nocapture probe`).
    #[test]
    #[ignore]
    fn probe() {
        let f0 = PositionsFixture::setup();
        for spec in f0.instructions() {
            let mut variants: Vec<(String, Vec<AccountMeta>)> = vec![];
            let mut m = spec.accounts.clone();
            m[0].is_signer = false;
            variants.push(("signer off".into(), m));
            for (i, alts) in spec.swap_alternates.iter().enumerate() {
                for alt in alts {
                    let mut m = spec.accounts.clone();
                    m[i].pubkey = *alt;
                    variants.push((format!("slot {i} swapped"), m));
                }
            }
            for (what, metas) in variants {
                let mut f = f0.clone();
                let signer = Arc::clone(&spec.signers[0]);
                let (ok, out) = f.send_logged(f.ix(metas, spec.data_sample.clone()), signer);
                let lines: Vec<&str> = out.split("\", \"").collect();
                let log = lines.iter().rev().find(|l| l.starts_with("Program log:")).copied().unwrap_or("-");
                let fail = lines.iter().rev().find(|l| l.contains(" failed: ")).map(|l| l.split(" failed: ").last().unwrap_or("")).unwrap_or("-");
                println!("PROBE {} | {what} | ok={ok} | {} | {}", spec.name, log.chars().take(100).collect::<String>(), fail.split('"').next().unwrap_or(""));
            }
        }
    }

    /// Collateral below 10% of notional is refused.
    #[test]
    fn open_below_initial_margin_is_refused() {
        let mut f = PositionsFixture::setup();
        let (c, c_q) = (Arc::clone(&f.c), f.c_q);
        let ix = f.ix(f.open_metas(&c.pubkey(), &c_q), open_data(SIZE, COLLATERAL / 2 - 1));
        let (ok, out) = f.send_logged(ix, c);
        assert!(!ok, "C opened 1,000 notional with {} collateral", COLLATERAL / 2 - 1);
        assert!(out.contains("< initial margin"), "refused for another reason: {out}");
    }

    /// B's short is healthy on m at 100, so it cannot be liquidated.
    #[test]
    fn healthy_position_is_not_liquidatable() {
        let mut f = PositionsFixture::setup();
        let (l, b_q) = (Arc::clone(&f.l), f.b_q);
        let ix = f.ix(f.liquidate_metas(&f.b.pubkey(), &b_q), vec![IX_LIQUIDATE]);
        let (ok, out) = f.send_logged(ix, l);
        assert!(!ok, "B's healthy short was liquidated");
        assert!(out.contains("not liquidatable"), "refused for another reason: {out}");
    }

    /// B's short is healthy on m at 100; n's oracle says 200.
    #[test]
    fn liquidate_refuses_another_markets_oracle() {
        let mut f = PositionsFixture::setup();
        let (l, b_q, n) = (Arc::clone(&f.l), f.b_q, f.n);
        let mut metas = f.liquidate_metas(&f.b.pubkey(), &b_q);
        metas[2].pubkey = oracle(&n);
        let (ok, out) = f.send_logged(f.ix(metas, vec![IX_LIQUIDATE]), l);
        assert!(!ok, "B's healthy short was liquidated at n's price");
        assert!(out.contains("oracle"), "refused for another reason: {out}");
    }
}
