//! solinv harness for the Chapter 12 trading vault — `VaultDeposit` and
//! `VaultWithdraw`.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). One market holds three
//! trading vaults, each with its own token account: K's, where depositors A
//! and B each hold shares, A's own, and C's in a junk mint. The specs send
//! A's deposit into K's vault and A's withdrawal from it, and solinv swaps in
//! B's share account, A's share account in the other vault, the other vault,
//! the other vault's token account, or a token account B holds. A two-slot
//! set points C's junk withdrawal at K's token account. K's update of its
//! vault's NAV is sent with the signer cleared and with A's vault in its
//! place, and B's first deposit into A's vault with another account named
//! as the system program. Any swap that succeeds and changes state is a
//! finding.

use crucible_fuzzer::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;
use std::sync::Arc;

use solinv_core::invariants::account_swap::SwapSet;
use solinv_fuzz::{HasContext, HasInstructionSet, InstructionSpec};

const CORE_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/openhl_core.so");

const CORE_ID: Pubkey = Pubkey::new_from_array([3u8; 32]);
const SYSTEM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);
const TOKEN_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// openhl-core instruction tags (programs/openhl-core/src/lib.rs).
const IX_CREATE_TRADING_VAULT: u8 = 19;
const IX_VAULT_DEPOSIT: u8 = 20;
const IX_VAULT_WITHDRAW: u8 = 21;
const IX_VAULT_UPDATE_NAV: u8 = 22;

const START_BALANCE: u64 = 1_000_000;
const START_DEPOSIT: u64 = 100_000;

fn token() -> Pubkey {
    Pubkey::from_str(TOKEN_ID).unwrap()
}
fn trading_vault(market: &Pubkey, manager: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"trading_vault", market.as_ref(), manager.as_ref()], &CORE_ID).0
}
fn share(vault: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault_share", vault.as_ref(), owner.as_ref()], &CORE_ID).0
}
/// A trading vault's own token account.
fn vault_token(vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"tvault_token", vault.as_ref()], &CORE_ID).0
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

fn amount_data(tag: u8, amount: u64) -> Vec<u8> {
    let mut d = vec![tag];
    d.extend_from_slice(&amount.to_le_bytes());
    d
}

#[derive(Clone)]
struct VaultFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    a: Arc<Keypair>,
    b: Arc<Keypair>,
    /// The manager of K's vault.
    k: Arc<Keypair>,
    market: Pubkey,
    mint: Pubkey,
    /// K's vault, where A and B both hold shares.
    vault: Pubkey,
    /// A's own vault in the same market.
    a_vault: Pubkey,
    a_ata: Pubkey,
    b_ata: Pubkey,
    /// C's vault in a junk mint, in the same market.
    c: Arc<Keypair>,
    junk: Pubkey,
    c_vault: Pubkey,
    c_junk: Pubkey,
    c_real: Pubkey,
    /// An account that is no program, named where a program id goes.
    not_a_program: Pubkey,
}

impl VaultFixture {
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

