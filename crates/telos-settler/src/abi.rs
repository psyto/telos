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
}
