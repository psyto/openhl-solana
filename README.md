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
- Not a Solana tutorial in itself. The depth-and-learning surface for Solana internals lives on SolDojo (see [Curriculum companion](#curriculum-companion) below). This repo is the product-facing sandbox engine.

## Design choices worth noting

These choices are what make the engine valuable as a sandbox surface:

- **Bytes-up layouts.** Every account state can be dumped, byte-diffed, and reasoned about without a deserializer. Scenarios can verify "after this instruction, byte 0x28 holds X" with no library.
- **CU costs surfaced.** The `bench` script (Chapter 4 in the curriculum) is a CU instrumentation harness; per-instruction CU is one of the headline numbers the sandbox tracks.
- **Deliberate counter-examples.** The flat-array `OrderBook` (Chapter 7/8) is kept alongside the critbit `Slab` (Chapter 15) so a scenario can compare the two on the same matching workload — a direct illustration of how Solana compute constraints shape DEX design.

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
| (2) Business-readable output | **v1 present** | `scenario run` spawns each step as a sub-process with stdio inherited, wrapped with a headline header, per-step separators, and a final pass/fail verdict. Step output (each script's CU prints + account dumps) streams live. v2 will tee stdio so declared expect-substrings can be verified. |
| (3) Parameter dial | partial | Per-step args (e.g., `--price 80`) provide a dial. Engine-level parameters (tick size, funding interval, liquidation buffer) are still recompiled into the program. |
| (4) Scenario replay | **present** | Each scenario file is a deterministic step list — re-running yields the same sub-process invocations. Bit-identical state requires `solana-test-validator --reset` between runs. |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access) with `product=solana-perp` for waitlist enrichment. |

## Scripts as scenario surface

The 15 scripts under `scripts/` map roughly to perp DEX feature areas. Today they are ordered by curriculum chapter; the sandbox grouping (planned) collapses them into themed scenarios:

| Script | Curriculum ch. | Sandbox theme (planned) |
|---|---|---|
| `allocate-market`, `init-market`, `create-market` | ch.1–3 | Bring-up |
| `bench` | ch.4 | CU cost comparison |
| `stats` | ch.5 | Sealevel parallelism counter-example |
| `create-vault`, `deposit` | ch.6 | Collateral flow |
| `book`, `match`, `slab` | ch.7, 8, 15 | Order matching (flat-array vs critbit) |
| `oracle` | ch.9 | Oracle ingestion |
| `funding` | ch.10 | Funding accumulator |
| `position` | ch.11 | Liquidation + insurance fund |
| `vault` | ch.12 | Trading vault NAV |
| `builder` | ch.13 | Builder codes / fee claim |

## Curriculum companion

This repo doubles as the code companion for the **SolDojo Solana Internals** track. Chapter drafts live under [`docs/chapter-XX-*/`](./docs/) (DRAFT.en.md / DRAFT.ja.md), with each chapter teaching one piece of the Solana runtime by building one primitive against it. Published lessons live on SolDojo and import the drafts from this repo via provenance notes.

**Important:** the curriculum framing is the SolDojo audience, not the Fabrknt sandbox audience. A buyer arriving via Fabrknt who wants to understand the sandbox should not need to read 15 chapters first. A learner arriving via SolDojo who wants to understand the runtime walks chapter-by-chapter. Both paths use the same code; the two READMEs (this one + SolDojo's) frame it for the right audience.

Chapter directories stay at `docs/chapter-XX-*/` (not moved) to preserve the file-path provenance SolDojo's imports already cite.

## Build

```bash
cargo build-sbf -p openhl-core       # build the on-chain program for BPF
cargo build                           # build all scripts (host-side)
cargo test --workspace                # unit + integration tests
```

## Related

- [`fabrknt/website/CONCEPT.md`](../fabrknt/website/CONCEPT.md) — the Fabrknt brand and 2x2 sandbox structure.
- [`fabrknt/website/SANDBOX-PATTERN.md`](../fabrknt/website/SANDBOX-PATTERN.md) — cross-engine spec for the five sandbox elements.
- [`docs/`](./docs/) — chapter drafts (SolDojo curriculum source).
- Sibling Fabrknt engines: [`rdk/openhl`](../rdk/openhl/) (EVM Perp Sandbox), [`rdk/princeps`](../rdk/princeps/) (EVM Prime Broker Sandbox).

## License

MIT — see [LICENSE](./LICENSE).
