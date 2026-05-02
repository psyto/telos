//! Solidity ABI bindings used during simulation.
//!
//! IERC20 covers what the settler needs to encode the merchant settlement —
//! the `transfer` call — and to recognise the `Transfer` event when the
//! simulated tx succeeds.

use alloy::sol;

sol! {
    #[derive(Debug)]
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
        event Transfer(address indexed from, address indexed to, uint256 value);
    }

    /// Mock HL builder/bridge contract: a single function that places a perp
    /// short on L1 via the CoreWriter precompile and returns the order id.
    /// Real HL gateway will look similar; the simulation only needs the
    /// selector + arg layout to encode and the OrderPlaced event to detect.
    #[derive(Debug)]
    interface IHyperliquidGateway {
        function placeShort(address asset, uint256 size, uint16 maxSlippageBps) external returns (bytes32 orderId);
        event OrderPlaced(bytes32 indexed orderId, address indexed asset, uint256 size, uint16 maxSlippageBps);
    }
}
