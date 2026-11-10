# openhl-solana

**Perp DEX parts for Solana programs, pick the one you need:** a critbit order book that fits in
one account, oracle checks (staleness, a price band, Pyth v1 and v2 parsing), margin and
liquidation math, funding, fees with builder codes, and vault share math. Each is a small
`no_std` crate with no Solana SDK dependency, so it runs the same inside your program, in a client
and in a plain `cargo test`. A complete native program built from them (`openhl-core`, 34
instructions, no Anchor) shows how the parts fit.

## Pick a part

| I want to… | Part | Other openhl crates it pulls in |
|---|---|---|
| match orders in one account, price-time priority, no allocation | `openhl-slab` | `openhl-state` |
| refuse stale prices, bound order prices around the mark, read Pyth v1 / `PriceUpdateV2` bytes | `openhl-oracle` | `openhl-state` |
| compute equity, initial/maintenance margin, and split a liquidation between liquidator, insurance fund and owner | `openhl-margin` | `openhl-funding` |
| clamp a keeper's funding rate and accrue the cumulative index | `openhl-funding` | — |
| charge a protocol fee and pay a builder (front end) its share | `openhl-fees` | — |
| mint and burn shares of a pooled vault, track cost basis | `openhl-vault` | — |
| cap the notional that can settle in any window of N seconds on a thin market | `openhl-guard` | — |
| use the account layouts (`repr(C)` + `bytemuck::Pod`) of every account above | `openhl-state` | — |

Outside openhl the parts use nothing except `bytemuck` (`openhl-state`, and so `openhl-slab` and
`openhl-oracle`; and `openhl-guard`). Ratios are basis points; amounts are integers in your quote units. None of the
parts reads the clock or an `AccountInfo`: your program passes the slot, the timestamp and the
account bytes in, and maps the returned error to its own `ProgramError`.

## Quickstart

Each builds only its own crate and needs no validator:

```bash
git clone https://github.com/psyto/openhl-solana && cd openhl-solana
cargo run -p openhl-slab --example slab_quickstart              # asks at two prices, crossed by one bid
cargo run -p openhl-oracle --example oracle_quickstart          # a Pyth PriceUpdateV2 gating an order
cargo run -p openhl-margin --example liquidation_quickstart     # a 10x long checked as the mark falls
cargo run -p openhl-funding --example funding_quickstart        # three keeper updates, one clamped
cargo run -p openhl-guard --example guard_quickstart            # a burst fills a 60 s budget, the next trade waits
```
```
slab is 82000 bytes, one account
ask #0: 4 @ 101
ask #1: 3 @ 100
ask #2: 2 @ 100
best ask: 100
fill 1: 3 @ 100 from #1 (taker left 3)
fill 2: 2 @ 100 from #2 (taker left 1)
fill 3: 1 @ 101 from #0 (taker left 0)
3 fills, 0 unfilled, 1 orders resting
```
```
open long 10 @ 100000, collateral 100000 (initial margin 100000)
mark  100000: equity  100000  maintenance  50000  ok
mark   96000: equity   60000  maintenance  48000  ok
mark   95000: equity   50000  maintenance  47500  ok
mark   94500: equity   45000  maintenance  47250  LIQUIDATABLE
liquidate: penalty 9450 → liquidator 4725, insurance fund 4725; owner gets 35550
```

## Use a part in your program

```toml
[dependencies]
openhl-slab = { git = "https://github.com/psyto/openhl-solana" }
openhl-state = { git = "https://github.com/psyto/openhl-solana" }
bytemuck = "1"
```
```rust
use openhl_state::{side, Slab};

// `data` is your slab account's data (Slab::LEN bytes, created and owned by your program).
fn place_and_cross(data: &mut [u8], owner: [u8; 32]) -> Result<u64, openhl_slab::SlabError> {
    let slab: &mut Slab = bytemuck::from_bytes_mut(&mut data[..Slab::LEN]);
    openhl_slab::insert_order(slab, side::ASK, 100, owner, 5)?;
    let out = openhl_slab::match_order(slab, side::BID, 100, 3, 8, |_fill| {})?;
    Ok(out.remaining)
}
```

