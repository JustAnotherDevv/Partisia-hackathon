//! lending

#[macro_use]
extern crate pbc_contract_codegen;
extern crate core;

use pbc_contract_common::address::Address;
use pbc_contract_common::context::{CallbackContext, ContractContext};
use pbc_contract_common::events::EventGroup;
use std::ops::RangeInclusive;

use defi_common::interact_mpc20;
use defi_common::liquidity_util::calculate_swap_to_amount;
use defi_common::math::u128_sqrt;
pub use defi_common::token_balances::Token;
use defi_common::token_balances::{TokenBalances, TokensInOut};
use pbc_contract_common::sorted_vec_map::SortedVecMap;
use std::ops::{Add, Sub};

/// |||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||
///                         CONSTANTS
/// |||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||

pub type TokenAmount = u128;

#[state]
pub struct ContractState {
    owner: Address,
    fee: u128,
    collected_fees: u128,
    pub liquidity_pool_address: Address,
    pub token_balances: TokenBalances,
    pub supplied_collateral: SortedVecMap<Address, u128>,
    pub borrowed_liquidity: SortedVecMap<Address, u128>,
}

trait BalanceMap<K: Ord, V> {
    /// Insert into the map if `value` is not zero.
    /// Removes the key from the map if `value` is zero.
    ///
    /// ## Arguments
    ///
    /// * `key`: Key for map.
    ///
    /// * `value`: The balance value to insert.
    fn insert_balance(&mut self, key: K, value: V);
}

impl<V: Sub<V, Output = V> + PartialEq + Copy> BalanceMap<Address, V> for SortedVecMap<Address, V> {
    #[allow(clippy::eq_op)]
    fn insert_balance(&mut self, key: Address, value: V) {
        let zero = value - value;
        if value == zero {
            self.remove(&key);
        } else {
            self.insert(key, value);
        }
    }
}

impl ContractState {
    fn contract_pools_have_liquidity(&self) -> bool {
        let contract_token_balance = self
            .token_balances
            .get_balance_for(&self.liquidity_pool_address);
        contract_token_balance.a_tokens != 0 && contract_token_balance.b_tokens != 0
    }
}

#[init]
fn initialize(
    ctx: ContractContext,
    initial_fee_value: u128,
    token_a_address: Address,
    token_b_address: Address,
) -> (ContractState, Vec<EventGroup>) {

    let token_balances =
        TokenBalances::new(ctx.contract_address, token_a_address, token_b_address).unwrap();

    let mut supplied_collateral = SortedVecMap::new();
    let mut borrowed_liquidity = SortedVecMap::new();

    let state = ContractState {
        owner: ctx.sender,
        fee: initial_fee_value,
        liquidity_pool_address: ctx.contract_address,
        token_balances,
        supplied_collateral,
        borrowed_liquidity,
        collected_fees: 0,
    };

    (state, vec![])
}

/// |||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||
///         Main lending functions: borrow, repay, liquidate
/// |||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||

#[action(shortname = 0x02)]
pub fn borrow(
    ctx: ContractContext,
    mut state: ContractState,
    collateral_amount: TokenAmount,
    borrowed_amount: TokenAmount,
    token_in: Address,
) -> (ContractState, Vec<EventGroup>) {
    assert!(
        state.contract_pools_have_liquidity(),
        "Pools must have existing liquidity to borrow"
    );
    assert!(
        collateral_amount >= (borrowed_amount * 2),
        "Not enough collateral was supplied for borrowed amount"
    );

    let tokens = state.token_balances.deduce_tokens_in_out(token_in);

    let mut current_collateral = state.supplied_collateral.get(&ctx.sender).copied().unwrap_or(0);

    assert!(
        current_collateral == 0,
        "Can't borrow before repaying"
    );
    
    state.supplied_collateral.insert_balance(ctx.sender, current_collateral.add(collateral_amount));
    state.borrowed_liquidity.insert_balance(ctx.sender, borrowed_amount);

    state.token_balances.move_tokens(
        ctx.sender,
        state.liquidity_pool_address,
        tokens.token_in,
        collateral_amount,
    );
    state.token_balances.move_tokens(
        state.liquidity_pool_address,
        ctx.sender,
        tokens.token_out,
        borrowed_amount,
    );
    (state, vec![])
}

