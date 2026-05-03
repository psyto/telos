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

## Week 9 — Persistence with sqlx

**What you build.** A `telos-store` crate that writes every intent's
lifecycle into sqlite via sqlx. One append-only `intent_events` table
keyed by `intent_id`. The listener records at every stage —
observed → quoted → simulated → decided → settled — and on startup the
CLI reports how many intents were observed but never settled in the
previous run.

**What you learn.**

The first decision is **schema shape**: per-stage tables vs a single
event log. Per-stage tables give you stronger typing and cleaner
queries, but they couple the schema to the domain types — every change
to `RouteQuote` or `SimulationOutcome` is a migration. The single
events table treats the lifecycle as what it is — append-only — and
keeps the schema stable while the domain evolves. The cost is no
SQL-level filtering on payload contents, but that is what the
application layer is for.

```sql
CREATE TABLE intent_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    intent_id    TEXT    NOT NULL,
    stage        TEXT    NOT NULL,
    payload_json TEXT    NOT NULL,
    created_at   INTEGER NOT NULL
);
```

The sqlx workflow is **migrations + pool**. `sqlx::migrate!("./migrations")`
expands at compile time into an embedded migrator that runs whatever
`*.sql` files live in the directory. `SqlitePool::connect_with(opts)`
gives you a cheap-to-clone pool — internally `Arc`-shared, so every
`Store` clone hands out connections from the same backing pool.

```rust
let opts = SqliteConnectOptions::from_str(url)?
    .create_if_missing(true);
let pool = SqlitePool::connect_with(opts).await?;

sqlx::migrate!("./migrations")
    .run(&pool)
    .await?;
```

`payload: impl Serialize` lets the store stay generic over what the
application is recording. Each stage hands in its own struct
(`PaymentIntent`, `RouteQuote`, `SimulationOutcome`,
`SettlerDecision`, `SettlementResult`); the store turns whichever it
gets into a JSON string and writes the row. No giant enum, no
per-stage method.

