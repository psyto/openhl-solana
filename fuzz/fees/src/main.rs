//! solinv harness for Chapter 13 — `PlaceOrderWithBuilder` and
//! `ClaimBuilderFees`.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). Builder B registers
//! in the quote mint Q; user U trades through B. Three markets, each with
//! its book and a fresh oracle:
//!
//! - `m`: the market for `(base, Q)` at its PDA.
//! - `m2`: a second market for the same pair, made by `Initialize`
//!   (Chapter 2) at an address that is not the PDA (Chapter 6 §6.6).
//! - `jm`: a market for `(base, J)`, where J is a mint anyone can make.
//!
//! What the specs declare, beyond the account checks every harness does:
//!
//! - a relation, `profile.accumulated_fees <= fee_vault.amount`: the fee
//!   vault can always pay the builder what it is owed;
//! - a guard on placing an order, `profile.fee_mint == market.quote_mint`:
//!   a builder is credited only in the mint it is paid out in.

use crucible_fuzzer::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;
use std::sync::Arc;

use solinv_fuzz::{
    GuardCondition, GuardOp, GuardOperand, HasContext, HasInstructionSet, InstructionSpec, RelOp, RelTerm,
    StateInvariant, StateInvariantKind,
};

const CORE_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/openhl_core.so");

const CORE_ID: Pubkey = Pubkey::new_from_array([3u8; 32]);
const SYSTEM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);
const TOKEN_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// openhl-core instruction tags (programs/openhl-core/src/lib.rs).
const IX_INITIALIZE: u8 = 0;
const IX_CREATE_MARKET: u8 = 1;
const IX_CREATE_ORDER_BOOK: u8 = 7;
const IX_CREATE_ORACLE: u8 = 11;
const IX_SET_ORACLE_PRICE: u8 = 12;
const IX_PLACE_ORDER_CHECKED: u8 = 13;
const IX_REGISTER_BUILDER: u8 = 23;
const IX_PLACE_ORDER_WITH_BUILDER: u8 = 24;
const IX_CLAIM_BUILDER_FEES: u8 = 25;
const IX_CREATE_FEE_VAULT: u8 = 26;

const START_BALANCE: u64 = 1_000_000_000_000;
const PRICE: u64 = 1_000;
const MARKET_LEN: usize = 256; // openhl_state::Market::LEN

// Field offsets (crates/state/src/lib.rs; SPL Token's account layout).
const PROFILE_ACCUMULATED: usize = 56; // BuilderProfile.accumulated_fees: u64
const PROFILE_FEE_MINT: usize = 72; // BuilderProfile.fee_mint: [u8; 32]
const MARKET_QUOTE_MINT: usize = 80; // Market.quote_mint: [u8; 32]
const TOKEN_AMOUNT: usize = 64; // spl_token::state::Account.amount: u64

fn token() -> Pubkey {
    Pubkey::from_str(TOKEN_ID).unwrap()
}
fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &CORE_ID).0
}
fn market_pda(base: &Pubkey, quote: &Pubkey) -> Pubkey {
    pda(&[b"market", base.as_ref(), quote.as_ref()])
}
fn book(market: &Pubkey) -> Pubkey {
    pda(&[b"book", market.as_ref()])
}
fn oracle(market: &Pubkey) -> Pubkey {
    pda(&[b"oracle", market.as_ref()])
}
fn fee_vault(mint: &Pubkey) -> Pubkey {
    pda(&[b"fee_vault", mint.as_ref()])
}
fn fee_vault_auth(mint: &Pubkey) -> Pubkey {
    pda(&[b"fee_vault_auth", mint.as_ref()])
}
fn profile(builder: &Pubkey) -> Pubkey {
    pda(&[b"builder", builder.as_ref()])
}

