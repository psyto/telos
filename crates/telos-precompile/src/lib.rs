//! Telos precompile — Week 10 Reth/REVM extension.
//!
//! A precompile is native Rust that lives at a fixed EVM address and runs
//! when a tx targets that address. It bypasses the EVM interpreter, so
//! you pay for native CPU rather than per-opcode gas. Existing Ethereum
//! precompiles (ecrecover, sha256, modexp, …) live at addresses
//! 0x01–0x0a; custom L1s and L2s extend the set with their own.
//!
//! [`intent_digest`] is the Telos primitive for committing canonical
//! intent identity on-chain. A bundler or settlement contract calls it
//! with the ABI-encoded intent fields and receives a 32-byte digest it
//! can use as a stable key. Same fields → same digest, regardless of how
//! the caller serialised them, because the precompile owns the
//! canonicalisation.
//!
//! This crate stays node-agnostic: it exports the precompile function
//! and its address constant. Wiring it into a real Reth node is a
//! configuration step on the node side; here we provide the building
//! block and unit tests that exercise it directly.

use alloy::primitives::{Address, Bytes, address, keccak256};
use revm::context::Cfg;
use revm::context_interface::ContextTr;
use revm::handler::{EthPrecompiles, PrecompileProvider};
use revm::interpreter::interpreter_action::CallInputs;
use revm::interpreter::{Gas, InstructionResult, InterpreterResult};
use revm::precompile::{EthPrecompileOutput, EthPrecompileResult, PrecompileHalt};
use revm::primitives::hardfork::SpecId;

/// Address the precompile lives at. Convention: Telos-namespace precompiles
/// occupy 0x…0901 onwards, well clear of the EIP-allocated 0x01–0x0a band.
pub const TELOS_INTENT_DIGEST_ADDRESS: Address =
    address!("0000000000000000000000000000000000000901");

/// Canonical input layout: ABI-encoded `(payer, merchant, asset, amount, deadline)`.
/// Five 32-byte slots = 160 bytes. Addresses occupy the low 20 bytes of their
/// slot; `uint256` and `uint64` are big-endian and right-aligned.
pub const INTENT_DIGEST_INPUT_LEN: usize = 32 * 5;

/// Gas schedule: a fixed base plus a small per-byte cost. Mirrors the shape
/// of the SHA-256 precompile so callers reason about cost the same way.
pub const INTENT_DIGEST_BASE_GAS: u64 = 500;
pub const INTENT_DIGEST_PER_BYTE_GAS: u64 = 25;

/// Compute the canonical Telos intent digest.
///
/// Eth-style precompile signature: takes raw input bytes plus a gas limit,
/// returns either an [`EthPrecompileOutput`] (gas used + 32-byte digest) or
/// a [`PrecompileHalt`] for non-fatal failures. Out-of-gas and wrong-length
/// inputs are halts, not Rust errors — they unwind the EVM call cleanly,
/// not the host process.
///
/// Wrong-length input is rejected rather than zero-padded so a caller cannot
/// hash truncated data and silently get back a "valid" digest.
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

/// Custom [`PrecompileProvider`] that adds [`intent_digest`] to the standard
/// Ethereum set. Cloneable so it can be slotted into the EVM builder once;
/// `set_spec` and `run` mutate through the wrapped `EthPrecompiles` for every
/// non-Telos address.
#[derive(Debug, Clone)]
pub struct TelosPrecompiles {
    eth: EthPrecompiles,
}

impl TelosPrecompiles {
    pub fn new(spec: SpecId) -> Self {
        Self { eth: EthPrecompiles::new(spec) }
    }
}

impl<CTX: ContextTr> PrecompileProvider<CTX> for TelosPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        <EthPrecompiles as PrecompileProvider<CTX>>::set_spec(&mut self.eth, spec)
    }

    fn run(
        &mut self,
        ctx: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        if inputs.bytecode_address != TELOS_INTENT_DIGEST_ADDRESS {
            return self.eth.run(ctx, inputs);
        }

        // Materialise the calldata. as_bytes() handles both inline-bytes and
        // shared-buffer forms transparently.
        let bytes_ref = inputs.input.as_bytes(ctx);
        let eth_result = intent_digest(&bytes_ref, inputs.gas_limit);

        Ok(Some(eth_result_to_interpreter(eth_result, inputs.gas_limit)))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        let mut addresses: Vec<Address> =
            <EthPrecompiles as PrecompileProvider<CTX>>::warm_addresses(&self.eth).collect();
        addresses.push(TELOS_INTENT_DIGEST_ADDRESS);
        Box::new(addresses.into_iter())
    }

    fn contains(&self, address: &Address) -> bool {
        *address == TELOS_INTENT_DIGEST_ADDRESS
            || <EthPrecompiles as PrecompileProvider<CTX>>::contains(&self.eth, address)
    }
}