Call `openhl_slab::init` once on a new slab account. A `Slab` is 82,000 bytes, more than the
10 KiB an account may be given from inside a program (`create_account` via CPI), so create it
from the client: a top-level `SystemProgram::CreateAccount` (space `Slab::LEN`, owner your
program) followed in the same transaction by your init instruction. The bundled `CreateSlab`
works this way (it only initializes, and refuses an account that isn't fresh, owned by the
program, exactly `Slab::LEN` bytes and signed by the slab keypair); `slab-cli --create` sends
both instructions.

## Notes per part

- **`openhl-slab`** holds at most 1,024 resting orders and 128 price levels per side
  (`SLAB_POOL_CAPACITY`, `SLAB_TREE_CAPACITY`); past that `insert_order` returns
  `PoolExhausted` / `TreeExhausted`. There is no cancel by order id yet.
- **`openhl-oracle`** `pyth_v2::parse` reads both verification levels. A Full update stores the
  level in 1 byte instead of 2 (Borsh), so its fields sit one byte earlier than the
  `PYTH_V2_OFFSET_*` constants in `openhl-state`, which are the Partial offsets. The parser does
  not verify the VAA (the Pyth receiver program did) and does not apply `expo`.
- **`openhl-margin`** and **`openhl-funding`** share the index scale `FUNDING_INDEX_SCALE`
  (1e9); funding PnL is `(index_now - snapshot) * size / 1e9`.

## The whole program: how the parts fit