/// An SPL Token mint (82 bytes).
fn mint_data(decimals: u8) -> Vec<u8> {
    let mut d = vec![0u8; 82];
    d[36..44].copy_from_slice(&(START_BALANCE * 4).to_le_bytes()); // supply
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

/// `CreateMarket` / `Initialize` payload: authority, base, quote, tick, lot.
fn market_payload(tag: u8, authority: &Pubkey, base: &Pubkey, quote: &Pubkey) -> Vec<u8> {
    let mut d = vec![tag];
    d.extend_from_slice(authority.as_ref());
    d.extend_from_slice(base.as_ref());
    d.extend_from_slice(quote.as_ref());
    d.extend_from_slice(&1u64.to_le_bytes());
    d.extend_from_slice(&1u64.to_le_bytes());
    d
}

/// `PlaceOrderWithBuilder` payload: a bid at `PRICE` for `size`.
fn order_data(size: u64) -> Vec<u8> {
    let mut d = vec![IX_PLACE_ORDER_WITH_BUILDER, 0];
    d.extend_from_slice(&PRICE.to_le_bytes());
    d.extend_from_slice(&size.to_le_bytes());
    d
}

/// Two guards that together say `profile.fee_mint == market.quote_mint`:
/// solinv compares fields of at most 16 bytes, and a mint is 32.
fn fee_mint_guards(profile_ix: usize, market_ix: usize) -> Vec<GuardCondition> {
    (0..2)
        .map(|half| GuardCondition {
            name: format!("builder credited only in its fee mint (bytes {}..{})", 16 * half, 16 * half + 16),
            lhs: GuardOperand::Field { account_index: profile_ix, field_offset: PROFILE_FEE_MINT + 16 * half, field_size: 16 },
            op: GuardOp::Eq,
            rhs: GuardOperand::Field { account_index: market_ix, field_offset: MARKET_QUOTE_MINT + 16 * half, field_size: 16 },
        })
        .collect()
}

/// `sum(profile.accumulated_fees) <= fee_vault.amount` over the builders
/// paid from that vault (B and B2, both in Q).
fn fees_covered(profile_ixs: &[usize], vault_ix: usize) -> StateInvariant {
    StateInvariant {
        name: "fee vault covers what builders are owed".into(),
        kind: StateInvariantKind::Relation {
            lhs: profile_ixs
                .iter()
                .map(|&a| RelTerm::Field { account: a, field_offset: PROFILE_ACCUMULATED, field_size: 8 })
                .collect(),
            op: RelOp::Le,
            rhs: vec![RelTerm::Field { account: vault_ix, field_offset: TOKEN_AMOUNT, field_size: 8 }],
            tolerance: 0,
        },
        accounts: vec![],
    }
}

#[derive(Clone)]
struct FeesFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    u: Arc<Keypair>,
    b: Arc<Keypair>,
    /// A second builder, registered in Q, whose profile is never U's to use.
    b2: Arc<Keypair>,
    q: Pubkey,
    j: Pubkey,
    m: Pubkey,
    m2: Pubkey,
    jm: Pubkey,
    u_q: Pubkey,
    u_j: Pubkey,
    b_q: Pubkey,
    /// B's account in J, the destination an attacker would name.
    b_j: Pubkey,
}

impl FeesFixture {
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

    /// The book and a fresh oracle at `PRICE` for `market`.
    fn book_and_oracle(&mut self, market: Pubkey) {
        let p = self.payer.pubkey();
        for (tag, acc) in [(IX_CREATE_ORDER_BOOK, book(&market)), (IX_CREATE_ORACLE, oracle(&market))] {
            let ix = self.ix(
                vec![
                    AccountMeta::new(p, true),
                    AccountMeta::new_readonly(market, false),
                    AccountMeta::new(acc, false),
                    AccountMeta::new_readonly(SYSTEM_ID, false),
                ],
                vec![tag],
            );
            self.send(ix, &[], "create book / oracle");
        }
        let mut d = vec![IX_SET_ORACLE_PRICE];
        d.extend_from_slice(&(PRICE as i64).to_le_bytes());
        d.extend_from_slice(&1u64.to_le_bytes());
        d.extend_from_slice(&0i32.to_le_bytes());
        let ix = self.ix(vec![AccountMeta::new_readonly(p, true), AccountMeta::new(oracle(&market), false)], d);
        self.send(ix, &[], "set oracle price");
    }