    fn deposit_metas(&self, vault: &Pubkey, who: &Pubkey, ata: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*who, true),
            AccountMeta::new(*vault, false),
            AccountMeta::new(share(vault, who), false),
            AccountMeta::new_readonly(SYSTEM_ID, false),
            AccountMeta::new_readonly(self.market, false),
            AccountMeta::new_readonly(self.mint, false),
            AccountMeta::new(*ata, false),
            AccountMeta::new(vault_token(vault), false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    fn withdraw_metas(&self, vault: &Pubkey, who: &Pubkey, ata: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(*who, true),
            AccountMeta::new(*vault, false),
            AccountMeta::new(share(vault, who), false),
            AccountMeta::new_readonly(self.market, false),
            AccountMeta::new_readonly(self.mint, false),
            AccountMeta::new(*ata, false),
            AccountMeta::new(vault_token(vault), false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    /// `metas` with the mint at `mint_ix` switched to the junk mint.
    fn junk_metas(&self, mut metas: Vec<AccountMeta>, mint_ix: usize) -> Vec<AccountMeta> {
        metas[mint_ix] = AccountMeta::new_readonly(self.junk, false);
        metas
    }

    fn update_nav_metas(&self, vault: &Pubkey, manager: &Pubkey) -> Vec<AccountMeta> {
        vec![AccountMeta::new_readonly(*manager, true), AccountMeta::new(*vault, false)]
    }

    fn ix(&self, accounts: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
        Instruction { program_id: CORE_ID, accounts, data }
    }

    fn create_vault(&mut self, manager: Arc<Keypair>, mint: Pubkey, vault: Pubkey, what: &str) {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.market, false),
                AccountMeta::new_readonly(manager.pubkey(), true),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
                AccountMeta::new(vault_token(&vault), false),
                AccountMeta::new_readonly(token(), false),
            ],
            data: vec![IX_CREATE_TRADING_VAULT],
        };
        self.send(ix, &[&*manager], what);
    }

    fn deposit(&mut self, vault: Pubkey, who: Arc<Keypair>, ata: Pubkey, what: &str) {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.deposit_metas(&vault, &who.pubkey(), &ata),
            data: amount_data(IX_VAULT_DEPOSIT, START_DEPOSIT),
        };
        self.send(ix, &[&*who], what);
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

    fn send_ok(&mut self, ix: Instruction, signer: Arc<Keypair>) -> bool {
        let payer = Arc::clone(&self.payer);
        self.ctx
            .raw_call(ix)
            .fee_payer(&*payer)
            .signers(&[&*payer, &*signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }
}

#[fuzz_fixture]
impl VaultFixture {
    pub fn setup() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");

        let payer = Arc::new(Keypair::new());
        let a = Arc::new(Keypair::new());
        let b = Arc::new(Keypair::new());
        let k = Arc::new(Keypair::new());
        let c = Arc::new(Keypair::new());
        for kp in [&payer, &a, &b, &k, &c] {
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(SYSTEM_ID)
                .create()
                .unwrap();
        }

        let market = Keypair::new().pubkey();
        let mint = Keypair::new().pubkey();
        let mut f = Self {
            ctx,
            payer: Arc::clone(&payer),
            a: Arc::clone(&a),
            b: Arc::clone(&b),
            k: Arc::clone(&k),
            market,
            mint,
            vault: trading_vault(&market, &k.pubkey()),
            a_vault: trading_vault(&market, &a.pubkey()),
            a_ata: Keypair::new().pubkey(),
            b_ata: Keypair::new().pubkey(),
            c: Arc::clone(&c),
            junk: Keypair::new().pubkey(),
            c_vault: trading_vault(&market, &c.pubkey()),
            c_junk: Keypair::new().pubkey(),
            c_real: Keypair::new().pubkey(),
            not_a_program: Keypair::new().pubkey(),
        };

        // The vault handlers read only the market's key and owner, so the
        // market account is written directly instead of via CreateMarket.
        f.put(market, CORE_ID, vec![0u8; 8]);
        f.put(mint, token(), mint_data(6));
        let (aa, ba) = (f.a_ata, f.b_ata);
        f.put(aa, token(), token_account_data(&mint, &a.pubkey(), START_BALANCE));
        f.put(ba, token(), token_account_data(&mint, &b.pubkey(), START_BALANCE));

        // K's vault and A's vault, both in this market.
        let (v, av) = (f.vault, f.a_vault);
        f.create_vault(Arc::clone(&k), mint, v, "create K's vault");
        f.create_vault(Arc::clone(&a), mint, av, "create A's vault");

        // A and B hold shares in K's vault; A also in its own.
        f.deposit(v, Arc::clone(&a), aa, "deposit A into K's vault");
        f.deposit(v, Arc::clone(&b), ba, "deposit B into K's vault");
        f.deposit(av, Arc::clone(&a), aa, "deposit A into A's vault");

        // C: a junk mint, a vault in it (anyone may open one), and a deposit.
        let (junk, cv, cj, cr) = (f.junk, f.c_vault, f.c_junk, f.c_real);
        f.put(junk, token(), mint_data(6));
        f.put(cj, token(), token_account_data(&junk, &c.pubkey(), START_BALANCE));
        f.put(cr, token(), token_account_data(&mint, &c.pubkey(), 0));
        let np = f.not_a_program;
        f.put(np, SYSTEM_ID, vec![]);
        f.create_vault(Arc::clone(&c), junk, cv, "C opens a junk-mint vault");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.junk_metas(f.deposit_metas(&cv, &c.pubkey(), &cj), 5),
            data: amount_data(IX_VAULT_DEPOSIT, 3 * START_DEPOSIT),
        };
        f.send(ix, &[&*c], "C deposits junk");
        f
    }

