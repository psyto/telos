# Telos Learning Walkthrough

A week-by-week build journey through the modern Rust + Ethereum stack —
**Alloy** (typed clients), **REVM** (Rust EVM), and the surrounding
**tokio** patterns you cannot avoid. By the end, you will have built an
intent-based aggregator that listens on two chains, simulates execution
against forked state, and broadcasts hedges when the simulation approves.

This is the practical companion to [psyto/rethlab](https://github.com/psyto/rethlab) —
RethLab teaches the stack from source, Telos shows what those concepts
look like once they are wired into a working system.

## How to read this

Each chapter tells you the same story:

1. **What you build** — the concrete deliverable that week.
2. **What you learn** — the concepts you have to internalize, with the
   relevant snippet so you can read the pattern in place.
3. **Where to look** — the file + commit so you can read the full diff.

The chapters are deliberately ordered. Earlier weeks set up muscle memory
that later weeks rely on. The reference table at the end lets you jump
back when you need to look something up.

---

## Week 1 — Alloy read path

**What you build.** A Cargo workspace with a single binary that opens a
WebSocket subscription to an Ethereum RPC and prints incoming block
headers. Then, a typed log subscription for a `PaymentIntent` event you
declare in inline Solidity.

**What you learn.**

The first muscle is `ProviderBuilder::new().connect_ws(...)` — Alloy's
canonical "give me a provider that talks to a chain over WebSocket"
pattern. From there, `provider.subscribe_blocks().await?.into_stream()`
gives you back a `Stream<Item = Header>` you can `while let Some(...)`
over. You are writing async Rust against typed wire formats; no
hand-rolled JSON-RPC.

```rust
let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await?;
let mut stream = provider.subscribe_blocks().await?.into_stream();

while let Some(header) = stream.next().await {
    info!(number = header.number, hash = %header.hash, "block");
}
```

The second muscle is `sol!`. You declare an event in Solidity inside
a `sol! { ... }` block, and the macro generates a Rust struct that
implements `SolEvent` (carrying `SIGNATURE_HASH` and `decode_log`).
The Solidity declaration *is* the source of truth — your Rust types
cannot drift from the on-chain interface, because they are generated
from it.

```rust
sol! {
    #[derive(Debug)]
    event PaymentIntent(
        bytes32 indexed intentId,
        address indexed payer,
        address indexed merchant,
        address settlementAsset,
        uint256 settlementAmount,
        uint16 maxSlippageBps,
        uint64 deadline
    );
}
```

The seam between *generated* and *domain* code is a `From` impl. The
`sol!` macro emits types with Solidity casing (`intentId`); your
domain types use snake_case (`intent_id`). One conversion impl is the
*one place* the boundary lives — call sites keep using domain types.

```rust
impl From<PaymentIntent> for telos_types::PaymentIntent {
    fn from(ev: PaymentIntent) -> Self {
        Self {
            intent_id: ev.intentId,
            payer: ev.payer,
            merchant: ev.merchant,
            settlement_asset: ev.settlementAsset,
            settlement_amount: ev.settlementAmount,
            max_slippage_bps: ev.maxSlippageBps,
            deadline: ev.deadline,
        }
    }
}
```

The third muscle is the typed filter:
`Filter::new().address(contract).event_signature(EventType::SIGNATURE_HASH)`.
Wrap it in a private helper the moment you have more than one
subscription. The signature hash is computed at compile time from the
`sol!` macro, so the filter is type-safe.

```rust
fn intent_filter(contract: Address) -> Filter {
    Filter::new()
        .address(contract)
        .event_signature(abi::PaymentIntent::SIGNATURE_HASH)
}
```

**Where to look.**

- `crates/telos-listener/src/lib.rs` — `watch_headers` ([3ba07dd](https://github.com/psyto/telos/commit/3ba07dd))
- `crates/telos-listener/src/abi.rs` — `sol!` block + From impl ([b31d22e](https://github.com/psyto/telos/commit/b31d22e))
- `crates/telos-listener/src/lib.rs` — `intent_filter`, `decode_intent` ([b31d22e](https://github.com/psyto/telos/commit/b31d22e))

---

## Week 2 — Multiplex two streams

**What you build.** A second typed subscription for Hyperliquid `Fill`
events on a different chain, then a `watch_both` function that runs
both subscriptions concurrently using `tokio::select!`.

**What you learn.**

`tokio::select!` is the routing-loop primitive every async Rust app
eventually needs. Its single most important property is
**cancel-safety**: when one arm fires, the futures in the other arms
are *dropped*. If those dropped futures had partial state (a half-read
buffer, an in-flight write), you lose it. `StreamExt::next` is
documented as cancel-safe, so a `select!` over two streams works
correctly. Internalize this rule first; it will save you from data
loss in every async program you write.

```rust
loop {
    tokio::select! {
        Some(log) = intents.next() => {
            if let Some(intent) = decode_intent(&log) { /* ... */ }
        }
        Some(log) = fills.next() => {
            if let Some(fill) = decode_fill(&log) { /* ... */ }
        }
        else => break,
    }
}
```

The second pattern: when the work in an arm is non-trivial, do not
`.await` it inside the arm. `tokio::spawn(handle_thing(item))`
detaches the work onto its own task so the `select!` loop stays
responsive to the next event. The trade-off is no backpressure — a
slow downstream can spawn unbounded tasks. That is real for
production; fine for now.

```rust
Some(log) = intents.next() => {
    if let Some(intent) = decode_intent(&log) {
        // fire-and-forget so the loop keeps draining the stream
        tokio::spawn(handle_intent_async(intent));
    }
}
```

**Where to look.**

- `crates/telos-listener/src/lib.rs` — `watch_both`, `tokio::select!` block ([ada0751](https://github.com/psyto/telos/commit/ada0751))
- `crates/telos-cli/src/main.rs` — env-driven mode dispatch (`Config::from_env`, `Mode`) ([ada0751](https://github.com/psyto/telos/commit/ada0751))

---

## Week 3 — REVM in-memory, then forked

**What you build.** A `telos-settler` crate that takes a decoded
`PaymentIntent` and replays it through REVM. First against an empty
in-memory state (proves the harness wires up), then against forked
state pulled from a live RPC via `AlloyDB`.

**What you learn.**

REVM 38's modern entry point is `Context::mainnet().with_db(db).build_mainnet()`.
The `MainBuilder` and `MainContext` traits gate the chained API; the
`ExecuteEvm` trait provides `transact`. Four imports cover the
minimum surface for any synchronous simulation.

```rust
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::TxEnv,
    database::{CacheDB, EmptyDB},
};

let mut evm = Context::mainnet()
    .with_db(CacheDB::<EmptyDB>::default())
    .build_mainnet();

let result = evm.transact(tx_env)?.result;
```

Then EIP-8037 surprises you: `ExecutionResult::gas_used()` is
deprecated in favour of `tx_gas_used()`. The split exists for
stateless-client pricing of state access — execution gas and state
gas are now distinct dimensions. You only need the per-tx total, but
the rename is worth understanding rather than mechanically following.

```rust
LegOutcome {
    success: result.is_success(),
    gas_used: result.tx_gas_used(),  // not gas_used() — deprecated
    revert_reason: /* ... */,
}
```

The hard problem of Week 3 is forking. You want to fetch real chain
state on demand: account balances, nonces, contract bytecode, storage
slots. REVM's `Database` trait is **sync**; `AlloyDB`'s state-fetch
is **async**. The bridge is `WrapDatabaseAsync`, which calls
`block_on` on a tokio handle. From inside an async worker, that
**deadlocks**. The fix is a recipe you will reuse forever:

```rust
let handle = tokio::runtime::Handle::current();

tokio::task::spawn_blocking(move || -> Result<SimulationOutcome> {
    // Re-enter the parent runtime so WrapDatabaseAsync can find a Handle
    // and submit async state fetches to it. Without `_enter`, the
    // blocking thread has no current runtime and `WrapDatabaseAsync::new`
    // returns None.
    let _enter = handle.enter();

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let alloy_db = AlloyDB::new(provider, block);
    let wrapped = WrapDatabaseAsync::new(alloy_db)
        .ok_or_else(|| eyre!("no tokio runtime available for AlloyDB"))?;
    let db = CacheDB::new(wrapped);

    let mut evm = Context::mainnet().with_db(db).build_mainnet();
    // ... transact ...
})
.await
.map_err(|e| eyre!("settler task panicked: {e}"))?
```

`spawn_blocking` moves the work onto a dedicated OS thread. `_enter =
handle.enter()` makes the parent runtime's `Handle` reachable via
`Handle::current()` on that thread, so `WrapDatabaseAsync::new` finds
a runtime to schedule its async fetches against. Without `_enter`,
the call returns `None`. The implicit thread-local runtime context is
something tokio leans on heavily; learn to spot it.

**Where to look.**

- `crates/telos-settler/src/lib.rs` — `simulate_settlement` (in-memory) ([c983ed1](https://github.com/psyto/telos/commit/c983ed1))
- `crates/telos-settler/src/lib.rs` — `simulate_settlement_forked` ([ea85199](https://github.com/psyto/telos/commit/ea85199))
- `Cargo.toml` — `revm = { version = "38", features = ["alloydb"] }` ([ea85199](https://github.com/psyto/telos/commit/ea85199))

---

## Week 4 — Real calldata, real revert reasons

**What you build.** Replace the symbolic `value=0` test transaction
with an actual `IERC20.transfer(merchant, amount)` call, encoded via
`sol!`-generated bindings. Decode the receipt: extract revert reasons
when the tx fails, find the matching `Transfer` event when it
succeeds.

**What you learn.**

`SolCall::abi_encode()` is the workhorse. Every Solidity function
`foo(...)` becomes a `fooCall { ... }` Rust struct. Construct it with
the args, call `.abi_encode()`, and you get selector + ABI-encoded
args ready for `TxEnv.data`. Same encoding path you will later use
for broadcast — one mental model covers simulation and submission.

```rust
let calldata = IERC20::transferCall {
    to: intent.merchant,
    amount: intent.settlement_amount,
}
.abi_encode();

TxEnv {
    caller: intent.payer,
    kind: TxKind::Call(intent.settlement_asset),  // the token contract
    data: Bytes::from(calldata),
    /* ... */
}
```

`ExecutionResult` is a sum type, not a boolean. Match on `Success {
logs, output }`, `Revert { output }`, `Halt { reason }`. The output
bytes mean different things in each variant: return data on success,
revert payload on revert, nothing on halt. Treating it as
`is_success() / else fail` collapses the most useful diagnostic. *Why*
it failed matters more than that it failed.

```rust
let revert_reason = match result {
    ExecutionResult::Revert { output, .. } => decode_revert_reason(output),
    _ => None,
};

let transfer_emitted = match result {
    ExecutionResult::Success { output: Output::Call(_), logs, .. } => logs
        .iter()
        .any(|log| match IERC20::Transfer::decode_log(log) {
            Ok(t) => t.from == from && t.to == to,
            Err(_) => false,
        }),
    _ => false,
};
```

For revert reasons, `alloy::sol_types::Revert::abi_decode(output)`
extracts the standard `Error(string)` payload — the one OpenZeppelin's
ERC-20 emits ("ERC20: transfer amount exceeds balance"). It needs
`SolError` in scope; the compiler error tells you so. That trait
gates the entire decode/encode infrastructure for revert types.
Custom errors and `Panic(uint256)` would each need their own selector
match; defer until you hit one.

```rust
use alloy::sol_types::SolError;  // <-- the trait that gates abi_decode

fn decode_revert_reason(output: &Bytes) -> Option<String> {
    let revert = alloy::sol_types::Revert::abi_decode(output).ok()?;
    Some(revert.reason)
}
```

**Where to look.**

- `crates/telos-settler/src/lib.rs` — `build_spot_tx`, `decode_outcome`, `decode_revert_reason` ([024e3e9](https://github.com/psyto/telos/commit/024e3e9))
- `crates/telos-settler/src/abi.rs` — `IERC20` interface ([024e3e9](https://github.com/psyto/telos/commit/024e3e9))

---

## Week 5 — A feedback loop with shared state

**What you build.** A `PriceBook` that records HL `Fill` events as
mark prices keyed by spot asset. When a `PaymentIntent` decodes, the
listener reads the book to size the hedge. Same task pool, shared
state, no channels.

**What you learn.**

The lock you reach for inside async code is `tokio::sync::RwLock`.
Its `read()` and `write()` futures yield the task instead of blocking
the worker thread. `std::sync::RwLock` would block — fatal in a
single-threaded scheduler, fine but wasteful in a multi-threaded one.
`parking_lot::RwLock` is correct inside async if your critical
section is microseconds; learn to spot when each applies.

Hide the lock behind a struct, not a type alias. Exposing
`Arc<RwLock<HashMap<Address, PriceQuote>>>` to every caller leaks
your storage choice. Wrapping it in `PriceBook` with `record_fill`
and `get` methods means you can swap in `parking_lot::RwLock`, an
LRU cache, or a sharded map later without touching call sites.

```rust
#[derive(Clone, Default)]
pub struct PriceBook {
    inner: Arc<RwLock<HashMap<Address, PriceQuote>>>,
}

impl PriceBook {
    pub async fn record_fill(&self, fill: &Fill) {
        let quote = PriceQuote { /* ... */ };
        self.inner.write().await.insert(fill.asset, quote);
    }

    pub async fn get(&self, asset: Address) -> Option<PriceQuote> {
        self.inner.read().await.get(&asset).copied()
    }
}
```

`Option<T>` is the right shape for "no price observed yet." It is
not an error — it is a state. Returning `Option<RouteQuote>` (rather
than `Result<RouteQuote, NoPriceError>`) makes the absence first-class.
The listener logs a warning and proceeds; once a fill arrives, the
next intent gets a real quote without retry logic.

```rust
pub async fn quote_route(
    intent: &PaymentIntent,
    prices: &PriceBook,
    hedge_venue: Address,
) -> Option<RouteQuote> {
    let quote = prices.get(intent.settlement_asset).await?;  // None if no price
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();

    Some(RouteQuote {
        intent_id: intent.intent_id,
        spot_amount: intent.settlement_amount,
        hedge_size: intent.settlement_amount,
        /* ... */
    })
}
```

The architectural insight: a feedback loop in the same process does
not need channels. Channels become necessary when writer and reader
run on different processes, or when the writer needs backpressure
against a slow reader. Reaching for them prematurely adds overhead
and complexity for problems you do not have.

**Where to look.**

- `crates/telos-settler/src/lib.rs` — `PriceBook`, `quote_route` ([85b5a14](https://github.com/psyto/telos/commit/85b5a14))
- `crates/telos-listener/src/lib.rs` — `decode_fill` writes, `spawn_simulation` reads ([85b5a14](https://github.com/psyto/telos/commit/85b5a14))

---

## Week 6 — Two legs in one EVM context

**What you build.** Layer the perp hedge into the simulation as a
second transaction. The settler now runs the spot transfer, then —
against the same EVM — runs `IHyperliquidGateway.placeShort(...)`.
Both legs together either succeed atomically or you know which one
broke.

**What you learn.**

REVM's `Context` is **mutable across `transact` calls**. The hedge
tx sees the post-state of the spot tx — nonces incremented, balances
changed, storage written. This is *the* property that makes REVM
useful for sequential simulation. If you wanted isolated per-tx
execution, you would reset the DB between calls. Sequential
`transact` is correct for "what happens if these run in order."

```rust
// Both calls go through the same `evm` — the second sees the first's
// post-state automatically. No re-build, no DB reset.
let spot_result = evm.transact(build_spot_tx(intent))?.result;
let hedge_result = evm.transact(build_hedge_tx(payer, route, slippage))?.result;
```

When the same logic needs to run against two different database
backends (`CacheDB<EmptyDB>` for empty state, `CacheDB<WrapDatabaseAsync<AlloyDB>>`
for forked state), make the function generic over `DB`. The trait
bound `where DB: Database, DB::Error: std::error::Error + Send +
Sync + 'static` is the minimum needed for `eyre`'s `?` operator to
flow through. You write the leg-walking code once; it compiles
against both worlds.

```rust
fn run_legs<DB>(
    evm: &mut MainnetEvm<Context<BlockEnv, TxEnv, CfgEnv, DB>>,
    intent: &PaymentIntent,
    route: Option<&RouteQuote>,
    /* ... */
) -> Result<SimulationOutcome>
where
    DB: revm::database_interface::Database,
    <DB as revm::database_interface::Database>::Error:
        std::error::Error + Send + Sync + 'static,
{
    /* same code path for empty-state and forked-state */
}
```

The reporting shape matters. A `bool` for "did the simulation
succeed" collapses information you will want during debugging. A
`SimulationOutcome { spot, hedge, atomic_success }` lets you see
*which* leg blocked atomicity. "The hedge would have worked but the
spot reverted because the allowance was zero" is operationally
different from "the hedge venue has no bytecode." Per-leg outcomes
preserve that.

```rust
pub struct LegOutcome {
    pub success: bool,
    pub gas_used: u64,
    pub revert_reason: Option<String>,
}

pub struct SimulationOutcome {
    pub spot: LegOutcome,
    pub hedge: Option<LegOutcome>,    // None when no RouteQuote was supplied
    pub transfer_emitted: bool,
    pub hedge_acked: bool,
    pub atomic_success: bool,         // AND of both legs
}
```

Mock interfaces are a legitimate development tool. `IHyperliquidGateway.placeShort`
will never exist on real HL — that chain routes through L1 actions
via CoreWriter precompiles. But for *simulation*, the settler only
needs a known selector to encode and a known event signature to
detect. The mock lets the rest of the architecture solidify before
you wire the real plumbing.

**Where to look.**

- `crates/telos-settler/src/lib.rs` — `run_legs<DB>`, `LegOutcome`, `SimulationOutcome` ([03bc090](https://github.com/psyto/telos/commit/03bc090))
- `crates/telos-settler/src/abi.rs` — `IHyperliquidGateway` mock interface ([03bc090](https://github.com/psyto/telos/commit/03bc090))

---

## Week 7 — Decision gate and dry-run-default broadcast

**What you build.** The settler stops at "would this work" and
starts at "should we send it." A `should_submit(outcome, route, intent,
cfg)` function returns `Submit(SubmissionPlan)` or `Reject(RejectReason)`.
A new `telos-submitter` crate takes the plan and broadcasts via an
Alloy wallet-aware provider — but defaults to dry-run.

**What you learn.**

A sum type for decisions beats `Result<SubmissionPlan>`. `SettlerDecision::Submit`
carries *what to send*; `Reject` carries *why not*. Both branches log
structured information. A `Result` would conflate "no quote yet" (a
state) with "system error" (a bug). Keep operationally distinct
outcomes in distinct variants.

```rust
pub enum SettlerDecision {
    Submit(SubmissionPlan),
    Reject(RejectReason),
}

pub enum RejectReason {
    NoQuote,
    SpotWouldRevert(Option<String>),
    HedgeWouldRevert(Option<String>),
    PriceTooStale { age_secs: u64, max_secs: u64 },
}
```

The Alloy wallet-aware provider is `ProviderBuilder::new().wallet(wallet).connect_http(url)`.
The `wallet` filler middleware composes with default fillers — nonce,
gas, chain-id — so `provider.send_transaction(tx)` produces a
fully-formed signed tx from a sparse `TransactionRequest`. You write
business logic against unsigned/incomplete txs; middleware fills the
rest. Do not build TransactionRequests by hand when you can let the
chain.

```rust
let wallet = EthereumWallet::from(PrivateKeySigner::from_str(key)?);
let provider = ProviderBuilder::new()
    .wallet(wallet)
    .connect_http(rpc_url.parse()?);

let tx = TransactionRequest::default()
    .with_from(signer_address)
    .with_to(prepared.to)
    .with_input(prepared.data.clone())
    .with_gas_limit(prepared.gas_limit);

let pending = provider.send_transaction(tx).await?;  // signed + broadcast
```

Risky operations need **two gates**. The submitter's `broadcast: bool`
is captured at construction *and* `TELOS_BROADCAST=1` is required at
the CLI. Either gate alone keeps you in dry-run. This is defense in
depth: a misconfigured submitter that silently broadcasts is
unrecoverable, so the dry-run default should never quietly flip.

```rust
// Gate 1: at construction
let submitter = Submitter::new(rpc_url, signer_key, /* broadcast: */ cfg.broadcast)?;

// Gate 2: in CLI env parsing
broadcast: matches!(std::env::var("TELOS_BROADCAST").as_deref(), Ok("1")),
```

A compile-time `Send + Sync` guard is one of the cheapest correctness
mechanisms in Rust:

```rust
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Submitter>();
};
```

Since the listener clones `Submitter` across async tasks, that
property must hold. If a future change adds an `Rc` or `RefCell`
field, the build breaks at this line. Catching it at compile time
beats discovering it at runtime when a clone fails.

The architectural decision worth flagging: **Telos signs only what
Telos owns**. The spot leg is the merchant's settlement, initiated
by the payer on Tempo — observed for *gating* but not broadcast.
Telos signs only the hedge. A bundler/Permit2 design that pulls
tokens from the payer would change that scope. Decisions about *who
signs what* are architectural, not implementation details — they
constrain the trust model and the regulatory surface.

```rust
/// Telos signs only the hedge — the spot leg is the merchant's
/// settlement, initiated by the payer on Tempo, observed here for
/// *gating* but not broadcast by us.
pub struct SubmissionPlan {
    pub intent_id: B256,
    pub hedge: PreparedTx,
    // no `spot: PreparedTx` field — by design
}
```

**Where to look.**

- `crates/telos-settler/src/lib.rs` — `SettlerDecision`, `RejectReason`, `should_submit` ([138bdf2](https://github.com/psyto/telos/commit/138bdf2))
- `crates/telos-submitter/src/lib.rs` — `Submitter::new`, two-gate pattern, `assert_send_sync` ([138bdf2](https://github.com/psyto/telos/commit/138bdf2))
- `crates/telos-cli/src/main.rs` — `TELOS_BROADCAST`, `TELOS_SIGNER_KEY`, `TELOS_SUBMIT_RPC_URL` ([138bdf2](https://github.com/psyto/telos/commit/138bdf2))

---

## Week 8 — Closing the loop with confirmation

**What you build.** Past the broadcast point: `submit_and_confirm`
awaits the receipt under a `(confirmations, timeout)` budget, decodes
the `OrderPlaced` event from the receipt logs, and returns a typed
`SettlementResult`. The system now reports a terminal state for every
intent it touches.

**What you learn.**

The Alloy confirmation pattern is the `PendingTransactionBuilder`:

```rust
let receipt = pending
    .with_required_confirmations(cfg.confirmations)
    .with_timeout(Some(cfg.timeout))
    .get_receipt()
    .await?;
```

Each builder method returns `Self`; `get_receipt()` consumes the
chain. Policy lives in the builder, business logic stays clean.
Tune `confirmations` per chain and per asset.

EIP-658 added a `status` field to receipts: `true` = success, `false`
= revert. Pre-Byzantium chains report by post-state hash instead, but
Alloy abstracts that. Trust `receipt.status()`. Receipts do **not**
carry revert data, only this bit + logs. To recover the revert
*reason* from a failed receipt, you have to re-call `eth_call`
against the same block — defer until you need it.

```rust
if !receipt.status() {
    return Ok(SettlementResult::Failed {
        intent_id: plan.intent_id,
        tx_hash,
        block_number,
    });
}

let hedge_acked = receipt.inner.logs().iter().any(|log| {
    matches!(IHyperliquidGateway::OrderPlaced::decode_log(&log.inner), Ok(_))
});
```

The single most important API distinction in this commit is the
shape of the return type. Submitter-level errors (invalid RPC URL,
malformed signer key) propagate as `Result::Err`. Transaction-level
outcomes (revert, timeout) come back as `Ok(SettlementResult::Failed
| Timeout)`. **A revert is normal operation** — the system did its
job, the chain rejected the tx. Forcing callers to `match
err.downcast()` to handle expected outcomes is wrong. Bugs in
`Result::Err`; outcomes in the success type.

```rust
pub enum SettlementResult {
    DryRun   { intent_id: B256, estimated_gas: u64 },
    Confirmed { intent_id: B256, tx_hash: B256, block_number: u64,
                gas_used: u64, hedge_acked: bool },
    Failed   { intent_id: B256, tx_hash: B256, block_number: u64 },
    Timeout  { intent_id: B256, tx_hash: B256, waited_secs: u64 },
}

// Errors are *bugs* (bad URL, bad key); outcomes are *states*.
pub async fn submit_and_confirm(/* ... */) -> Result<SettlementResult> { /* ... */ }
```

When two crates need the same ABI, promoting `mod abi` to `pub mod abi`
gives you a single source of truth. Both the simulator and the
submitter need to decode `OrderPlaced`. Duplicating the `sol!` block
in the submitter would invite drift. Promoting the module is the
cleanest way to share — though the abi module then becomes part of
the crate's public surface, and breaking changes there ripple.

```rust
// crates/telos-settler/src/lib.rs
pub mod abi;  // was: mod abi;

// crates/telos-submitter/src/lib.rs
use telos_settler::abi::IHyperliquidGateway;
```

**Where to look.**

- `crates/telos-submitter/src/lib.rs` — `submit_and_confirm`, `ConfirmConfig` ([5a97014](https://github.com/psyto/telos/commit/5a97014))
- `crates/telos-types/src/lib.rs` — `SettlementResult` enum ([5a97014](https://github.com/psyto/telos/commit/5a97014))
- `crates/telos-settler/src/lib.rs` — `pub mod abi` ([5a97014](https://github.com/psyto/telos/commit/5a97014))

---

## Quick reference

When you want to look something up rather than re-read the chapter.

| Topic | Concept | Where |
|---|---|---|
| **Alloy** | `sol!` macro | abi.rs files |
| **Alloy** | `From<GeneratedType> for DomainType` seam | listener/abi.rs |
| **Alloy** | `Filter::new().address().event_signature()` | listener/lib.rs |
| **Alloy** | `SolCall::abi_encode` | settler/lib.rs `build_spot_tx` |
| **Alloy** | `Revert::abi_decode` (needs `SolError` in scope) | settler/lib.rs `decode_revert_reason` |
| **Alloy** | Wallet-aware `ProviderBuilder` | submitter/lib.rs |
| **Alloy** | `PendingTransactionBuilder` | submitter/lib.rs `submit_and_confirm` |
| **Alloy** | `receipt.status()` (EIP-658) | submitter/lib.rs |
| **REVM** | `Context::mainnet().with_db().build_mainnet()` | settler/lib.rs |
| **REVM** | `tx_gas_used()` (EIP-8037 split) | settler/lib.rs `decode_leg` |
| **REVM** | `ExecutionResult` sum type | settler/lib.rs `decode_outcome` |
| **REVM** | Mutable `Context` across `transact` | settler/lib.rs `run_legs` |
| **REVM** | Generic over `DB` | settler/lib.rs `run_legs<DB>` |
| **REVM** | `alloydb` feature flag | Cargo.toml workspace |
| **REVM** | `AlloyDB` + `WrapDatabaseAsync` + `spawn_blocking` + `Handle::enter()` | settler/lib.rs `simulate_settlement_forked` |
| **Tokio** | `select!` cancel-safety | listener/lib.rs `watch_both` |
| **Tokio** | Spawn rather than await in arms | listener/lib.rs `spawn_simulation` |
| **Tokio** | `tokio::sync::RwLock` vs `std::sync::RwLock` | settler/lib.rs `PriceBook` |
| **Tokio** | Errors vs outcomes in async APIs | submitter/lib.rs `submit_and_confirm` |
| **Design** | Hide sync primitives behind a struct | settler/lib.rs `PriceBook` |
| **Design** | Per-leg outcomes vs single boolean | settler/lib.rs `LegOutcome` |
| **Design** | Sum type for decisions | settler/lib.rs `SettlerDecision` |
| **Design** | `Option<T>` as soft failure | settler/lib.rs `quote_route` |
| **Design** | Two gates for risky ops | submitter/lib.rs + cli/main.rs |
| **Design** | Compile-time `Send + Sync` guard | submitter/lib.rs |
| **Design** | Single source of truth for ABI | settler/lib.rs `pub mod abi` |
| **Architecture** | Mode dispatch from env | cli/main.rs |
| **Architecture** | Same-process feedback loop, no channels | listener + settler |
| **Architecture** | Inline vs worker placement | listener/lib.rs `spawn_simulation` |
| **Architecture** | Telos signs only what Telos owns | settler/lib.rs `SubmissionPlan` doc |
| **Architecture** | Mock interfaces during development | settler/abi.rs `IHyperliquidGateway` |