#[action(shortname = 0x08)]
pub fn repay(
    ctx: ContractContext,
    mut state: ContractState,
    token_in: Address,
) -> (ContractState, Vec<EventGroup>) {

    let tokens = state.token_balances.deduce_tokens_in_out(token_in);

    let mut current_collateral = state.supplied_collateral.get(&ctx.sender).copied().unwrap_or(0);
    let mut current_borrowed= state.borrowed_liquidity.get(&ctx.sender).copied().unwrap_or(0);

    assert!(
        current_collateral > 0,
        "Can't repay if collateral is 0"
    );
    
    state.supplied_collateral.insert_balance(ctx.sender, 0);
    state.borrowed_liquidity.insert_balance(ctx.sender, 0);
    state.collected_fees += state.fee;

    state.token_balances.move_tokens(
        ctx.sender,
        state.liquidity_pool_address,
        tokens.token_in,
        current_borrowed,
    );
    state.token_balances.move_tokens(
        state.liquidity_pool_address,
        ctx.sender,
        tokens.token_out,
        current_collateral - state.fee,
    );
    (state, vec![])
}

// ToDo -> Liquidation based on the Oracles implementation
// Starting overcollateralization of account is 200%
// Liquidation can occur if the overcollateralization of the account is below or at 120%
// Liquidation can be triggered by anyone
// Liquidator supplies borrowed collateral and pockets the supplied collateral
// standard fee is left as profit in the smart contract

/// |||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||
///         Liquidity providing, deposit, withdrawals
/// |||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||||

#[action(shortname = 0x01)]
pub fn deposit(
    context: ContractContext,
    state: ContractState,
    token_address: Address,
    amount: TokenAmount,
) -> (ContractState, Vec<EventGroup>) {
    let tokens = state.token_balances.deduce_tokens_in_out(token_address);

    let mut event_group_builder = EventGroup::builder();
    interact_mpc20::MPC20Contract::at_address(token_address).transfer_from(
        &mut event_group_builder,
        &context.sender,
        &state.liquidity_pool_address,
        amount,
    );

    event_group_builder
        .with_callback(SHORTNAME_DEPOSIT_CALLBACK)
        .argument(tokens.token_in)
        .argument(amount)
        .done();

    (state, vec![event_group_builder.build()])
}

#[callback(shortname = 0x10)]
pub fn deposit_callback(
    context: ContractContext,
    callback_context: CallbackContext,
    mut state: ContractState,
    token: Token,
    amount: TokenAmount,
) -> (ContractState, Vec<EventGroup>) {
    assert!(callback_context.success, "Transfer did not succeed");

    state
        .token_balances
        .add_to_token_balance(context.sender, token, amount);

    (state, vec![])
}

#[action(shortname = 0x03)]
pub fn withdraw(
    context: ContractContext,
    mut state: ContractState,
    token_address: Address,
    amount: TokenAmount,
    wait_for_callback: bool,
) -> (ContractState, Vec<EventGroup>) {
    let tokens = state.token_balances.deduce_tokens_in_out(token_address);

    state
        .token_balances
        .deduct_from_token_balance(context.sender, tokens.token_in, amount);

    let mut event_group_builder = EventGroup::builder();
    interact_mpc20::MPC20Contract::at_address(token_address).transfer(
        &mut event_group_builder,
        &context.sender,
        amount,
    );

    if wait_for_callback {
        event_group_builder
            .with_callback(SHORTNAME_WAIT_WITHDRAW_CALLBACK)
            .done();
    }

    (state, vec![event_group_builder.build()])
}

#[callback(shortname = 0x15)]
fn wait_withdraw_callback(
    _context: ContractContext,
    _callback_context: CallbackContext,
    state: ContractState,
) -> (ContractState, Vec<EventGroup>) {
    (state, vec![])
}

#[action(shortname = 0x04)]
pub fn provide_liquidity(
    context: ContractContext,
    mut state: ContractState,
    token_address: Address,
    amount: TokenAmount,
) -> (ContractState, Vec<EventGroup>) {
    let user = &context.sender;
    let tokens = state.token_balances.deduce_tokens_in_out(token_address);
    let contract_token_balance = state
        .token_balances
        .get_balance_for(&state.liquidity_pool_address);

    let (token_out_equivalent, minted_liquidity_tokens) = calculate_equivalent_and_minted_tokens(
        amount,
        contract_token_balance.get_amount_of(tokens.token_in),
        contract_token_balance.get_amount_of(tokens.token_out),
        contract_token_balance.liquidity_tokens,
    );
    assert!(
        minted_liquidity_tokens > 0,
        "The given input amount yielded 0 minted liquidity"
    );

    provide_liquidity_internal(
        &mut state,
        user,
        tokens,
        amount,
        token_out_equivalent,
        minted_liquidity_tokens,
    );
    (state, vec![])
}

