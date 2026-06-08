# openhl-solana

**Solana Perp Sandbox engine.** Native Solana programs implementing Hyperliquid-style perpetuals primitives — CLOB, funding, liquidation, oracle, vault, insurance fund, builder codes — built without Anchor, designed to make perp DEX behavior on Solana explorable.

This is the engine behind Fabrknt's [Solana Perp Sandbox](https://fabrknt.com/solana-perp.html). Per `fabrknt/website/CONCEPT.md`, the sandbox exists so engineering teams, protocol designers, and product owners can study how a perp system behaves on Solana — and especially how it differs from an EVM perp — by running scenarios rather than reading code.

## What it is

`openhl-solana` ships:

- A single Solana program (`programs/openhl-core`) holding every instruction: `Initialize`, `CreateMarket`, `OpenPosition`, `Match`, `Liquidate`, `UpdateFunding`, oracle ingestion, vault deposit/withdraw, builder codes, insurance fund, plus a Pyth V2 checked variant. 33 instructions total.
- A shared `crates/state` defining every account layout as `repr(C) + bytemuck::Pod` Pod structs — the byte layout *is* the contract.
- 15 per-scenario scripts under `scripts/` that each `cargo run -p X` and exercise one slice of the program against a local validator, with CU cost measurement and account-state dumps.

## What it is NOT

- Not Anchor. Every program uses `entrypoint!`, `AccountInfo`, and manual deserialization. Anchor is the abstraction this engine deliberately avoids — partly so byte layouts are pinned, partly so CU costs are inspectable without an abstraction tax.
- Not production. No mainnet deployment, no fee capture, no governance, no keeper infra. The `match` instruction deliberately omits settlement transfers from the hot path; the mock oracle is open-auth; the `Stats` singleton is included as a parallelism counter-example, not a recommendation.
- Not a Solana tutorial. The depth-and-learning surface for Solana internals lives on SolDojo; this repo is product-facing. SolDojo imports some of this code as worked examples, but the curriculum framing is theirs — see [Curriculum companion](#curriculum-companion-separate-audience) for the boundary.

## Design choices worth noting

These choices are what make the engine valuable as a sandbox surface:

- **Bytes-up layouts.** Every account state can be dumped, byte-diffed, and reasoned about without a deserializer. Scenarios can verify "after this instruction, byte 0x28 holds X" with no library.
- **CU costs surfaced.** A dedicated `bench` script is a CU instrumentation harness; per-instruction CU is one of the headline numbers the sandbox tracks.
- **Deliberate counter-examples.** The flat-array `OrderBook` is kept alongside the critbit `Slab` so a scenario can compare the two on the same matching workload — a direct illustration of how Solana compute constraints shape DEX design.

## How to explore (today)

### Sandbox surface (recommended starting point)

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

Three scenarios ship today: `bring-up` (allocate → init → create-market → create-vault → deposit), `matching-cu-comparison` (flat OrderBook vs critbit Slab CU comparison), `liquidation-walkthrough` (open position → oracle drop → liquidate).

### Drive individual scripts directly

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
cargo run -p slab             # Critbit slab orderbook (compare vs flat-array `book`)
cargo run -p bench            # CU instrumentation harness
```

The full script roster is below in [Scripts as scenario surface](#scripts-as-scenario-surface).

## Sandbox elements: current state

Per `fabrknt/website/SANDBOX-PATTERN.md`, every Fabrknt sandbox must ship five elements. Here is `openhl-solana`'s current state:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 3 scenarios (`bring-up`, `matching-cu-comparison`, `liquidation-walkthrough`). More to follow as additional script combinations are scripted. |
| (2) Business-readable output | **partial** | `scenario run` spawns each step as a sub-process and inherits stdio — the underlying script output (CU prints, account dumps, raw program logs) is operator-style, wrapped only by a scenario header + per-step pass/fail verdict + final tally. A true business-readable layer (named outcomes, CU summary table, account-state diff in non-operator language) is v2 work. |
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
| Order matching (flat-array vs critbit) | `book`, `match-cli`, `slab` |
| Oracle ingestion | `oracle` |
| Funding accumulator | `funding` |
| Liquidation + insurance fund | `position` |
| Trading vault NAV | `vault` |
| Builder codes / fee claim | `builder` |

## Curriculum companion (separate audience)

This repo's code is also imported by the **SolDojo Solana Internals** track. The chapter drafts under [`docs/chapter-XX-*/`](./docs/) and any `Chapter N` references in `crates/state` / program source are SolDojo material — they are explicitly NOT part of the Fabrknt sandbox surface. A buyer arriving via Fabrknt should never need to read them; a learner arriving via SolDojo walks them chapter-by-chapter.

Directories are kept at `docs/chapter-XX-*/` only so SolDojo's existing provenance imports don't break.

## Build

```bash
cargo build-sbf -p openhl-core       # build the on-chain program for BPF
cargo build                           # build all scripts (host-side)
cargo test --workspace                # unit + integration tests
```

## Related

- [`fabrknt/website/CONCEPT.md`](../fabrknt/website/CONCEPT.md) — the Fabrknt brand and 2x2 sandbox structure.
- [`fabrknt/website/SANDBOX-PATTERN.md`](../fabrknt/website/SANDBOX-PATTERN.md) — cross-engine spec for the five sandbox elements.
- Sibling Fabrknt engines: [`rdk/openhl`](../rdk/openhl/) (EVM Perp Sandbox), [`rdk/princeps`](../rdk/princeps/) (EVM Prime Broker Sandbox).
- [`docs/`](./docs/) — chapter drafts (SolDojo material; out-of-scope for the sandbox).

## License

MIT — see [LICENSE](./LICENSE).
