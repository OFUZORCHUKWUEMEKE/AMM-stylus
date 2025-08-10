#![cfg_attr(not(any(test, feature = "export-abi")), no_main)]
#![cfg_attr(not(any(test, feature = "export-abi")), no_std)]

#[macro_use]
extern crate alloc;

use core::hash;

use alloc::vec::Vec;

/// Import items from the SDK. The prelude contains common traits and macros.
use alloy_primitives::{aliases::U24, Address, FixedBytes, U256};
use alloy_sol_types::{abi::token, sol, SolValue};
use stylus_sdk::{crypto::keccak, prelude::*};

sol_interface! {
    interface IERC20{
        function transferFrom(address from , address to , uint256 value) external returns(bool);

        function transfer(address to , uint256 value) external returns(bool);
    }
}

// Define some persistent storage using the Solidity ABI.
// `Counter` will be the entrypoint.
sol_storage! {
    #[entrypoint]
    pub struct StylusSwap {
        mapping(bytes32 => Pool) pools;
    }

    pub struct Pool{
        address token0;
        address token1;
        uint24 fee;
        uint256 liquidity;
        uint256 balance0;
        uint256 balance1;
        mapping(bytes32 => Position) positions;
    }

    pub struct Position{
        address owner;
        uint256 liquidity;
    }
}

sol! {
    error PoolAlreadyExists(bytes32 pool_id);

    error PoolDoesNotExist(bytes32 pool_id);

    error InsufficientLiquidityMinted();

    error InsufficientAmount();

    error InsufficientLiquidityOwned();

    error FailedOrInsufficientTokenTransfer(address token, address from , address to , uint256 amount);

    error FailedToReturnExtraEth(address to , uint256 amount);

    error TooMuchSlippage();

    event PoolCreated(bytes32 pool_id,address token0,address token1, uint24 fee);

    event LiquidityMinted(bytes32 pool_id,address owner,uint256 liquidity);

    event LiquidityBurned(bytes32 pool_id,address owner,uint256 liquidity);

     event Swap(bytes32 pool_id, address user, uint256 input_amount, uint256 output_amount_after_fees, uint256 fees, bool zero_for_one);
}

#[derive(SolidityError)]
pub enum StylusSwapError {
    PoolAlreadyExists(PoolAlreadyExists),
    PoolDoesNotExist(PoolDoesNotExist),
    InsufficientAmount(InsufficientAmount),
    InsufficientLiquidityMinted(InsufficientLiquidityMinted),
    InsufficientLiquidityOwned(InsufficientLiquidityOwned),
    FailedOrInsufficientTokenTransfer(FailedOrInsufficientTokenTransfer),
    TooMuchSlippage(TooMuchSlippage),
}

#[public]
impl StylusSwap {
    pub fn create_pool(
        &mut self,
        token_a: Address,
        token_b: Address,
        fee: U24,
    ) -> Result<(), StylusSwapError> {
        let (pool_id, token0, token1) = self.get_pool_id(token_a, token_b, fee);
        let existing_pool = self.pools.get(pool_id);

        if !existing_pool.token0.get().is_zero() || !existing_pool.token1.get().is_zero() {
            return Err(StylusSwapError::PoolAlreadyExists(PoolAlreadyExists {
                pool_id: pool_id,
            }));
        }
        let mut pool_setter = self.pools.setter(pool_id);
        pool_setter.token0.set(token0);
        pool_setter.token1.set(token1);
        pool_setter.fee.set(fee);

        pool_setter.liquidity.set(U256::from(0));
        pool_setter.balance0.set(U256::from(0));
        pool_setter.balance1.set(U256::from(0));

        log(
            self.vm(),
            PoolCreated {
                pool_id,
                token0,
                token1,
                fee,
            },
        );
        Ok(())
    }

