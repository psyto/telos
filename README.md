# Telos

> τέλος — *the intended end*. You declare the outcome; the protocol resolves the path.

**Telos** is an intent-based atomic aggregator connecting [Tempo](https://tempo.network) (settlement-specialized Reth chain) and [Hyperliquid](https://hyperliquid.xyz) (CLOB-native L1). A merchant or user submits a single intent — *"settle this payment in USD-equivalent value"* — and Telos routes the execution across both venues, hedging price risk in the settlement window.

This is a **research project**, not yet a product. It is also the practical companion to [psyto/rethlab](https://github.com/psyto/rethlab): RethLab teaches the Rust EVM stack from source; Telos puts it into production.

A running list of concepts internalized while building Telos lives in [LEARNING.md](./LEARNING.md), organized by topic with pointers back to the commits that introduced them.

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
│   ├── telos-settler/    # REVM simulation + decision gating (Weeks 3–6)
│   ├── telos-submitter/  # signed broadcast + confirmation (Weeks 7–8)
│   ├── telos-store/      # sqlx + sqlite event log per intent (Week 9)
│   ├── telos-precompile/ # custom EVM precompile: intent_digest (Week 10)
│   └── telos-cli/        # binary entry point
└── Cargo.toml            # workspace
```

---

## Status

Concept stage. As of 2026-05-03:

**Listener** handles four modes — headers (default), typed `PaymentIntent` on a Tempo contract, typed `Fill` on a Hyperliquid HyperEVM contract, or both multiplexed via `tokio::select!`. Configure via env vars: `TELOS_TEMPO_WS_URL` + `TELOS_TEMPO_CONTRACT`, `TELOS_HL_WS_URL` + `TELOS_HL_CONTRACT`.

**Settler** runs each decoded intent through REVM in two sequential legs against the same EVM context — REVM's `transact` accumulates state, so the hedge tx sees the post-state of the spot tx.

- **Spot leg**: `IERC20.transfer(merchant, amount)` against the settlement asset.
- **Hedge leg** (only when `TELOS_HL_GATEWAY` is set): `IHyperliquidGateway.placeShort(asset, size, maxSlippageBps)` against the configured HL builder/bridge address.

Both modes — empty in-memory state (`simulate_settlement`) and forked from a live RPC (`simulate_settlement_forked`, async, dispatched via `spawn_blocking` to bridge async Alloy ↔ sync REVM) — walk the same two-leg path. Outcomes carry per-leg success, gas, decoded revert reason, plus an `atomic_success` flag that is the AND of both legs.

**Feedback loop**: a shared `PriceBook` is updated by every HL `Fill` and read by every decoded intent. `quote_route(intent, &prices, hedge_venue)` returns a `RouteQuote` with spot amount and a 1:1 hedge size at the current mark — the placeholder that funding-tilt logic will replace.

**Decision gate**: `should_submit(outcome, route, intent, cfg)` returns `Submit(SubmissionPlan)` or `Reject(reason)` — rejecting on no quote, spot or hedge revert, or stale price (`max_price_age_secs` defaults to 30).

**Submitter** (Weeks 7–8): when approved, the hedge tx is handed to a `Submitter` backed by an Alloy wallet-aware provider. **Dry-run by default**: estimates gas and logs the intended broadcast. Set `TELOS_BROADCAST=1` *and* provide `TELOS_SIGNER_KEY` + `TELOS_SUBMIT_RPC_URL` to actually broadcast. Telos signs only the hedge — the spot leg is the merchant's settlement, observed for gating but not broadcast by us.

After broadcast, `submit_and_confirm` awaits the receipt under a `ConfirmConfig { confirmations, timeout }` budget and decodes any emitted `OrderPlaced` event from the receipt logs. Returns a typed `SettlementResult`: `DryRun`, `Confirmed { tx_hash, block_number, gas_used, hedge_acked }`, `Failed { ... }` (mined but reverted), or `Timeout { waited_secs }`.

**Persistence** (Week 9): set `TELOS_DB_URL=sqlite://./telos.db` to enable the `telos-store` event log. Every intent's lifecycle — observed → quoted → simulated → decided → settled — is appended as JSON-payload rows in a single `intent_events` table. On startup the CLI reports how many intents were observed but never settled in a previous run (the reconciliation set). Store writes are best-effort; a sqlite hiccup is logged but never poisons the in-memory pipeline.

**Custom precompile** (Weeks 10–11): `telos-precompile` exposes `intent_digest`, an Eth-style precompile at `0x…0901` that hashes the canonical intent fields and returns a 32-byte digest. `TelosPrecompiles` wraps the standard `EthPrecompiles` and intercepts that one address — implementing `PrecompileProvider`. Constructing a custom EVM is one line: `Evm::new(ctx, EthInstructions::new_mainnet_with_spec(spec), TelosPrecompiles::new(spec))`. An integration test sends a tx through that EVM, calls the precompile, and asserts the returned bytes equal the off-chain keccak.

This repository is private during the learning phase. It will open if and when the architecture stabilizes.

---

## Why the name

τέλος is the Greek for *purpose, end, completion* — the word Aristotle used for the goal toward which a thing tends. Every other word in the aggregator vocabulary describes mechanism: *route, swap, settle, hedge*. Telos describes the user's relationship to the system. They state the end. The protocol owns the means.

---

## License

MIT — see [LICENSE](./LICENSE).
