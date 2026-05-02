# Telos

> τέλος — *the intended end*. You declare the outcome; the protocol resolves the path.

**Telos** is an intent-based atomic aggregator connecting [Tempo](https://tempo.network) (settlement-specialized Reth chain) and [Hyperliquid](https://hyperliquid.xyz) (CLOB-native L1). A merchant or user submits a single intent — *"settle this payment in USD-equivalent value"* — and Telos routes the execution across both venues, hedging price risk in the settlement window.

This is a **research project**, not yet a product. It is also the practical companion to [psyto/rethlab](https://github.com/psyto/rethlab): RethLab teaches the Rust EVM stack from source; Telos puts it into production.

---

## The novel primitive: spot-settle + perp-hedge

Existing aggregators (1inch, Jupiter, CowSwap, Paraswap) route swaps between liquidity sources. None of them protect a merchant from price drift between *user pays* and *merchant settles*.

Telos does. The settlement gateway:

1. Receives a `PaymentIntent` on Tempo (via the [MPP](https://tempo.network/docs/mpp) standard — Multi-Party Payments).
2. Hits the Hyperliquid CLOB for spot execution at the best available price.
3. Opens a perp hedge on Hyperliquid sized to the settlement window.
4. Closes the hedge atomically on outbound settlement to the merchant.

The perp leg is the same delta-neutral logic Fabrknt's Kodiak vault uses for yield, repurposed here as **merchant FX protection**.

---

## Architecture

```
┌─────────────┐      PaymentIntent      ┌──────────────┐
│   Payer     │  ────────────────────▶  │  Tempo (L1)  │
└─────────────┘                          │  Reth-based  │
                                         └──────┬───────┘
                                                │ event
                                                ▼
                                  ┌────────────────────────┐
                                  │   Telos Solver (Rust)  │
                                  │   ─ Alloy listener     │
                                  │   ─ REVM simulator     │
                                  │   ─ Reth precompile    │
                                  └────────┬───────────────┘
                                           │ spot + perp
                                           ▼
                                  ┌────────────────────────┐
                                  │  Hyperliquid CLOB +    │
                                  │  Perp Order Book       │
                                  └────────┬───────────────┘
                                           │ settle
                                           ▼
                                  ┌────────────────────────┐
                                  │       Merchant         │
                                  └────────────────────────┘
```

| Layer | Responsibility | Tech |
| --- | --- | --- |
| **Listener** | Sub-100ms event capture from Tempo & Hyperliquid | Alloy (typed providers, `sol!` bindings) |
| **Settler** | Pre-flight simulation — does this route fill within slippage budget? | REVM (forked-state execution) |
| **Precompile** | On-chain custom opcode for atomic settle+hedge accounting | Reth (execution extension API) |

---

## Learning roadmap

Telos is structured to exercise the modern Rust+Ethereum stack one layer at a time. RethLab provides the source-level theory; Telos is where it ships.

| Phase | Tech focus | Deliverable |
| --- | --- | --- |
| **Week 1–2** | **Alloy** — typed providers, `sol!` macro, event subscriptions | `telos-listener` connects to Tempo & HL, prints incoming events |
| **Week 3–4** | **REVM** — forking, inspectors, gas accounting | `telos-settler` simulates swap+hedge against forked Tempo state |
| **Week 5–8** | **Reth** — execution extensions, custom precompiles | On-chain precompile for atomic settle accounting |
| **Week 9+** | Adversarial work — MEV resistance, oracle deviation circuit breakers, ZK private intents | Audit-ready spec |

Only after the off-chain pain is felt do we move logic on-chain. This is deliberate.

---

## Workspace layout

```
telos/
├── crates/
│   ├── telos-types/      # shared types: Intent, RouteQuote, etc.
│   ├── telos-listener/   # Alloy-based event listener (Week 1–2)
│   ├── telos-settler/    # REVM simulation harness (Week 3, fork support next)
│   ├── telos-precompile/ # Reth execution extension (Week 5–8) [planned]
│   └── telos-cli/        # binary entry point
└── Cargo.toml            # workspace
```

---

## Status

Concept stage. As of 2026-05-03:

**Listener** handles four modes — headers (default), typed `PaymentIntent` on a Tempo contract, typed `Fill` on a Hyperliquid HyperEVM contract, or both multiplexed via `tokio::select!`. Configure via env vars: `TELOS_TEMPO_WS_URL` + `TELOS_TEMPO_CONTRACT`, `TELOS_HL_WS_URL` + `TELOS_HL_CONTRACT`.

**Settler** runs each decoded intent through REVM. Two modes: in-memory empty state (`simulate_settlement`), or forked from a live RPC at the latest block (`simulate_settlement_forked`, async, dispatched via `spawn_blocking` to bridge async Alloy ↔ sync REVM). Set `TELOS_TEMPO_FORK_URL` to enable forked mode. Contract calldata and perp hedge sizing are next.

This repository is private during the learning phase. It will open if and when the architecture stabilizes.

---

## Why the name

τέλος is the Greek for *purpose, end, completion* — the word Aristotle used for the goal toward which a thing tends. Every other word in the aggregator vocabulary describes mechanism: *route, swap, settle, hedge*. Telos describes the user's relationship to the system. They state the end. The protocol owns the means.

---

## License

MIT — see [LICENSE](./LICENSE).