    #[payable]
    pub fn add_liquidity(
        &mut self,
        pool_id: FixedBytes<32>,
        amount_0_desired: U256,
        amount_1_desired: U256,
        amount_0_min: U256,
        amount_1_min: U256,
    ) -> Result<(), StylusSwapError> {
        let msg_sender = self.vm().msg_sender();
        let address_this = self.vm().contract_address();
        // Load the pools current state
        let pool = self.pools.get(pool_id);
        let token0 = pool.token0.get();
        let token1 = pool.token1.get();

        if token0.is_zero() && token1.is_zero() {
            return Err(StylusSwapError::PoolDoesNotExist(PoolDoesNotExist {
                pool_id,
            }));
        }
        let balance_0 = pool.balance0.get();
        let balance_1 = pool.balance1.get();
        let liquidity = pool.liquidity.get();
        let is_initial_liquidity = liquidity.is_zero();

        let position_id = self.get_position_id(pool_id, msg_sender);
        let user_position = pool.positions.get(position_id);

        let user_liquidity = user_position.liquidity.get();

        let (amount0, amount1) = self.get_liquidity_amounts(
            amount_0_desired,
            amount_1_desired,
            amount_0_min,
            amount_1_min,
            balance_0,
            balance_1,
        )?;

        let new_user_liquidity = if is_initial_liquidity {
            self.interger_sqrt(amount0 * amount1) - U256::from(1000)
        } else {
            let l_0 = (amount0 * liquidity) / balance_0;
            let l_1 = (amount1 * liquidity) / balance_1;
            self.min(l_0, l_1)
        };

        let new_pool_liquidity = if is_initial_liquidity {
            new_user_liquidity + U256::from(1000)
        } else {
            new_user_liquidity
        };
        if new_pool_liquidity.is_zero() {
            return Err(StylusSwapError::InsufficientLiquidityMinted(
                InsufficientLiquidityMinted {},
            ));
        }

        let mut pool_setter = self.pools.setter(pool_id);
        pool_setter.liquidity.set(liquidity + new_pool_liquidity);
        pool_setter.balance0.set(balance_0 + amount0);
        pool_setter.balance1.set(balance_1 + amount1);

        let mut user_position_setter = pool_setter.positions.setter(position_id);
        user_position_setter
            .liquidity
            .set(user_liquidity + new_user_liquidity);

        self.try_transfer_token(token0, msg_sender, address_this, amount0)?;

        self.try_transfer_token(token1, msg_sender, address_this, amount1)?;

        log(
            self.vm(),
            LiquidityMinted {
                pool_id,
                owner: msg_sender,
                liquidity: new_pool_liquidity,
            },
        );
        Ok(())
    }


    pub fn get_pool_id(
        &self,
        token_a: Address,
        token_b: Address,
        fee: U24,
    ) -> (FixedBytes<32>, Address, Address) {
        let token0: Address;
        let token1: Address;

        if token_a <= token_b {
            token0 = token_a;
            token1 = token_b;
        } else {
            token0 = token_b;
            token1 = token_a;
        }
        let hash_data = (token0, token1, fee);
        let pool_id = keccak(hash_data.abi_encode_sequence());
        (pool_id, token0, token1)
        // sort the tokens to ensure determinism
    }

    pub fn get_position_id(&self, pool_id: FixedBytes<32>, owner: Address) -> FixedBytes<32> {
        let hash_data = (pool_id, owner);
        let position_id = keccak(hash_data.abi_encode_sequence());
        position_id
    }

    pub fn get_position_liquidity(&self, pool_id: FixedBytes<32>, owner: Address) -> U256 {
        let position_id = self.get_position_id(pool_id, owner);
        let pool = self.pools.get(pool_id);
        let position = pool.positions.get(position_id);
        position.liquidity.get()
    }

    pub fn get_liquidity_amounts(
        &self,
        amount_0_desired: U256,
        amount_1_desired: U256,
        amount_0_min: U256,
        amount_1_min: U256,
        balance_0: U256,
        balance_1: U256,
    ) -> Result<(U256, U256), StylusSwapError> {
        if balance_0.eq(&U256::from(0)) && balance_1.eq(&U256::ZERO) {
            return Ok((amount_0_desired, amount_1_desired));
        }
        let amount_1_optimal = (amount_0_desired * balance_1) / balance_0;
        if amount_1_optimal <= amount_1_desired {
            if amount_1_optimal < amount_1_min {
                return Err(StylusSwapError::InsufficientAmount(InsufficientAmount {}));
            }
            return Ok((amount_0_desired, amount_1_optimal));
        }
        let amount_0_optimal = (amount_1_desired * balance_0) / balance_1;

        if amount_0_optimal < amount_0_desired {
            return Err(StylusSwapError::InsufficientAmount(InsufficientAmount {}));
        }
        Ok((amount_0_optimal, amount_1_desired))
    }