#[action(shortname = 0x05)]
pub fn reclaim_liquidity(
    context: ContractContext,
    mut state: ContractState,
    liquidity_token_amount: TokenAmount,
) -> (ContractState, Vec<EventGroup>) {
    let user = &context.sender;

    state
        .token_balances
        .deduct_from_token_balance(*user, Token::LIQUIDITY, liquidity_token_amount);

    let contract_token_balance = state
        .token_balances
        .get_balance_for(&state.liquidity_pool_address);

    let (a_output, b_output) = calculate_reclaim_output(
        liquidity_token_amount,
        contract_token_balance.a_tokens,
        contract_token_balance.b_tokens,
        contract_token_balance.liquidity_tokens,
    );

    state
        .token_balances
        .move_tokens(state.liquidity_pool_address, *user, Token::A, a_output);
    state
        .token_balances
        .move_tokens(state.liquidity_pool_address, *user, Token::B, b_output);
    state.token_balances.deduct_from_token_balance(
        state.liquidity_pool_address,
        Token::LIQUIDITY,
        liquidity_token_amount,
    );

    (state, vec![])
}

#[action(shortname = 0x06)]
pub fn provide_initial_liquidity(
    context: ContractContext,
    mut state: ContractState,
    token_a_amount: TokenAmount,
    token_b_amount: TokenAmount,
) -> (ContractState, Vec<EventGroup>) {
    assert!(
        !state.contract_pools_have_liquidity(),
        "Can only initialize when both pools are empty"
    );

    let minted_liquidity_tokens = initial_liquidity_tokens(token_a_amount, token_b_amount);
    assert!(
        minted_liquidity_tokens > 0,
        "The given input amount yielded 0 minted liquidity"
    );

    provide_liquidity_internal(
        &mut state,
        &context.sender,
        TokensInOut::A_IN_B_OUT,
        token_a_amount,
        token_b_amount,
        minted_liquidity_tokens,
    );
    (state, vec![])
}

pub fn initial_liquidity_tokens(
    token_a_amount: TokenAmount,
    token_b_amount: TokenAmount,
) -> TokenAmount {
    u128_sqrt(token_a_amount * token_b_amount).into()
}

pub fn calculate_equivalent_and_minted_tokens(
    token_in_amount: TokenAmount,
    token_in_pool: TokenAmount,
    token_out_pool: TokenAmount,
    total_minted_liquidity: TokenAmount,
) -> (TokenAmount, TokenAmount) {
    // Handle zero-case
    let token_out_equivalent = if token_in_amount > 0 {
        (token_in_amount * token_out_pool / token_in_pool) + 1
    } else {
        0
    };
    let minted_liquidity_tokens = token_in_amount * total_minted_liquidity / token_in_pool;
    (token_out_equivalent, minted_liquidity_tokens)
}

pub fn calculate_reclaim_output(
    liquidity_token_amount: TokenAmount,
    pool_a: TokenAmount,
    pool_b: TokenAmount,
    minted_liquidity: TokenAmount,
) -> (TokenAmount, TokenAmount) {
    let a_output = pool_a * liquidity_token_amount / minted_liquidity;
    let b_output = pool_b * liquidity_token_amount / minted_liquidity;
    (a_output, b_output)
}

fn provide_liquidity_internal(
    state: &mut ContractState,
    user: &Address,
    tokens: TokensInOut,
    token_in_amount: TokenAmount,
    token_out_amount: TokenAmount,
    minted_liquidity_tokens: TokenAmount,
) {
    state.token_balances.move_tokens(
        *user,
        state.liquidity_pool_address,
        tokens.token_in,
        token_in_amount,
    );
    state.token_balances.move_tokens(
        *user,
        state.liquidity_pool_address,
        tokens.token_out,
        token_out_amount,
    );

    state
        .token_balances
        .add_to_token_balance(*user, Token::LIQUIDITY, minted_liquidity_tokens);
    state.token_balances.add_to_token_balance(
        state.liquidity_pool_address,
        Token::LIQUIDITY,
        minted_liquidity_tokens,
    );
}