`programs/openhl-core` is one native Solana program (no Anchor) that wires every part into 34
instructions: markets, an SPL Token collateral vault, a flat-array order book and the slab, a
mock oracle plus Pyth v1 and v2 readers, funding, positions and liquidation with an insurance
fund, a trading vault, and builder codes. It checks accounts, moves tokens by CPI, logs, and calls
the parts for the math. It is also the engine behind Fabrknt's
[Solana Perp Sandbox](https://fabrknt.com/solana-perp.html), where teams compare how a perp
behaves on Solana against an EVM perp by running scenarios rather than reading code.

### What the program ships

`openhl-solana` ships:

- A single Solana program (`programs/openhl-core`) holding every instruction: `Initialize`, `CreateMarket`, `OpenPosition`, `Match`, `Liquidate`, `UpdateFunding`, oracle ingestion, vault deposit/withdraw, builder codes, insurance fund, plus Pyth v1 and v2 checked variants against feeds the market pins. 35 instructions total.
- A shared `crates/state` defining every account layout as `repr(C) + bytemuck::Pod` Pod structs — the byte layout *is* the contract.
- The parts above under `crates/`, which the program calls for matching, oracle checks, margin, funding, fees and vault shares.
- 15 per-scenario scripts under `scripts/` that each `cargo run -p X` and exercise one slice of the program against a local validator, with CU cost measurement and account-state dumps.

### What it is NOT

- Not Anchor. Every program uses `entrypoint!`, `AccountInfo`, and manual deserialization. Anchor is the abstraction this engine deliberately avoids — partly so byte layouts are pinned, partly so CU costs are inspectable without an abstraction tax.
- Not production. No mainnet deployment, no fee capture, no governance, no keeper infra. The `match` instruction deliberately omits settlement transfers from the hot path; the mock oracle is open-auth; the `Stats` singleton is included as a parallelism counter-example, not a recommendation.
- No upgrade path for old state. The program isn't deployed anywhere, and account layouts and instruction account lists change without migrations — for example the per-vault trading-vault token account, `BuilderProfile.fee_mint` with `RegisterBuilder`'s new mint account, and OpenPosition's quote-mint check. Deploy a fresh program; don't upgrade one that holds state created by an older version (old trading vaults, builder profiles without a fee mint, or positions opened in another mint would be stranded or unchecked).
- Not a Solana tutorial. This README is about using the parts; the learning path is the course built from [`docs/`](./docs/) — see [Learn it](#learn-it-the-solana-internals-course).

### Design choices worth noting

These choices are what make the engine valuable as a sandbox surface:

- **Bytes-up layouts.** Every account state can be dumped, byte-diffed, and reasoned about without a deserializer. Scenarios can verify "after this instruction, byte 0x28 holds X" with no library.
- **CU costs surfaced.** A dedicated `bench` script is a CU instrumentation harness; per-instruction CU is one of the headline numbers the sandbox tracks.
- **Deliberate counter-examples.** The flat-array `OrderBook` is kept alongside the critbit `Slab` so a scenario can compare the two on the same matching workload — a direct illustration of how Solana compute constraints shape DEX design.

### How to explore (today)

#### Sandbox surface

```bash
# Discover the curated scenarios.
cargo run -p scenario -- list

# Inspect one without running.
cargo run -p scenario -- show liquidation-walkthrough

# Run the scenario: each step (cargo run -p X -- args) is spawned as
# a sub-process with stdio inherited so per-step CU prints + account
# dumps stream live. Requires solana-test-validator + a deployed
# openhl-core (see PREREQUISITES section of the output).
cargo run -p scenario -- run liquidation-walkthrough

# Pass --dry-run to print only the step list without executing.
cargo run -p scenario -- run liquidation-walkthrough --dry-run
```

Five scenarios ship today:
- **`account-layout-demo`** (walkthrough, v2 no validator) — pure-Rust inspection of every on-chain account type via `openhl-state` constants; renders a byte-precise size table + OrderBook-vs-Slab density comparison; ✓ all 5 outcomes verify.
- **`instruction-dispatch-demo`** (walkthrough, v2 no validator) — pure-Rust enumeration of all 34 openhl-core instruction tags with names + categories + descriptions; mirrors `programs/openhl-core/src/lib.rs::process_instruction`; ✓ all 8 outcomes verify.
- `bring-up` (bring-up, v1 sub-process) — allocate → init → create-market → create-vault → deposit.
- `matching-cu-comparison` (comparison, v1 sub-process) — flat OrderBook vs critbit Slab CU comparison on a live validator.
- `liquidation-walkthrough` (stress, v1 sub-process) — open position → oracle drop → liquidate.

#### Drive individual scripts directly

```bash
# Boot a local Solana validator (separate terminal).
solana-test-validator

# Build and deploy openhl-core.
cargo build-sbf -p openhl-core
solana program deploy target/deploy/openhl_core.so

# Per-feature scripts (each is one `cargo run -p X`):
cargo run -p create-market    # PDA derivation + invoke_signed
cargo run -p deposit          # CPI to SPL Token
cargo run -p match-cli        # Match instruction with CU cost surfaced
cargo run -p position         # OpenPosition / ClosePosition / Liquidate
cargo run -p oracle           # Mock oracle publisher
cargo run -p funding          # Funding rate accumulator
cargo run -p vault            # TradingVault deposit / withdraw / NAV
cargo run -p builder          # Builder codes + fee claim
cargo run -p slab-cli         # Critbit slab orderbook (compare vs flat-array `book`)
cargo run -p bench            # CU instrumentation harness
```

The full script roster is below in [Scripts as scenario surface](#scripts-as-scenario-surface).

## Sandbox elements: current state

Every Fabrknt sandbox ships five elements. Here is `openhl-solana`'s current state:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 5 scenarios (`account-layout-demo`, `instruction-dispatch-demo`, `bring-up`, `matching-cu-comparison`, `liquidation-walkthrough`). More to follow as additional script combinations are scripted. |
| (2) Business-readable output | **v2 done for v2-eligible scenarios** | Scenarios whose steps are all in-process-eligible (currently: `account-layout-demo`) take the v2 path: in-process dispatch into `run_account_layout_demo_structured`, then HEADLINE (with ✓/⚠/unverified badge per `expected_outcomes` verification), TIMELINE, DELTA, OUTCOMES, NEXT. `account-layout-demo` declares 5 outcomes that all verify ✓. Scenarios that need a real validator + deployed program (`bring-up`, `matching-cu-comparison`, `liquidation-walkthrough`) still spawn sub-processes with stdio inherited (v1); converting them requires either lib-izing the scripts or stdio-capture-and-parse — not done yet. |
| (3) Parameter dial | partial | Per-step args (e.g., `--price 80`) provide a dial. Engine-level parameters (tick size, funding interval, liquidation buffer) are still recompiled into the program. |
| (4) Scenario replay | **present** | Each scenario file is a deterministic step list — re-running yields the same sub-process invocations. Bit-identical state requires `solana-test-validator --reset` between runs. |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access) with `product=solana-perp` for waitlist enrichment. |

## Scripts as scenario surface

The scripts under `scripts/` cover the perp DEX feature areas. The sandbox scenarios compose them by theme:

| Sandbox theme | Scripts |
|---|---|
| Bring-up | `allocate-market`, `init-market`, `create-market` |
| CU cost measurement | `bench` |
| Sealevel parallelism counter-example | `stats` |
| Collateral flow | `create-vault`, `deposit` |
| Order matching (flat-array vs critbit) | `book`, `match-cli`, `slab-cli` |
| Oracle ingestion | `oracle` |
| Funding accumulator | `funding` |
| Liquidation + insurance fund | `position` |
| Trading vault NAV | `vault` |
| Builder codes / fee claim | `builder` |

## Learn it: the Solana Internals course

The fifteen chapters under [`docs/`](./docs/) walk this program from the account bytes up, each ending
with an exercise you run from a clone (several with [solinv](https://github.com/psyto/solinv) fuzz
harnesses). They are published as a free course, in English and Japanese:
**[Solana Internals — a native perp program from the bytes up](https://fabrknt.com/dojo/courses/solana-internals-en)**
([日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en)). Lessons 1–5 are Solana itself;
6–15 build the trading primitives.

| # | Chapter | Lesson | Source |
|---|---|---|---|
| 1 | The Account Model from the Bytes Up | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch01-account-model-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch01-account-model-en) | [`DRAFT.en.md`](./docs/chapter-01-account-model/DRAFT.en.md) · [`ja`](./docs/chapter-01-account-model/DRAFT.ja.md) |
| 2 | Writing a Native Program Without Anchor | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch02-native-program-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch02-native-program-en) | [`DRAFT.en.md`](./docs/chapter-02-native-program/DRAFT.en.md) · [`ja`](./docs/chapter-02-native-program/DRAFT.ja.md) |
| 3 | PDAs from First Principles | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch03-pdas-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch03-pdas-en) | [`DRAFT.en.md`](./docs/chapter-03-pdas/DRAFT.en.md) · [`ja`](./docs/chapter-03-pdas/DRAFT.ja.md) |
| 4 | Compute Budget and Heap Discipline | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch04-compute-budget-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch04-compute-budget-en) | [`DRAFT.en.md`](./docs/chapter-04-compute-budget/DRAFT.en.md) · [`ja`](./docs/chapter-04-compute-budget/DRAFT.ja.md) |
| 5 | Sealevel Parallelism and Account Locks | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch05-sealevel-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch05-sealevel-en) | [`DRAFT.en.md`](./docs/chapter-05-sealevel/DRAFT.en.md) · [`ja`](./docs/chapter-05-sealevel/DRAFT.ja.md) |
| 6 | CPI Internals via Vault Deposits | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch06-cpi-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch06-cpi-en) | [`DRAFT.en.md`](./docs/chapter-06-cpi/DRAFT.en.md) · [`ja`](./docs/chapter-06-cpi/DRAFT.ja.md) |
| 7 | On-Chain CLOB Data Structures | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch07-clob-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch07-clob-en) | [`DRAFT.en.md`](./docs/chapter-07-clob/DRAFT.en.md) · [`ja`](./docs/chapter-07-clob/DRAFT.ja.md) |
| 8 | Matching Engine Under CU Pressure | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch08-matching-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch08-matching-en) | [`DRAFT.en.md`](./docs/chapter-08-matching/DRAFT.en.md) · [`ja`](./docs/chapter-08-matching/DRAFT.ja.md) |
| 9 | Oracle Ingestion: Pyth Internals | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch09-oracle-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch09-oracle-en) | [`DRAFT.en.md`](./docs/chapter-09-oracle/DRAFT.en.md) · [`ja`](./docs/chapter-09-oracle/DRAFT.ja.md) |
| 10 | Funding Rate Mechanics | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch10-funding-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch10-funding-en) | [`DRAFT.en.md`](./docs/chapter-10-funding/DRAFT.en.md) · [`ja`](./docs/chapter-10-funding/DRAFT.ja.md) |
| 11 | Position Lifecycle and Liquidation Engine | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch11-liquidation-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch11-liquidation-en) | [`DRAFT.en.md`](./docs/chapter-11-liquidation/DRAFT.en.md) · [`ja`](./docs/chapter-11-liquidation/DRAFT.ja.md) |
| 12 | Native Vault Program (Pooled Trading) | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch12-vault-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch12-vault-en) | [`DRAFT.en.md`](./docs/chapter-12-vault/DRAFT.en.md) · [`ja`](./docs/chapter-12-vault/DRAFT.ja.md) |
| 13 | Builder Codes as a Protocol Primitive | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch13-builder-codes-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch13-builder-codes-en) | [`DRAFT.en.md`](./docs/chapter-13-builder-codes/DRAFT.en.md) · [`ja`](./docs/chapter-13-builder-codes/DRAFT.ja.md) |
| 14 | Cranks, Keepers, and Off-Chain Glue | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch14-cranks-keepers-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch14-cranks-keepers-en) | [`DRAFT.en.md`](./docs/chapter-14-cranks-keepers/DRAFT.en.md) · [`ja`](./docs/chapter-14-cranks-keepers/DRAFT.ja.md) |
| 15 | Slab Order Book | [read](https://fabrknt.com/dojo/courses/solana-internals-en/lessons/solana-internals-ch15-slab-en) · [日本語](https://fabrknt.com/ja/dojo/courses/solana-internals-en/lessons/solana-internals-ch15-slab-en) | [`DRAFT.en.md`](./docs/chapter-15-slab/DRAFT.en.md) · [`ja`](./docs/chapter-15-slab/DRAFT.ja.md) |

The course is generated from the files here at a pinned commit; fix a chapter in this repo and the
lesson follows on the next import. The chapters quote the code as it was when it all lived in
`programs/openhl-core/src/lib.rs`; the slab, oracle, margin, funding, fee and vault logic now lives
in the crates above, and the chapters' `lib.rs:NNN` line references are not kept in sync.

## Build

```bash
cargo build-sbf -p openhl-core       # build the on-chain program for BPF
cargo build                           # build all scripts (host-side)
cargo test --workspace                # the parts' unit tests + scenario tests
```

## Related

- [Solana Internals](https://fabrknt.com/dojo/courses/solana-internals-en) — the chapters in [`docs/`](./docs/) as a course (EN / JA).
- [Reactor](https://fabrknt.com/reactor) — perp-engine sandboxes, including the EVM one built on
  [`rdk/openhl`](https://github.com/psyto/rdk/tree/main/openhl).
- Sibling engines in [`psyto/rdk`](https://github.com/psyto/rdk): [`openhl`](https://github.com/psyto/rdk/tree/main/openhl) (EVM perp), [`princeps`](https://github.com/psyto/rdk/tree/main/princeps) (EVM prime broker).

## License

Apache-2.0 — see [LICENSE](./LICENSE).