    // This function is used to remove liquidity from a pool. It takes in the pool ID and the
    // amount of liquidity to remove.
    // It returns an error if the pool does not exist, if the user's liquidity is insufficient,
    // or if we fail to transfer the tokens to the user.
    pub fn remove_liquidity(
        &mut self,
        pool_id: FixedBytes<32>,
        liquidity_to_remove: U256,
    ) -> Result<(), StylusSwapError> {
        let msg_sender = self.vm().msg_sender();
        let address_this = self.vm().contract_address();

        // Load the pool's current state
        let pool = self.pools.get(pool_id);
        let token0 = pool.token0.get();
        let token1 = pool.token1.get();

        // If both token addresses are zero, this pool is not initialized and does not exist
        if token0.is_zero() && token1.is_zero() {
            return Err(StylusSwapError::PoolDoesNotExist(PoolDoesNotExist {
                pool_id,
            }));
        }

        let balance_0 = pool.balance0.get();
        let balance_1 = pool.balance1.get();
        let liquidity = pool.liquidity.get();

        // Load the user's current position in the pool (default zero if they don't have one)
        let position_id = self.get_position_id(pool_id, msg_sender);
        let user_position = pool.positions.get(position_id);
        let user_liquidity = user_position.liquidity.get();

        if liquidity_to_remove > user_liquidity {
            return Err(StylusSwapError::InsufficientLiquidityOwned(
                InsufficientLiquidityOwned {},
            ));
        }

        // The amount of tokens to be removed is the % share of the pool's balance of each token
        // based on the user's share of the pool's liquidity
        // e.g. If user owns 10% of the pool's total liquidity, they will receive 10% of the pool's
        // token0 balance, and 10% of the pool's token1 balance
        let amount_0 = (balance_0 * liquidity_to_remove) / liquidity;
        let amount_1 = (balance_1 * liquidity_to_remove) / liquidity;

        if amount_0.is_zero() || amount_1.is_zero() {
            return Err(StylusSwapError::InsufficientLiquidityOwned(
                InsufficientLiquidityOwned {},
            ));
        }

        let mut pool_setter = self.pools.setter(pool_id);
        pool_setter.liquidity.set(liquidity - liquidity_to_remove);
        pool_setter.balance0.set(balance_0 - amount_0);
        pool_setter.balance1.set(balance_1 - amount_1);
        let mut position_setter = pool_setter.positions.setter(position_id);
        position_setter
            .liquidity
            .set(user_liquidity - liquidity_to_remove);

        // Transfer amount0 of token0 and amount1 of token1 to the user
        self.try_transfer_token(token0, address_this, msg_sender, amount_0)?;
        self.try_transfer_token(token1, address_this, msg_sender, amount_1)?;

        // Emit the LiquidityBurned event
        log(
            self.vm(),
            LiquidityBurned {
                pool_id,
                owner: msg_sender,
                liquidity: liquidity_to_remove,
            },
        );

        Ok(())
    }