    fn place_metas(&self, market: &Pubkey, mint: &Pubkey, from: &Pubkey, builder: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(self.u.pubkey(), true),
            AccountMeta::new(book(market), false),
            AccountMeta::new_readonly(oracle(market), false),
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*from, false),
            AccountMeta::new(fee_vault(mint), false),
            AccountMeta::new_readonly(token(), false),
            AccountMeta::new(profile(builder), false),
        ]
    }

    fn claim_metas(&self, builder: &Pubkey, mint: &Pubkey, to: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(*builder, true),
            AccountMeta::new(profile(builder), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(fee_vault(mint), false),
            AccountMeta::new_readonly(fee_vault_auth(mint), false),
            AccountMeta::new(*to, false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    fn balance(&self, key: &Pubkey) -> u64 {
        self.ctx.token_balance(key)
    }

    fn accumulated(&self, builder: &Pubkey) -> u64 {
        let d = self.ctx.get_account(&profile(builder)).expect("profile").data;
        u64::from_le_bytes(d[PROFILE_ACCUMULATED..PROFILE_ACCUMULATED + 8].try_into().unwrap())
    }
}

#[fuzz_fixture]
impl FeesFixture {
    pub fn setup() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");

        let payer = Arc::new(Keypair::new());
        let u = Arc::new(Keypair::new());
        let b = Arc::new(Keypair::new());
        let b2 = Arc::new(Keypair::new());
        for kp in [&payer, &u, &b, &b2] {
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(SYSTEM_ID)
                .create()
                .unwrap();
        }

        let base = Keypair::new().pubkey();
        let (q, j) = (Keypair::new().pubkey(), Keypair::new().pubkey());
        let mut f = Self {
            ctx,
            payer: Arc::clone(&payer),
            u: Arc::clone(&u),
            b: Arc::clone(&b),
            b2: Arc::clone(&b2),
            q,
            j,
            m: market_pda(&base, &q),
            m2: Pubkey::new_from_array([7u8; 32]),
            jm: market_pda(&base, &j),
            u_q: Keypair::new().pubkey(),
            u_j: Keypair::new().pubkey(),
            b_q: Keypair::new().pubkey(),
            b_j: Keypair::new().pubkey(),
        };
        let p = payer.pubkey();
        let (m, m2, jm, u_q, u_j, b_q, b_j) = (f.m, f.m2, f.jm, f.u_q, f.u_j, f.b_q, f.b_j);

        f.put(q, token(), mint_data(6));
        f.put(j, token(), mint_data(6));
        f.put(u_q, token(), token_account_data(&q, &u.pubkey(), START_BALANCE));
        f.put(u_j, token(), token_account_data(&j, &u.pubkey(), START_BALANCE));
        f.put(b_q, token(), token_account_data(&q, &b.pubkey(), 0));
        f.put(b_j, token(), token_account_data(&j, &b.pubkey(), 0));

        // The fee vault for each quote mint (Chapter 13 §13.5).
        for mint in [q, j] {
            let ix = f.ix(
                vec![
                    AccountMeta::new(p, true),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new(fee_vault(&mint), false),
                    AccountMeta::new_readonly(fee_vault_auth(&mint), false),
                    AccountMeta::new_readonly(SYSTEM_ID, false),
                    AccountMeta::new_readonly(token(), false),
                ],
                vec![IX_CREATE_FEE_VAULT],
            );
            f.send(ix, &[], "create fee vault");
        }

        // Markets: (base, Q) and (base, J) at their PDAs; a second
        // (base, Q) market made by Initialize at a non-PDA address.
        for (market, quote) in [(m, q), (jm, j)] {
            let ix = f.ix(
                vec![AccountMeta::new(p, true), AccountMeta::new(market, false), AccountMeta::new_readonly(SYSTEM_ID, false)],
                market_payload(IX_CREATE_MARKET, &p, &base, &quote),
            );
            f.send(ix, &[], "create market");
        }
        f.put(m2, CORE_ID, vec![0u8; MARKET_LEN]);
        let ix = f.ix(vec![AccountMeta::new(m2, false)], market_payload(IX_INITIALIZE, &u.pubkey(), &base, &q));
        f.send(ix, &[], "initialize a market at a non-PDA address");
        for market in [m, m2, jm] {
            f.book_and_oracle(market);
        }

        // B and B2 register as builders in Q, asking for the 50% cap.
        for kp in [&b, &b2] {
            let ix = f.ix(
                vec![
                    AccountMeta::new(kp.pubkey(), true),
                    AccountMeta::new(profile(&kp.pubkey()), false),
                    AccountMeta::new_readonly(SYSTEM_ID, false),
                    AccountMeta::new_readonly(q, false),
                ],
                {
                    let mut d = vec![IX_REGISTER_BUILDER];
                    d.extend_from_slice(&5_000u64.to_le_bytes());
                    d
                },
            );
            f.send(ix, &[&**kp], "register builder");
        }
        // Setup credits no builder: a mutation that over-credits would
        // then break the relation before the fuzzer starts, and solinv
        // (rightly) reports nothing for a relation already false. The
        // fuzzer's own orders credit B and B2.
        // J's fee vault holds fees from a plain order (no builder), so a
        // claim pointed at it has something to take.
        let mut metas = f.place_metas(&jm, &j, &u_j, &b.pubkey());
        metas.pop();
        let mut d = order_data(50_000);
        d[0] = IX_PLACE_ORDER_CHECKED;
        let ix = f.ix(metas, d);
        f.send(ix, &[&*u], "U pays J fees");
        f
    }

    pub fn action_place(
        &mut self,
        #[range(1..100_000)] size: u64,
        #[range(0..2)] second_market: u8,
        #[range(0..2)] through_b2: u8,
    ) -> bool {
        let market = if second_market == 1 { self.m2 } else { self.m };
        let builder = if through_b2 == 1 { self.b2.pubkey() } else { self.b.pubkey() };
        let ix = self.ix(self.place_metas(&market, &self.q, &self.u_q, &builder), order_data(size));
        self.send_logged(ix, Arc::clone(&self.u)).0
    }

    pub fn action_claim(&mut self) -> bool {
        let ix = self.ix(self.claim_metas(&self.b.pubkey(), &self.q, &self.b_q), vec![IX_CLAIM_BUILDER_FEES]);
        self.send_logged(ix, Arc::clone(&self.b)).0
    }
}

impl HasContext for FeesFixture {
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

impl HasInstructionSet for FeesFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let (b, b2) = (self.b.pubkey(), self.b2.pubkey());
        let place = |name: &str, market: &Pubkey, mint: &Pubkey, from: &Pubkey| {
            // B2's profile rides along at the end, unread by the handler, so
            // the relation can sum both builders.
            let mut metas = self.place_metas(market, mint, from, &b);
            metas.push(AccountMeta::new_readonly(profile(&b2), false));
            let mut s = spec(name, metas, order_data(10), Arc::clone(&self.u));
            s.state_invariants = vec![fees_covered(&[8, 9], 6)];
            s.guards = fee_mint_guards(8, 3);
            s.swap_alternates[1] = vec![book(&self.m2)]; // another market's book
            s.swap_alternates[2] = vec![oracle(&self.m2)]; // another market's oracle
            s.swap_alternates[5] = vec![self.b_q]; // pay with B's tokens
            s.swap_alternates[6] = vec![fee_vault(&self.j)]; // another mint's fee vault
            // Slot 8 is not swapped: U may credit any builder it likes;
            // that is the design (§13.6).
            s
        };
        // U trades through B on the PDA market, on the second market for
        // the same pair, and on J's market (which must be refused: B is
        // paid in Q).
        let place_m = place("place_order_with_builder", &self.m, &self.q, &self.u_q);
        let place_m2 = place("place_order_with_builder_m2", &self.m2, &self.q, &self.u_q);
        let mut place_jm = place("place_order_with_builder_j", &self.jm, &self.j, &self.u_j);
        // B's Q credit against J's vault says nothing; only the guard applies.
        place_jm.state_invariants = vec![];

        let mut claim_accounts = self.claim_metas(&b, &self.q, &self.b_q);
        claim_accounts.push(AccountMeta::new_readonly(profile(&b2), false));
        let mut claim = spec("claim_builder_fees", claim_accounts, vec![IX_CLAIM_BUILDER_FEES], Arc::clone(&self.b));
        claim.state_invariants = vec![fees_covered(&[1, 7], 3)];
        claim.swap_alternates[1] = vec![profile(&b2)]; // claim B2's fees as B
        claim.swap_alternates[3] = vec![fee_vault(&self.j)]; // pay out of J's vault

        vec![place_m, place_m2, place_jm, claim]
    }
}

