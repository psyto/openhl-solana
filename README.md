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
  state/                  — shared account layouts (Pod structs)
scripts/
  allocate-market/        — Chapter 1 worked example: allocate raw Market account, dump bytes
programs/                 — (added in Chapter 2: first native program)
docs/
  chapter-01-account-model/
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

## License

MIT — see [LICENSE](./LICENSE).