    #[payable]
    pub fn swap(
        &mut self,
        pool_id: FixedBytes<32>,
        input_amount: U256,
        min_output_amount: U256,
        zero_for_one: bool,
    ) -> Result<(), StylusSwapError> {
        if input_amount.is_zero() {
            return Err(StylusSwapError::InsufficientAmount(InsufficientAmount {}));
        }

        let msg_sender = self.vm().msg_sender();
        let address_this = self.vm().contract_address();

        let pool = self.pools.get(pool_id);
        let token0 = pool.token0.get();
        let token1 = pool.token1.get();

        if token0.is_zero() && token1.is_zero() {
            return Err(StylusSwapError::PoolDoesNotExist(PoolDoesNotExist {
                pool_id,
            }));
        }
        let balance0 = pool.balance0.get();
        let balance1 = pool.balance1.get();
        let fee = pool.fee.get();

        let original_k = balance0 * balance1;

        let input_token: Address = if zero_for_one { token0 } else { token1 };

        let output_token = if zero_for_one { token1 } else { token0 };
        let input_balance = if zero_for_one { balance0 } else { balance1 };
        let output_balance = if zero_for_one { balance1 } else { balance0 };

        let output_amount = output_balance - (original_k / (input_balance + input_amount));

        // Now we apply swap fees on the output amount so LPs earn some yield for providing liquidity
        // First, we calculate the amount of fees to deduct
        let fees = (output_amount * U256::from(fee)) / U256::from(10_000);
        // Then, we calculate how much output amount the user will get after fees
        let output_amount_after_fees = output_amount - fees;

        if output_amount_after_fees < min_output_amount {
            return Err(StylusSwapError::TooMuchSlippage(TooMuchSlippage {}));
        }
        let mut pool_setter = self.pools.setter(pool_id);
        if zero_for_one {
            pool_setter.balance0.set(balance0 + input_amount);
            pool_setter
                .balance1
                .set(balance1 - output_amount_after_fees);
        } else {
            pool_setter
                .balance0
                .set(balance0 - output_amount_after_fees);
            pool_setter.balance1.set(balance1 + input_amount);
        }
        // Transfer the input token from user to pool
        self.try_transfer_token(input_token, msg_sender, address_this, input_amount)?;
        // Transfer the output token from pool to user
        self.try_transfer_token(
            output_token,
            address_this,
            msg_sender,
            output_amount_after_fees,
        )?;

        // Emit the Swap event
        log(
            self.vm(),
            Swap {
                pool_id,
                user: msg_sender,
                input_amount,
                output_amount_after_fees,
                fees,
                zero_for_one,
            },
        );
        Ok(())
    }
}

impl StylusSwap {
    // private functions in contracts
    fn interger_sqrt(&self, x: U256) -> U256 {
        let two = U256::from(2);
        let mut z: U256 = (x + U256::from(1)) >> 1;
        let mut y = x;

        while z < y {
            y = z;
            z = (x / z + z) / two;
        }
        y
    }

    fn min(&self, x: U256, y: U256) -> U256 {
        if x < y {
            return x;
        }
        y
    }

    fn try_transfer_token(
        &mut self,
        token: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<(), StylusSwapError> {
        let address_this = self.vm().contract_address();
        if from != address_this && to != address_this {
            // We are transferring tokens between two addresses where we are neither the sender nor the receiver
            return Err(StylusSwapError::FailedOrInsufficientTokenTransfer(
                FailedOrInsufficientTokenTransfer {
                    token,
                    from,
                    to,
                    amount,
                },
            ));
        }
        // We are transferring ETH
        if token.is_zero() {
            if from == address_this {
                // We are sending ETH out
                let result = self.vm().transfer_eth(to, amount);
                if result.is_err() {
                    return Err(StylusSwapError::FailedOrInsufficientTokenTransfer(
                        FailedOrInsufficientTokenTransfer {
                            token,
                            from,
                            to,
                            amount,
                        },
                    ));
                }
            } else if to == address_this {
                // We are receiving ETH
                if self.vm().msg_value() < amount {
                    return Err(StylusSwapError::FailedOrInsufficientTokenTransfer(
                        FailedOrInsufficientTokenTransfer {
                            token,
                            from,
                            to,
                            amount,
                        },
                    ));
                }
                // Refund any excess ETH back to the sender
                let extra_eth = self.vm().msg_value() - amount;
                if extra_eth > U256::ZERO {
                    self.try_transfer_token(token, address_this, from, extra_eth)?;
                }
            }
        }
        // We are transferring an ERC-20 token
        else {
            let token_contract = IERC20::new(token);
            if from == address_this {
                // We are sending the token out
                let result = token_contract.transfer(&mut *self, to, amount);
                if result.is_err() || result.unwrap() == false {
                    return Err(StylusSwapError::FailedOrInsufficientTokenTransfer(
                        FailedOrInsufficientTokenTransfer {
                            token,
                            from,
                            to,
                            amount,
                        },
                    ));
                }
            } else if to == address_this {
                // We are receiving the token
                let result = token_contract.transfer_from(&mut *self, from, to, amount);
                if result.is_err() || result.unwrap() == false {
                    return Err(StylusSwapError::FailedOrInsufficientTokenTransfer(
                        FailedOrInsufficientTokenTransfer {
                            token,
                            from,
                            to,
                            amount,
                        },
                    ));
                }
            }
        }
        Ok(())
    }
}