#[invariant_test]
fn invariant_ch13(fixture: &mut FeesFixture) {
    use solinv_core::invariants::account_swap::{self, SwapSet};
    solinv_core::invariants::signer_skip::check(fixture);
    account_swap::check(fixture);
    solinv_core::invariants::unchecked_math::check(fixture);
    solinv_core::invariants::precondition_skip::check(fixture);
    // Claim B's Q fees out of J's fee vault, and pay an order on the Q
    // market in J: in each, the mint and the accounts derived from it,
    // together.
    let (j, u_j, b_j) = (fixture.j, fixture.u_j, fixture.b_j);
    account_swap::check_sets(
        fixture,
        &[
            SwapSet {
                ix: "claim_builder_fees".into(),
                swaps: vec![(2, j), (3, fee_vault(&j)), (4, fee_vault_auth(&j)), (5, b_j)],
            },
            SwapSet { ix: "claim_builder_fees".into(), swaps: vec![(3, fee_vault(&j)), (5, b_j)] },
            SwapSet { ix: "place_order_with_builder".into(), swaps: vec![(4, j), (5, u_j), (6, fee_vault(&j))] },
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builder is credited at most half of what U paid into the fee
    /// vault, on either Q market.
    #[test]
    fn credit_never_exceeds_payment() {
        let mut f = FeesFixture::setup();
        let (u, b) = (Arc::clone(&f.u), f.b.pubkey());
        for market in [f.m, f.m2] {
            let (vault0, acc0) = (f.balance(&fee_vault(&f.q)), f.accumulated(&b));
            let ix = f.ix(f.place_metas(&market, &f.q, &f.u_q, &b), order_data(5_000));
            let (ok, out) = f.send_logged(ix, Arc::clone(&u));
            assert!(ok, "order refused: {out}");
            let paid = f.balance(&fee_vault(&f.q)) - vault0;
            let credited = f.accumulated(&b) - acc0;
            assert!(paid > 0 && 2 * credited <= paid, "paid {paid}, credited {credited}");
        }
    }

    /// B is paid in Q, so an order on J's market must not credit B.
    #[test]
    fn junk_mint_order_does_not_credit_a_q_builder() {
        let mut f = FeesFixture::setup();
        let (u, b) = (Arc::clone(&f.u), f.b.pubkey());
        let ix = f.ix(f.place_metas(&f.jm, &f.j, &f.u_j, &b), order_data(5_000));
        let (ok, out) = f.send_logged(ix, u);
        assert!(!ok, "B credited {} for a J order", f.accumulated(&b));
        assert!(out.contains("builder profile earns fees in another mint"), "refused for another reason: {out}");
    }

    /// Not a check: with B owed something, sends the swaps behind §13.6's
    /// "no" rows once each and prints how they ended
    /// (`cargo test -- --ignored --nocapture probe`).
    #[test]
    #[ignore]
    fn probe() {
        let mut f0 = FeesFixture::setup();
        let (u, b) = (Arc::clone(&f0.u), f0.b.pubkey());
        let ix = f0.ix(f0.place_metas(&f0.m, &f0.q, &f0.u_q, &b), order_data(5_000));
        assert!(f0.send_logged(ix, u).0);
        let (j, u_j) = (f0.j, f0.u_j);
        let mut place = f0.place_metas(&f0.m, &f0.q, &f0.u_q, &b);
        let mut claim = f0.claim_metas(&b, &f0.q, &f0.b_q);
        let mut variants: Vec<(&str, Vec<AccountMeta>, Vec<u8>, Arc<Keypair>)> = vec![];
        let mut v = place.clone();
        v[4].pubkey = j;
        v[5].pubkey = u_j;
        v[6].pubkey = fee_vault(&j);
        variants.push(("place: pay in J (set)", v, order_data(10), Arc::clone(&f0.u)));
        let mut v = place.clone();
        v[0].is_signer = false;
        variants.push(("place: user did not sign", v, order_data(10), Arc::clone(&f0.u)));
        place[6].pubkey = fee_vault(&j);
        variants.push(("place: J's fee vault", place, order_data(10), Arc::clone(&f0.u)));
        let mut v = claim.clone();
        v[2].pubkey = j;
        v[3].pubkey = fee_vault(&j);
        v[4].pubkey = fee_vault_auth(&j);
        v[5].pubkey = f0.b_j;
        variants.push(("claim: from J into B's J account (set)", v, vec![IX_CLAIM_BUILDER_FEES], Arc::clone(&f0.b)));
        claim[3].pubkey = fee_vault(&j);
        claim[5].pubkey = f0.b_j;
        variants.push(("claim: J's fee vault into B's J account (set)", claim, vec![IX_CLAIM_BUILDER_FEES], Arc::clone(&f0.b)));
        for (what, metas, data, signer) in variants {
            let mut f = f0.clone();
            let (ok, out) = f.send_logged(f.ix(metas, data), signer);
            let lines: Vec<&str> = out.split("\", \"").collect();
            let log = lines.iter().rev().find(|l| l.starts_with("Program log:")).copied().unwrap_or("-");
            let fail = lines.iter().rev().find(|l| l.contains(" failed: ")).map(|l| l.split(" failed: ").last().unwrap_or("")).unwrap_or("-");
            println!("PROBE {what} | owed {} | ok={ok} | {} | {}", f0.accumulated(&b), log.chars().take(100).collect::<String>(), fail.split('"').next().unwrap_or(""));
        }
    }

    /// Claiming twice pays once.
    #[test]
    fn claim_pays_once() {
        let mut f = FeesFixture::setup();
        let (u, bk) = (Arc::clone(&f.u), Arc::clone(&f.b));
        let ix = f.ix(f.place_metas(&f.m, &f.q, &f.u_q, &bk.pubkey()), order_data(5_000));
        assert!(f.send_logged(ix, u).0);
        let owed = f.accumulated(&bk.pubkey());
        for _ in 0..2 {
            let ix = f.ix(f.claim_metas(&bk.pubkey(), &f.q, &f.b_q), vec![IX_CLAIM_BUILDER_FEES]);
            assert!(f.send_logged(ix, Arc::clone(&bk)).0);
        }
        assert_eq!(f.balance(&f.b_q), owed);
    }
}