/// Convert an Eth-style precompile result into the [`InterpreterResult`] shape
/// that EVM frame execution expects. Mirrors the behaviour the stock
/// EthPrecompiles uses internally so callers see consistent gas accounting.
fn eth_result_to_interpreter(
    result: EthPrecompileResult,
    gas_limit: u64,
) -> InterpreterResult {
    let mut gas = Gas::new(gas_limit);
    match result {
        Ok(out) => {
            let _ = gas.record_regular_cost(out.gas_used);
            InterpreterResult {
                result: InstructionResult::Return,
                gas,
                output: out.bytes,
            }
        }
        Err(PrecompileHalt::OutOfGas) => {
            gas.spend_all();
            InterpreterResult {
                result: InstructionResult::PrecompileOOG,
                gas,
                output: Bytes::new(),
            }
        }
        Err(_other) => {
            gas.spend_all();
            InterpreterResult {
                result: InstructionResult::PrecompileError,
                gas,
                output: Bytes::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::TxEnv;
    use revm::context::result::{ExecutionResult, Output};
    use revm::database::{CacheDB, EmptyDB};
    use revm::handler::instructions::EthInstructions;
    use revm::primitives::{TxKind, U256};
    use revm::{Context, ExecuteEvm, MainContext};

    fn sample_input() -> Vec<u8> {
        let mut buf = vec![0u8; INTENT_DIGEST_INPUT_LEN];
        // Slot 0: payer = 0xbe…be (20 bytes) right-aligned in 32-byte slot.
        buf[12..32].copy_from_slice(&[0xbe; 20]);
        // Slot 1: merchant = 0xfe…fe.
        buf[12 + 32..32 + 32].copy_from_slice(&[0xfe; 20]);
        // Slot 2: asset = 0xca…ca.
        buf[12 + 64..32 + 64].copy_from_slice(&[0xca; 20]);
        // Slot 3: amount = 1e18 big-endian in low 16 bytes (uint128 fits the slot tail).
        let amount: u128 = 1_000_000_000_000_000_000;
        buf[32 + 96 - 16..32 + 96].copy_from_slice(&amount.to_be_bytes());
        // Slot 4: deadline = 1_700_000_000 big-endian in low 8 bytes.
        let deadline: u64 = 1_700_000_000;
        buf[32 + 128 - 8..32 + 128].copy_from_slice(&deadline.to_be_bytes());
        buf
    }

    #[test]
    fn happy_path_matches_offchain_keccak() {
        let input = sample_input();
        let result = intent_digest(&input, 100_000).expect("should succeed");

        let expected = keccak256(input.as_slice());
        assert_eq!(result.bytes.as_ref(), expected.as_slice());
        assert_eq!(
            result.gas_used,
            INTENT_DIGEST_BASE_GAS + (input.len() as u64) * INTENT_DIGEST_PER_BYTE_GAS,
        );
    }

    #[test]
    fn out_of_gas_halts_before_hashing() {
        let input = sample_input();
        let stingy = INTENT_DIGEST_BASE_GAS - 1;
        let halt = intent_digest(&input, stingy).unwrap_err();
        assert!(matches!(halt, PrecompileHalt::OutOfGas));
    }

    #[test]
    fn wrong_length_input_halts_with_message() {
        let input = vec![0u8; INTENT_DIGEST_INPUT_LEN - 1];
        let halt = intent_digest(&input, 100_000).unwrap_err();
        match halt {
            PrecompileHalt::Other(msg) => assert!(msg.contains("expected")),
            other => panic!("unexpected halt: {other:?}"),
        }
    }

    #[test]
    fn determinism_same_input_same_digest() {
        let a = intent_digest(&sample_input(), 100_000).unwrap();
        let b = intent_digest(&sample_input(), 100_000).unwrap();
        assert_eq!(a.bytes, b.bytes);
    }

    /// Full integration: build a REVM with TelosPrecompiles, send a tx that
    /// targets the precompile address, assert the call output matches the
    /// off-chain keccak. This is what proves the registration plumbing works.
    #[test]
    fn evm_call_returns_canonical_digest() {
        let ctx = Context::mainnet().with_db(CacheDB::<EmptyDB>::default());
        let spec = *ctx.cfg.spec();
        let mut evm = revm::context::Evm::new(
            ctx,
            EthInstructions::new_mainnet_with_spec(spec),
            TelosPrecompiles::new(spec),
        );

        let calldata = sample_input();
        let tx = TxEnv {
            caller: Address::ZERO,
            kind: TxKind::Call(TELOS_INTENT_DIGEST_ADDRESS),
            value: U256::ZERO,
            data: Bytes::from(calldata.clone()),
            gas_limit: 100_000,
            gas_price: 0,
            ..Default::default()
        };

        let result = evm.transact(tx).expect("transact succeeds").result;
        let output = match result {
            ExecutionResult::Success { output: Output::Call(b), .. } => b,
            other => panic!("expected Success, got {other:?}"),
        };

        let expected = keccak256(calldata.as_slice());
        assert_eq!(output.as_ref(), expected.as_slice());
    }
}