```rust
pub async fn record_event<P: Serialize>(
    &self,
    intent_id: B256,
    stage: Stage,
    payload: &P,
) -> Result<()> {
    let payload_json = serde_json::to_string(payload)?;
    sqlx::query(
        "INSERT INTO intent_events (intent_id, stage, payload_json, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(format!("{:#x}", intent_id))
    .bind(stage.as_str())
    .bind(&payload_json)
    .bind(now_secs())
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

The most important architectural rule for any persistence layer
attached to a hot pipeline: **store failures must not poison the
pipeline**. The in-memory feedback loop is the source of truth for
correctness; the database is a side channel for restart reconciliation
and operator queries. A sqlite write that times out should produce a
warning log line and nothing else — never a panic, never a `?` that
unwinds the whole task.

```rust
async fn record<P: serde::Serialize>(
    store: &Option<Store>,
    intent_id: B256,
    stage: Stage,
    payload: &P,
) {
    if let Some(s) = store
        && let Err(err) = s.record_event(intent_id, stage, payload).await
    {
        // log and move on — never let the store poison the pipeline
        warn!(?err, ?stage, %intent_id, "store write failed");
    }
}
```

The reconciliation query — distinct intents that have an `observed`
row but no `settled` row — is the simplest "what was in flight when we
crashed" question you can ask of an event log. The CLI logs that
count at startup so an operator notices when restart leaves work
behind. Real reconciliation logic (re-quote? abandon? mark stale?)
lives downstream; for now the visibility is the point.

```rust
pub async fn count_pending(&self) -> Result<u64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT intent_id) FROM intent_events \
         WHERE stage = 'observed' \
           AND intent_id NOT IN ( \
               SELECT DISTINCT intent_id FROM intent_events WHERE stage = 'settled' \
           )",
    )
    .fetch_one(&self.pool)
    .await?;
    Ok(row.0 as u64)
}
```

Persistence is opt-in via `TELOS_DB_URL`. Unset the env var and the
listener runs exactly as before — no sqlite, no migration, no on-disk
state. This matters during local development and during the learning
phase: you can iterate on the simulator without managing a database.

**Where to look.**

- `crates/telos-store/migrations/0001_initial.sql` — the schema ([b78cfc5](https://github.com/psyto/telos/commit/b78cfc5))
- `crates/telos-store/src/lib.rs` — `Store::open`, `record_event`, `count_pending` ([b78cfc5](https://github.com/psyto/telos/commit/b78cfc5))
- `crates/telos-listener/src/lib.rs` — `record()` helper, store calls in `spawn_simulation` ([b78cfc5](https://github.com/psyto/telos/commit/b78cfc5))
- `crates/telos-cli/src/main.rs` — `build_store`, `TELOS_DB_URL` ([b78cfc5](https://github.com/psyto/telos/commit/b78cfc5))

---

## Week 10 — Your first custom EVM precompile

**What you build.** A `telos-precompile` crate that exports
`intent_digest`, an Eth-style precompile. It lives at a fixed address
(`0x…0901`), takes the ABI-encoded intent fields, returns the canonical
32-byte keccak digest. The crate stays node-agnostic: it ships the
function and unit tests, leaving the Reth-node wiring as the next
deliberate step.

**What you learn.**

A precompile is **native Rust at a fixed EVM address**. When a tx
targets that address, the EVM short-circuits and runs your function
instead of interpreting bytecode. Existing Ethereum precompiles
(ecrecover at 0x01, sha256 at 0x02, modexp at 0x05, …) all follow this
pattern. Custom L1s and L2s extend the set with their own — Hyperliquid's
CoreWriter, Polygon's StateReader, and so on. Telos's `intent_digest`
joins that family.

The Eth-style signature is the simple one — `&[u8]` in, gas limit in,
`EthPrecompileResult` out. `EthPrecompileOutput` carries the gas used
and the output bytes; `PrecompileHalt` carries the non-fatal failure
reasons (out-of-gas, wrong input length, curve-point-off-curve, etc.).
Crucially, **halts are `Ok(...)`-shaped at the framework level** — they
unwind the EVM call cleanly, not the host process. Only fatal,
unrecoverable errors go in the outer `Err`.

```rust
pub fn intent_digest(input: &[u8], gas_limit: u64) -> EthPrecompileResult {
    let gas_used = INTENT_DIGEST_BASE_GAS + (input.len() as u64) * INTENT_DIGEST_PER_BYTE_GAS;
    if gas_used > gas_limit {
        return Err(PrecompileHalt::OutOfGas);
    }
    if input.len() != INTENT_DIGEST_INPUT_LEN {
        return Err(PrecompileHalt::other(format!(
            "intent_digest: expected {} bytes, got {}",
            INTENT_DIGEST_INPUT_LEN,
            input.len(),
        )));
    }
    let digest = keccak256(input);
    Ok(EthPrecompileOutput::new(
        gas_used,
        Bytes::copy_from_slice(digest.as_slice()),
    ))
}
```

The gas schedule mirrors SHA-256: a fixed base plus a small per-byte
cost. Fixed base covers the dispatch overhead; per-byte covers the work
keccak does on the buffer. Mirroring an existing precompile's shape
makes the cost predictable to anyone who has already integrated SHA-256.

```rust
pub const INTENT_DIGEST_BASE_GAS: u64 = 500;
pub const INTENT_DIGEST_PER_BYTE_GAS: u64 = 25;
```

**Wrong-length input is a halt, not a zero-pad.** This is a defensive
default that costs nothing but prevents an entire class of subtle bugs:
a caller could otherwise truncate or omit fields and silently get back
a "valid" digest of the wrong data.

```rust
if input.len() != INTENT_DIGEST_INPUT_LEN {
    return Err(PrecompileHalt::other(/* … */));
}
```

The unit tests are the first integration point. They exercise the
function directly — happy path matches an off-chain `keccak256`, OOG
halts before any hashing happens, wrong-length input halts with a
descriptive message, and the same input always produces the same
digest. These properties are the *contract* a downstream Reth node will
rely on; pinning them at the unit level catches regressions early.

```rust
#[test]
fn happy_path_matches_offchain_keccak() {
    let input = sample_input();
    let result = intent_digest(&input, 100_000).expect("should succeed");
    let expected = keccak256(input.as_slice());
    assert_eq!(result.bytes.as_ref(), expected.as_slice());
}
```

**What is deliberately out of scope here.** Wiring this precompile into
an actual REVM context (via `PrecompileProvider`) and into a Reth node
(via the node config + a custom EVM type) is the next deliberate step,
not this commit. The shape of that work:

- Implement `PrecompileProvider<CTX>` — likely by wrapping
  `EthPrecompiles` and intercepting the `intent_digest` address.
- Use it as the `Precompiles` parameter when configuring a custom EVM.
- For a real Reth node: pass the custom EVM type into the node builder.

Splitting the function from the wiring keeps this crate node-agnostic.
The same `intent_digest` function will eventually run in unit tests, in
a custom REVM, and in a Reth node — three different harnesses, one
implementation.

**Where to look.**

- `crates/telos-precompile/src/lib.rs` — `intent_digest`, address constant, gas constants ([622ecf3](https://github.com/psyto/telos/commit/622ecf3))
- `crates/telos-precompile/src/lib.rs#tests` — happy path + halts + determinism ([622ecf3](https://github.com/psyto/telos/commit/622ecf3))

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
| **sqlx** | Migrations via `sqlx::migrate!()` | store/src/lib.rs `Store::open` |
| **sqlx** | `SqlitePool` is `Arc`-shared, cheap to clone | store/src/lib.rs |
| **sqlx** | Generic `record_event<P: Serialize>` over JSON payload | store/src/lib.rs |
| **Design** | Single events table for lifecycle log | store/migrations/0001_initial.sql |
| **Design** | Store writes are best-effort, never poison the pipeline | listener/src/lib.rs `record` helper |
| **Architecture** | Persistence is opt-in via env var | cli/src/main.rs `build_store` |
| **Architecture** | Pending-intents query for restart reconciliation | store/src/lib.rs `count_pending` |
| **REVM** | Eth-style precompile signature `fn(&[u8], u64) -> EthPrecompileResult` | precompile/src/lib.rs `intent_digest` |
| **REVM** | `PrecompileHalt` for non-fatal failures (OOG, bad input) | precompile/src/lib.rs |
| **REVM** | Halts are `Ok` at the framework level; only fatal errors are `Err` | precompile/src/lib.rs |
| **Design** | Precompile gas schedule: base + per-byte | precompile/src/lib.rs |
| **Design** | Wrong-length input halts rather than zero-pads | precompile/src/lib.rs `intent_digest` |
| **Architecture** | Precompile crate stays node-agnostic; wiring is downstream | precompile/src/lib.rs (module doc) |
