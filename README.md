# openhl-solana

Open-source reference implementation of Hyperliquid-style perpetuals primitives — CLOB, funding, liquidation, oracle, vault, insurance fund, builder codes — built as **native Solana programs without Anchor**.

This repository is the companion code for the **SolDojo Solana Internals** track. Each chapter of the curriculum teaches one piece of the Solana runtime by building one HL primitive against it. Chapters cite this repo via `file:line`; learners clone, run, and inspect the same code the chapters reference.

## Goals

- **Curriculum first.** The code exists to teach the runtime, not to compete on perp-DEX UX. If an optimization hides the teaching point, the optimization loses.
- **No Anchor.** Every program is written with `entrypoint!`, `AccountInfo`, and manual deserialization. Anchor is the abstraction the track is meant to demystify.
- **Bytes-up.** Account layouts are `repr(C)` + `bytemuck::Pod`. The byte layout *is* the contract; the chapters read raw bytes from on-chain accounts.
- **MIT.** Fork, adapt, ship.

## Layout

```
crates/
  state/                  — shared account layouts (Pod structs) used by every
                            program + every script. The byte layouts the
                            chapters cite.
programs/
  openhl-core/            — single Solana program holding every instruction
                            (CreateMarket, OpenPosition, Match, Liquidate, …).
                            Built without Anchor; one file, one entrypoint!.
scripts/                   — per-chapter worked examples. Each `cargo run -p X`.
  allocate-market/        — ch.1: allocate a raw Market account, dump bytes
  init-market/            — ch.2: initialize the Market account
  create-market/          — ch.3: derive + create the per-market PDA
  bench/                  — ch.4: CU instrumentation harness
  stats/                  — ch.5: Stats singleton + parallelism counter-example
  create-vault/           — ch.6: allocate the per-(market, mint) vault PDA
  deposit/                — ch.6: SPL Token Transfer into the vault via CPI
  book/                   — ch.7: flat-array OrderBook (place / cancel / dump)
  match/                  — ch.8: Match instruction with pagination cap
  oracle/                 — ch.9: mock Oracle publisher (set price, dump state)
  funding/                — ch.10: drive CreateFundingState / UpdateFunding
  position/               — ch.11: OpenPosition / ClosePosition / Liquidate +
                            CreateInsuranceFund / InsuranceFundDeposit
  vault/                  — ch.12: TradingVault deposit / withdraw / NAV update
  builder/                — ch.13: RegisterBuilder / PlaceOrderWithBuilder /
                            ClaimBuilderFees / CreateFeeVault
  slab/                   — ch.15: critbit Slab (create / place / match / dump)
docs/
  chapter-01-account-model/ … chapter-15-slab/
                          — chapter drafts (EN + JA). Each directory holds
                            DRAFT.en.md and DRAFT.ja.md; the chapters cite
                            this repo via file:line so readers can navigate
                            straight to the code under discussion.
```

## Curriculum track

See [`docs/`](./docs/) for chapter drafts. Published lessons live on SolDojo.

### Phase A — Foundations

1. The Account Model from the Bytes Up
2. Writing a Native Program Without Anchor
3. PDAs from First Principles
4. Compute Budget and Heap Discipline
5. Sealevel Parallelism and Account Locks

### Phase B — HL Primitives

6. CPI Internals — Vault Deposits
7. On-Chain CLOB Data Structures
8. Matching Engine Under CU Pressure
9. Oracle Ingestion — Pyth Internals
10. Funding Rate Mechanics
11. Liquidation Engine
12. Native Vault Program
13. Builder Codes as Protocol Primitive
14. Cranks, Keepers, and Off-Chain Glue

### Appendix

15. Slab Order Book (the §8.4 critbit design, implemented)

## License

MIT — see [LICENSE](./LICENSE).
