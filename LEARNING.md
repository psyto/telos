# Telos Learning Notes

Concepts internalized while building Telos, organized by topic. Each entry has the rule, why it matters, and a pointer to where it lives in the code.

This is the practical companion to [psyto/rethlab](https://github.com/psyto/rethlab) — RethLab covers the theory by reading source, Telos shows what those concepts look like when wired into a real system.

---

## Alloy

### `sol!` accepts multiple declarations in one block
Events, structs, errors, and full interfaces can be declared together. They share generated infrastructure and stay co-located with the contract surface they describe.
- **Why:** the Solidity declaration *is* the source of truth; the Rust types are generated. No drift between on-chain interface and Rust struct.
- **Where:** `crates/telos-listener/src/abi.rs`, `crates/telos-settler/src/abi.rs` (commits b31d22e, ada0751)

### `From<GeneratedType> for DomainType` is the seam
`sol!` generates wire-format types with Solidity casing (`intentId`, not `intent_id`). The conversion impl is the boundary between *generated* and *domain* code.
- **Why:** future contract changes get isolated to the seam — call sites keep using domain types. Don't pass generated types around.
- **Where:** `crates/telos-listener/src/abi.rs` (commit b31d22e)

### `Filter` builder for typed log subscriptions
`Filter::new().address(contract).event_signature(EventType::SIGNATURE_HASH)` is the canonical pattern. Wrap in a private helper when you have multiple subscriptions against different addresses.
- **Why:** event signatures are computed at compile time from the `sol!` macro, so the filter is type-safe.
- **Where:** `intent_filter` and `fill_filter` in `crates/telos-listener/src/lib.rs` (commit ada0751)

### `SolCall::abi_encode()` for outbound calldata
Every Solidity function `foo(...)` becomes a `fooCall { ... }` struct. Construct it with the args, call `.abi_encode()`, and you have selector + ABI-encoded args ready for `TxEnv.data` or `TransactionRequest.input`.
- **Why:** same encoding path for simulation (REVM) and broadcast (Alloy provider). One mental model.
- **Where:** `build_spot_tx`, `hedge_calldata` in `crates/telos-settler/src/lib.rs` (commits 024e3e9, 03bc090)

### Revert decoding requires the `SolError` trait in scope
`Revert::abi_decode(output)` needs `use alloy::sol_types::SolError;` — the compiler error points right at it. The trait gates the entire decode/encode infrastructure for revert types.
- **Why:** rustc's "trait not in scope" hint is your debugger. Especially with `sol!`-generated code where the trait often isn't obvious from the type name.
- **Where:** `decode_revert_reason` in `crates/telos-settler/src/lib.rs` (commit 024e3e9)

### Wallet-aware provider via `ProviderBuilder::new().wallet(wallet)`
The `wallet` filler middleware composes with default fillers (nonce, gas, chain-id), so `provider.send_transaction(tx)` produces a fully-formed signed tx from a sparse `TransactionRequest`.
- **Why:** modern Alloy lets you write business logic against unsigned/incomplete txs; middleware handles the rest. Don't build TransactionRequests yourself when you can let the chain.
- **Where:** `Submitter::submit_and_confirm` in `crates/telos-submitter/src/lib.rs` (commit 138bdf2)

### `PendingTransactionBuilder` for confirmation budgets
`pending.with_required_confirmations(n).with_timeout(Some(d)).get_receipt().await` is Alloy's canonical wait-for-confirmation pattern. Each builder method returns `Self`; `get_receipt()` consumes the chain.
- **Why:** policy lives in the builder, business logic stays clean. Tune `confirmations` per chain and per asset.
- **Where:** `Submitter::submit_and_confirm` in `crates/telos-submitter/src/lib.rs` (commit 5a97014)

### `receipt.status()` is the EIP-658 success bit
`true` = success, `false` = revert. Pre-Byzantium chains report by post-state hash instead, but Alloy abstracts that. Don't second-guess.
- **Why:** receipts don't carry revert data, only this bit + logs. To get revert *reasons* from a failed receipt, re-call `eth_call` against the same block.
- **Where:** `Submitter::submit_and_confirm` in `crates/telos-submitter/src/lib.rs` (commit 5a97014)

---

## REVM

### `Context::mainnet()` is the modern entry point
`Context::mainnet().with_db(...).build_mainnet()` — the `MainBuilder` and `MainContext` traits gate the chained API; `ExecuteEvm` provides `transact`. Four imports cover the minimum surface for any synchronous simulation.
- **Where:** `simulate_settlement` in `crates/telos-settler/src/lib.rs` (commit c983ed1)

### EIP-8037 split execution gas from state gas
`gas_used()` is now ambiguous and deprecated. Use `tx_gas_used()` for the per-tx total. The split exists for stateless-client pricing of state access.
- **Where:** `decode_leg` in `crates/telos-settler/src/lib.rs` (commit c983ed1)

### `ExecutionResult` is a sum type, not a boolean
Match on `Success { logs, output }`, `Revert { output }`, `Halt { reason }`. The output bytes mean different things in each variant: return data on success, revert payload on revert, nothing on halt.
- **Why:** `is_success() / else fail` collapses the most useful diagnostic. *Why* it failed matters more than that it failed.
- **Where:** `decode_leg`, `scan_transfer_event` in `crates/telos-settler/src/lib.rs` (commits 024e3e9, 03bc090)

### `Context` is mutable across `transact` calls
The hedge tx sees the spot tx's post-state — nonces incremented, balances changed, storage written. This is *the* property that makes REVM useful for sequential simulation.
- **Why:** if you wanted isolated per-tx execution, you'd reset the DB between calls. Sequential `transact` is correct for "what happens if these run in order."
- **Where:** `run_legs` in `crates/telos-settler/src/lib.rs` (commit 03bc090)

### Generic over `DB` for code reuse
`run_legs<DB>` works against `CacheDB<EmptyDB>` and `CacheDB<WrapDatabaseAsync<AlloyDB>>` alike. The trait bound `where DB: Database, DB::Error: std::error::Error + Send + Sync + 'static` is the minimum needed for `eyre`'s `?` to flow through.
- **Where:** `run_legs` in `crates/telos-settler/src/lib.rs` (commit 03bc090)

### The `alloydb` feature is non-default
REVM's umbrella crate hides the Alloy adapter behind `features = ["alloydb"]`. Discovering this is the first lesson in REVM's modular crate split.
- **Where:** `Cargo.toml` workspace deps (commit ea85199)

### Async/sync bridge: `WrapDatabaseAsync` + `spawn_blocking` + `Handle::enter()`
`AlloyDB`'s state-fetch is async, REVM's `Database` trait is sync. `WrapDatabaseAsync::new(...)` calls `block_on` internally, which **deadlocks inside an async worker**. Move the simulation to `tokio::task::spawn_blocking` and call `handle.enter()` on the blocking thread so the wrapped DB submits fetches back to the parent runtime.
- **Why:** without `_enter`, the blocking thread has no current runtime and `WrapDatabaseAsync::new` returns `None`. The implicit thread-local runtime context matters.
- **Where:** `simulate_settlement_forked` in `crates/telos-settler/src/lib.rs` (commit ea85199)

---

## Tokio

### `tokio::select!` arms must be cancel-safe
Every arm uses `StreamExt::next`, which is cancel-safe. If you put a non-cancel-safe future in an arm, you lose data on every loop where the *other* arm fires.
- **Why:** the single most important thing to internalize about `select!`. When in doubt, `tokio::spawn` the work and let `select!` only handle stream demultiplexing.
- **Where:** `watch_both` in `crates/telos-listener/src/lib.rs` (commit ada0751)

### Spawn rather than await in `select!` arms
A 200ms simulation awaited inline blocks the next event. `tokio::spawn(handle_thing(item))` makes per-item work independent — the listener loop stays at sub-ms latency.
- **Why:** trade-off is no backpressure (a slow downstream could spawn unbounded tasks). Real for scale; fine for now.
- **Where:** `spawn_simulation` in `crates/telos-listener/src/lib.rs` (commit ea85199)

### `tokio::sync::RwLock` vs `std::sync::RwLock`
Tokio's lock yields the task and is correct inside async functions. `std::sync::RwLock` blocks the worker thread. `parking_lot::RwLock` works inside async if the critical section is microseconds.
- **Where:** `PriceBook` in `crates/telos-settler/src/lib.rs` (commit 85b5a14)

### Errors vs outcomes in async APIs
Submitter-level errors (invalid RPC, signer error) propagate as `Result::Err`. Transaction-level outcomes (revert, timeout) come back as `Ok(SettlementResult::Failed | Timeout)`.
- **Why:** a revert is *normal operation* — the system did its job, the chain rejected the tx. Forcing callers to `match err.downcast()` to handle expected outcomes is wrong.
- **Where:** `Submitter::submit_and_confirm` in `crates/telos-submitter/src/lib.rs` (commit 5a97014)

---

## API Design

### Hide sync primitives behind a struct
`Arc<RwLock<HashMap<Address, PriceQuote>>>` exposed as a type alias leaks lock semantics to every caller. Hiding it behind methods (`record_fill`, `get`) means you can swap to `parking_lot::RwLock`, an LRU cache, or a sharded map without touching call sites.
- **Where:** `PriceBook` in `crates/telos-settler/src/lib.rs` (commit 85b5a14)

### Per-leg outcomes vs single boolean
`SimulationOutcome { spot, hedge, atomic_success }` lets you see *which* leg blocked atomicity. A bare `bool` collapses information you'll want during debugging — "the hedge would have worked but the spot reverted because the payer's allowance was zero" is a different problem from "the hedge venue address has no bytecode."
- **Where:** `LegOutcome`, `SimulationOutcome` in `crates/telos-settler/src/lib.rs` (commit 03bc090)

### Sum type for decisions
`SettlerDecision::Submit(SubmissionPlan)` carries *what to send*; `SettlerDecision::Reject(RejectReason)` carries *why not*. Both branches log structured information.
- **Why:** a `Result<SubmissionPlan>` would conflate "no quote yet" with "system error" — operationally very different.
- **Where:** `should_submit` in `crates/telos-settler/src/lib.rs` (commit 138bdf2)

### `Option<T>` as soft failure
"No price for the asset yet" isn't an error, it's a state. Returning `Option<RouteQuote>` (not `Result`) makes the absence first-class. The listener logs a warning and proceeds; once a fill arrives, the next intent gets a real quote without retry logic.
- **Where:** `quote_route` in `crates/telos-settler/src/lib.rs` (commit 85b5a14)

### Two gates for risky operations
The submitter's `broadcast: bool` is captured at construction *and* `TELOS_BROADCAST=1` is required at the CLI. Either gate alone keeps you in dry-run.
- **Why:** defense in depth. A misconfigured submitter that silently broadcasts is unrecoverable. The dry-run default should never quietly flip.
- **Where:** `Submitter::new` and `Config::from_env` (commit 138bdf2)

### Compile-time `Send + Sync` guard
```rust
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Submitter>();
};
```
If a future change adds an `Rc` or `RefCell` field, the build breaks at this line. Since the listener clones `Submitter` across async tasks, that property must hold.
- **Why:** catching it at compile time beats discovering it at runtime when a clone fails.
- **Where:** bottom of `crates/telos-submitter/src/lib.rs` (commits 138bdf2, 5a97014)

### Single source of truth for the ABI
Both the simulator and the submitter need to decode `OrderPlaced`. Promoting `mod abi` to `pub mod abi` is the cleanest way to share — though `abi` becomes part of the crate's public surface and breaking changes there ripple.
- **Where:** `crates/telos-settler/src/lib.rs` (commit 5a97014)

---

## Architecture

### Mode dispatch from env
Clean separation of `Config::from_env()` (parsing) from `Mode` (intent). Same pattern you'd use in production with clap or a config file.
- **Where:** `Config` and `Mode` in `crates/telos-cli/src/main.rs` (commit ada0751)

### The feedback-loop shape
HL fills update the `PriceBook`; intents read it. Same task pool, shared state, no channels yet. Channels become necessary when writer and reader run on different processes or the writer needs backpressure.
- **Why:** worth knowing when *not* to reach for channels. Premature channelization adds overhead and complexity for problems you don't have.
- **Where:** `watch_both`, `spawn_simulation` in `crates/telos-listener/src/lib.rs` (commit 85b5a14)

### Inline vs worker placement
The simulation runs in the listener's read loop *for now* because it's microseconds. When fork-from-RPC arrives it becomes milliseconds and needs `tokio::spawn`. The decision deserves a comment at the call site.
- **Where:** `spawn_simulation` in `crates/telos-listener/src/lib.rs` (commits c983ed1, ea85199)

### Telos signs only what Telos owns
The spot leg is the merchant's settlement, initiated by the payer on Tempo — observed for *gating* but not broadcast by Telos. The hedge is the only tx Telos signs. A bundler/Permit2 design that pulls tokens from the payer would change that scope.
- **Why:** scope decisions about *who signs what* are architectural, not implementation details. They constrain the trust model and the regulatory surface.
- **Where:** doc-comment on `SubmissionPlan` in `crates/telos-settler/src/lib.rs` (commit 138bdf2)

### Mock interfaces for development
`IHyperliquidGateway.placeShort` will never exist on real HL (HL routes through L1 actions via CoreWriter precompiles). For *simulation* the settler only needs a known selector to encode and a known event signature to detect. Real-HL plumbing comes later; this lets the rest of the architecture solidify first.
- **Where:** `crates/telos-settler/src/abi.rs` (commit 03bc090)

---

## Chronological index

| Week | Commit | Subject |
|------|--------|---------|
| 1 | [3ba07dd](https://github.com/psyto/telos/commit/3ba07dd) | Cargo workspace scaffold + Alloy block headers |
| 1 | [b31d22e](https://github.com/psyto/telos/commit/b31d22e) | `sol!` macro + typed PaymentIntent subscription |
| 2 | [ada0751](https://github.com/psyto/telos/commit/ada0751) | Multiplex Tempo + HL via `tokio::select!` |
| 3 | [c983ed1](https://github.com/psyto/telos/commit/c983ed1) | REVM in-memory simulation harness |
| 3 | [ea85199](https://github.com/psyto/telos/commit/ea85199) | Fork-from-RPC via `AlloyDB` + `WrapDatabaseAsync` |
| 4 | [024e3e9](https://github.com/psyto/telos/commit/024e3e9) | `IERC20.transfer` calldata + revert decoding |
| 5 | [85b5a14](https://github.com/psyto/telos/commit/85b5a14) | `PriceBook` feedback loop for hedge sizing |
| 6 | [03bc090](https://github.com/psyto/telos/commit/03bc090) | Perp hedge as a second sequential leg |
| 7 | [138bdf2](https://github.com/psyto/telos/commit/138bdf2) | `SettlerDecision` + dry-run-default `Submitter` |
| 8 | [5a97014](https://github.com/psyto/telos/commit/5a97014) | `submit_and_confirm` + `SettlementResult` |