    pub fn action_deposit(&mut self, #[range(1..10_000)] amount: u64) -> bool {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.deposit_metas(&self.vault, &self.a.pubkey(), &self.a_ata),
            data: amount_data(IX_VAULT_DEPOSIT, amount),
        };
        self.send_ok(ix, Arc::clone(&self.a))
    }

    pub fn action_withdraw(&mut self, #[range(1..10_000)] shares: u64) -> bool {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.withdraw_metas(&self.vault, &self.a.pubkey(), &self.a_ata),
            data: amount_data(IX_VAULT_WITHDRAW, shares),
        };
        self.send_ok(ix, Arc::clone(&self.a))
    }
}

impl HasContext for VaultFixture {
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

impl HasInstructionSet for VaultFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let a = self.a.pubkey();
        let b = self.b.pubkey();
        let t = Some(token());
        let c = Some(CORE_ID);
        let deposit = InstructionSpec {
            program_id: CORE_ID,
            name: "vault_deposit".into(),
            accounts: self.deposit_metas(&self.vault, &a, &self.a_ata),
            signer_indices: vec![0],
            optional_signer_indices: vec![],
            expected_owners: vec![None, c, c, None, None, None, t, t, None],
            expected_discriminators: vec![None; 9],
            expected_pda_seeds: vec![None; 9],
            creates_indices: vec![],
            swap_alternates: vec![
                vec![],
                vec![self.a_vault],              // A's vault, with the share of K's
                vec![share(&self.vault, &b)],   // credit B's shares
                vec![],
                vec![],
                vec![],
                vec![],
                vec![
                    vault_token(&self.a_vault),  // A's vault's token account
                    self.b_ata,                  // "vault" account B holds
                ],
                vec![self.not_a_program],        // as the token program
            ],
            data_sample: amount_data(IX_VAULT_DEPOSIT, 10),
            signers: vec![Arc::clone(&self.a)],
            state_invariants: vec![],
            cu_budget: None,
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        };
        let withdraw = InstructionSpec {
            program_id: CORE_ID,
            name: "vault_withdraw".into(),
            accounts: self.withdraw_metas(&self.vault, &a, &self.a_ata),
            signer_indices: vec![0],
            optional_signer_indices: vec![],
            expected_owners: vec![None, c, c, None, None, t, t, None],
            expected_discriminators: vec![None; 8],
            expected_pda_seeds: vec![None; 8],
            creates_indices: vec![],
            swap_alternates: vec![
                vec![],
                vec![self.a_vault],                  // burn against A's vault instead
                vec![
                    share(&self.vault, &b),          // burn B's shares
                    share(&self.a_vault, &a),        // A's shares in the other vault
                ],
                vec![],
                vec![],
                // The destination is not swapped: paying one's own
                // withdrawal to someone else's account harms no one.
                vec![],
                vec![
                    vault_token(&self.a_vault),      // A's vault's token account
                    self.b_ata,                      // "vault" account B holds
                ],
                vec![self.not_a_program],            // as the token program
            ],
            data_sample: amount_data(IX_VAULT_WITHDRAW, 10),
            signers: vec![Arc::clone(&self.a)],
            state_invariants: vec![],
            cu_budget: None,
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        };
        // C's own withdrawal from its junk vault; check_sets below swaps
        // its source and destination for the real mint's together.
        let junk_withdraw = InstructionSpec {
            name: "vault_withdraw_junk".into(),
            accounts: self.junk_metas(self.withdraw_metas(&self.c_vault, &self.c.pubkey(), &self.c_junk), 4),
            swap_alternates: vec![vec![]; 8],
            signers: vec![Arc::clone(&self.c)],
            ..withdraw.clone()
        };
        // B's first deposit into A's vault: the share account does not exist
        // yet, so this is the call that uses the system program.
        let mut first_deposit_swaps = vec![vec![]; 9];
        first_deposit_swaps[3] = vec![self.not_a_program];
        let first_deposit = InstructionSpec {
            name: "vault_deposit_first".into(),
            accounts: self.deposit_metas(&self.a_vault, &b, &self.b_ata),
            swap_alternates: first_deposit_swaps,
            signers: vec![Arc::clone(&self.b)],
            ..deposit.clone()
        };
        // K, the manager of K's vault, reports its NAV.
        let update_nav = InstructionSpec {
            program_id: CORE_ID,
            name: "vault_update_nav".into(),
            accounts: self.update_nav_metas(&self.vault, &self.k.pubkey()),
            signer_indices: vec![0],
            optional_signer_indices: vec![],
            expected_owners: vec![None, c],
            expected_discriminators: vec![None; 2],
            expected_pda_seeds: vec![None; 2],
            creates_indices: vec![],
            swap_alternates: vec![
                vec![],
                vec![self.a_vault], // A's vault, which K does not manage
            ],
            data_sample: amount_data(IX_VAULT_UPDATE_NAV, 3 * START_DEPOSIT),
            signers: vec![Arc::clone(&self.k)],
            state_invariants: vec![],
            cu_budget: None,
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        };
        vec![deposit, first_deposit, withdraw, junk_withdraw, update_nav]
    }
}

#[invariant_test]
fn invariant_vault(fixture: &mut VaultFixture) {
    solinv_core::invariants::signer_skip::check(fixture);
    solinv_core::invariants::owner_skip::check(fixture);
    solinv_core::invariants::account_swap::check(fixture);
    // Withdraw junk, paid from K's token account (real mint) to C's own
    // real-mint account: two slots that only work together.
    let real_token = vault_token(&fixture.vault);
    let sets = [SwapSet { ix: "vault_withdraw_junk".into(), swaps: vec![(5, fixture.c_real), (6, real_token)] }];
    // check_sets skips a set whose spec or accounts are missing without a
    // word, so make sure this one is really there.
    assert!(fixture.instructions().iter().any(|s| s.name == sets[0].ix), "no spec named {}", sets[0].ix);
    assert!(fixture.ctx.account_exists(&fixture.c_real) && fixture.ctx.account_exists(&real_token), "set's accounts missing");
    solinv_core::invariants::account_swap::check_sets(fixture, &sets);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manages its own vault in the same market, so it can mint itself
    /// shares there for almost nothing (report NAV 1, then deposit 2), and
    /// then burn them against K's vault, at K's price, out of K's token
    /// account.
    #[test]
    fn withdraw_refuses_a_share_from_another_vault() {
        let mut f = VaultFixture::setup();
        let a = Arc::clone(&f.a);
        let (vault, a_vault, a_ata) = (f.vault, f.a_vault, f.a_ata);

        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(a.pubkey(), true), AccountMeta::new(a_vault, false)],
            data: amount_data(IX_VAULT_UPDATE_NAV, 1),
        };
        f.send(ix, &[&*a], "A reports NAV 1 for its own vault");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.deposit_metas(&a_vault, &a.pubkey(), &a_ata),
            data: amount_data(IX_VAULT_DEPOSIT, 2),
        };
        f.send(ix, &[&*a], "A deposits 2 into its own vault");

        let before = f.ctx.token_balance(&a_ata);
        // K's vault, but the share account of A's vault.
        let mut accounts = f.withdraw_metas(&vault, &a.pubkey(), &a_ata);
        accounts[2] = AccountMeta::new(share(&a_vault, &a.pubkey()), false);
        let ix = Instruction { program_id: CORE_ID, accounts, data: amount_data(IX_VAULT_WITHDRAW, 2 * START_DEPOSIT) };
        let (ok, out) = f.send_logged(ix, Arc::clone(&a));
        let taken = f.ctx.token_balance(&a_ata) - before;
        let left = f.ctx.token_balance(&vault_token(&vault));
        assert!(!ok, "withdraw from K's vault with A's other share succeeded: A took {taken}, {left} left in K's token account");
        assert!(out.contains("share belongs to another vault"), "refused for another reason: {out}");
    }

    /// C opens a vault in a junk mint, deposits junk, and withdraws naming
    /// K's token account (real mint) as the source and its own real-mint
    /// account as the destination. The program refuses it; without that
    /// check SPL Token would too, since C's vault is not the authority of
    /// K's account. The amount stays under K's balance so the transfer
    /// would reach SPL Token's authority check, not stop at the balance.
    /// Two accounts change together; the fuzzer reaches it only through the
    /// `check_sets` pair in `invariant_vault`.
    #[test]
    fn withdraw_refuses_the_token_account_of_another_vault() {
        let mut f = VaultFixture::setup();
        let (vault, c, c_vault, c_real) = (f.vault, Arc::clone(&f.c), f.c_vault, f.c_real);
        let mut accounts = f.withdraw_metas(&c_vault, &c.pubkey(), &c_real);
        accounts[4] = AccountMeta::new_readonly(f.junk, false);
        accounts[6] = AccountMeta::new(vault_token(&vault), false);
        let ix = Instruction { program_id: CORE_ID, accounts, data: amount_data(IX_VAULT_WITHDRAW, START_DEPOSIT) };
        let (ok, out) = f.send_logged(ix, Arc::clone(&c));
        let taken = f.ctx.token_balance(&c_real);
        let left = f.ctx.token_balance(&vault_token(&vault));
        assert!(!ok, "C swapped junk for {taken} real tokens; {left} left in K's token account");
        assert!(out.contains("vault_token is not this vault's token account"), "refused for another reason: {out}");
    }

    /// A vault's manager sets its NAV, so a manager can over-report it. What
    /// a vault pays out comes from its own token account, so the most the
    /// manager takes is what that vault holds; K's vault is untouched.
    #[test]
    fn manager_cannot_withdraw_more_than_its_vault_holds() {
        let mut f = VaultFixture::setup();
        let (a, vault) = (Arc::clone(&f.a), f.vault);
        let (a_vault, a_ata) = (f.a_vault, f.a_ata);

        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(a.pubkey(), true), AccountMeta::new(a_vault, false)],
            data: amount_data(IX_VAULT_UPDATE_NAV, 3 * START_DEPOSIT),
        };
        f.send(ix, &[&*a], "A reports its own vault's NAV at 3x what it holds");
        let before = f.ctx.token_balance(&a_ata);
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.withdraw_metas(&a_vault, &a.pubkey(), &a_ata),
            data: amount_data(IX_VAULT_WITHDRAW, START_DEPOSIT),
        };
        let (ok, out) = f.send_logged(ix, Arc::clone(&a));
        let taken = f.ctx.token_balance(&a_ata) - before;
        let k_left = f.ctx.token_balance(&vault_token(&vault));
        assert!(!ok, "A put {START_DEPOSIT} into its own vault and took {taken} out");
        // SPL Token refuses: the vault's own account holds only 100,000.
        assert!(out.contains("insufficient funds"), "refused for another reason: {out}");
        assert_eq!(k_left, 2 * START_DEPOSIT, "K's token account changed");
    }

    /// A holds 100,000 of the 200,000 shares in K's vault and asks to burn
    /// all 200,000. The program refuses. Without that check `share.shares -=`
    /// wraps (the program is built without overflow checks), and A takes
    /// B's 100,000 too. The fuzzer's amounts stay under 10,000, so only this
    /// test states the rule.
    #[test]
    fn withdraw_refuses_more_shares_than_held() {
        let mut f = VaultFixture::setup();
        let (a, vault, a_ata) = (Arc::clone(&f.a), f.vault, f.a_ata);
        let before = f.ctx.token_balance(&a_ata);
        let ix = f.ix(f.withdraw_metas(&vault, &a.pubkey(), &a_ata), amount_data(IX_VAULT_WITHDRAW, 2 * START_DEPOSIT));
        let (ok, out) = f.send_logged(ix, Arc::clone(&a));
        let taken = f.ctx.token_balance(&a_ata) - before;
        let left = f.ctx.token_balance(&vault_token(&vault));
        assert!(!ok, "A held {START_DEPOSIT} shares and took {taken}; {left} left in K's token account");
        assert!(out.contains("insufficient shares"), "refused for another reason: {out}");
    }

    /// Not a check: sends each spec once with its signer flag cleared, with
    /// each single-slot swap, with each account the program expects to be
    /// owned by a given program handed to another owner (same address, same
    /// bytes, so no seed check stands in the way), and with an amount of 0,
    /// and prints how it ended. Used for the "refused" rows in §12.7
    /// (`cargo test --release --features invariant_vault -- --ignored --nocapture probe`).
    #[test]
    #[ignore]
    fn probe() {
        let f0 = VaultFixture::setup();
        for spec in f0.instructions() {
            let mut variants: Vec<(String, Vec<AccountMeta>, Vec<u8>, Option<usize>)> = vec![];
            let mut m = spec.accounts.clone();
            m[0].is_signer = false;
            variants.push(("signer off".into(), m, spec.data_sample.clone(), None));
            for (i, alts) in spec.swap_alternates.iter().enumerate() {
                for (j, alt) in alts.iter().enumerate() {
                    let mut m = spec.accounts.clone();
                    m[i].pubkey = *alt;
                    let what = if j == 0 { format!("slot {i} swapped") } else { format!("slot {i} swapped, alternate {}", j + 1) };
                    variants.push((what, m, spec.data_sample.clone(), None));
                }
            }
            for (i, owner) in spec.expected_owners.iter().enumerate() {
                if owner.is_some() && f0.ctx.account_exists(&spec.accounts[i].pubkey) {
                    variants.push((format!("slot {i} owner changed"), spec.accounts.clone(), spec.data_sample.clone(), Some(i)));
                }
            }
            variants.push(("amount 0".into(), spec.accounts.clone(), vec![spec.data_sample[0], 0, 0, 0, 0, 0, 0, 0, 0], None));
            for (what, metas, data, reown) in variants {
                let mut f = f0.clone();
                if let Some(i) = reown {
                    let key = spec.accounts[i].pubkey;
                    let mut acct = f.ctx.get_account(&key).expect("account to re-own");
                    acct.owner = Pubkey::new_from_array([9u8; 32]);
                    f.ctx.write_account(&key, acct).expect("write account");
                }
                let signer = Arc::clone(&spec.signers[0]);
                let (ok, out) = f.send_logged(f.ix(metas, data), signer);
                let lines: Vec<&str> = out.split("\", \"").collect();
                let log = lines.iter().rev().find(|l| l.starts_with("Program log:")).copied().unwrap_or("-");
                let fail = lines.iter().rev().find(|l| l.contains(" failed: ")).map(|l| l.split(" failed: ").last().unwrap_or("")).unwrap_or("-");
                println!("PROBE {} | {what} | ok={ok} | {} | {}", spec.name, log.chars().take(100).collect::<String>(), fail.split('"').next().unwrap_or(""));
            }
        }
    }
}
