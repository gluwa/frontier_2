// This file is part of Frontier.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::sync::Arc;

use ethereum::{
	eip2930, legacy, BlockV3 as EthereumBlock, EIP658ReceiptData, PartialHeader, ReceiptV4,
	TransactionAction, TransactionV3 as EthereumTransaction,
};
use ethereum_types::{Bloom, H160, H256, H64};
use sp_api::{ApiError, ApiRef, ProvideRuntimeApi};
use sp_blockchain::{BlockStatus, HeaderBackend, Info};
use sp_runtime::{generic::Digest, traits::Header as _};
use substrate_test_runtime_client::runtime::{Block, Hash, Header};

use fp_rpc::TransactionStatus;

use super::*;
use crate::{
	cache::build_fee_history_cache_item,
	eth::{eip1559_effective_gas_price, rich_block_build, transaction_build},
};

const GAS_LIMIT: u64 = 75_000_000;
const TX_GAS: u64 = 21_000;

/// A chain together with the base fee the runtime holds once each of its blocks is finalized.
struct MockChain {
	headers: Vec<Header>,
	fee_after_block: Vec<Option<U256>>,
}

impl MockChain {
	/// Creates the chain. Fee `n` is the value left in state by block `n`, as reported by the
	/// runtime's `gas_price` at that block, i.e. the base fee of block `n + 1`.
	fn new(fee_after_block: &[Option<u64>]) -> Self {
		let mut headers: Vec<Header> = Vec::new();
		for number in 0..fee_after_block.len() as u64 {
			let parent_hash = headers.last().map(|h| h.hash()).unwrap_or_default();
			headers.push(Header::new(
				number,
				Default::default(),
				Default::default(),
				parent_hash,
				Digest::default(),
			));
		}
		Self {
			headers,
			fee_after_block: fee_after_block
				.iter()
				.map(|fee| fee.map(U256::from))
				.collect(),
		}
	}

	fn hash(&self, number: usize) -> Hash {
		self.headers[number].hash()
	}
}

#[derive(Clone)]
struct MockClient(Arc<MockChain>);

struct MockRuntimeApi(Arc<MockChain>);

impl ProvideRuntimeApi<Block> for MockClient {
	type Api = MockRuntimeApi;

	fn runtime_api(&self) -> ApiRef<'_, Self::Api> {
		MockRuntimeApi(self.0.clone()).into()
	}
}

impl HeaderBackend<Block> for MockClient {
	fn header(&self, hash: Hash) -> sp_blockchain::Result<Option<Header>> {
		Ok(self.0.headers.iter().find(|h| h.hash() == hash).cloned())
	}

	fn info(&self) -> Info<Block> {
		let best = self.0.headers.last().expect("chain is not empty");
		Info {
			best_hash: best.hash(),
			best_number: *best.number(),
			genesis_hash: self.0.hash(0),
			finalized_hash: best.hash(),
			finalized_number: *best.number(),
			finalized_state: None,
			number_leaves: 1,
			block_gap: None,
		}
	}

	fn status(&self, hash: Hash) -> sp_blockchain::Result<BlockStatus> {
		Ok(match self.header(hash)? {
			Some(_) => BlockStatus::InChain,
			None => BlockStatus::Unknown,
		})
	}

	fn number(&self, hash: Hash) -> sp_blockchain::Result<Option<u64>> {
		Ok(self.header(hash)?.map(|h| *h.number()))
	}

	fn hash(&self, number: u64) -> sp_blockchain::Result<Option<Hash>> {
		Ok(self.0.headers.get(number as usize).map(|h| h.hash()))
	}
}

sp_api::mock_impl_runtime_apis! {
	impl EthereumRuntimeRPCApi<Block> for MockRuntimeApi {
		#[advanced]
		fn gas_price(&self, at: Hash) -> Result<U256, ApiError> {
			self.0
				.headers
				.iter()
				.position(|h| h.hash() == at)
				.and_then(|n| self.0.fee_after_block[n])
				.ok_or_else(|| ApiError::Application(Box::from("state unavailable")))
		}

		fn chain_id() -> u64 {
			unimplemented!()
		}

		fn account_basic(_address: H160) -> fp_evm::Account {
			unimplemented!()
		}

		fn account_code_at(_address: H160) -> Vec<u8> {
			unimplemented!()
		}

		fn author() -> H160 {
			unimplemented!()
		}

		fn storage_at(_address: H160, _index: U256) -> H256 {
			unimplemented!()
		}

		fn call(
			_from: H160,
			_to: H160,
			_data: Vec<u8>,
			_value: U256,
			_gas_limit: U256,
			_max_fee_per_gas: Option<U256>,
			_max_priority_fee_per_gas: Option<U256>,
			_nonce: Option<U256>,
			_estimate: bool,
			_access_list: Option<Vec<(H160, Vec<H256>)>>,
			_authorization_list: Option<ethereum::AuthorizationList>,
		) -> Result<fp_evm::ExecutionInfoV2<Vec<u8>>, sp_runtime::DispatchError> {
			unimplemented!()
		}

		fn create(
			_from: H160,
			_data: Vec<u8>,
			_value: U256,
			_gas_limit: U256,
			_max_fee_per_gas: Option<U256>,
			_max_priority_fee_per_gas: Option<U256>,
			_nonce: Option<U256>,
			_estimate: bool,
			_access_list: Option<Vec<(H160, Vec<H256>)>>,
			_authorization_list: Option<ethereum::AuthorizationList>,
		) -> Result<fp_evm::ExecutionInfoV2<H160>, sp_runtime::DispatchError> {
			unimplemented!()
		}

		fn current_block() -> Option<EthereumBlock> {
			unimplemented!()
		}

		fn current_receipts() -> Option<Vec<ReceiptV4>> {
			unimplemented!()
		}

		fn current_transaction_statuses() -> Option<Vec<TransactionStatus>> {
			unimplemented!()
		}

		fn current_all() -> (
			Option<EthereumBlock>,
			Option<Vec<ReceiptV4>>,
			Option<Vec<TransactionStatus>>,
		) {
			unimplemented!()
		}

		fn extrinsic_filter(
			_xts: Vec<<Block as BlockT>::Extrinsic>,
		) -> Vec<EthereumTransaction> {
			unimplemented!()
		}

		fn elasticity() -> Option<Permill> {
			unimplemented!()
		}

		fn gas_limit_multiplier_support() {
			unimplemented!()
		}

		fn pending_block(
			_xts: Vec<<Block as BlockT>::Extrinsic>,
		) -> (Option<EthereumBlock>, Option<Vec<TransactionStatus>>) {
			unimplemented!()
		}

		fn initialize_pending_block(_header: &<Block as BlockT>::Header) {
			unimplemented!()
		}
	}
}

// Base fee left in state by each block of the chains used below.
//
// Each block is executed with the fee its parent left, so the fee a block was executed with
// is `fee_after_block[number - 1]` (the genesis block has no parent).
//
//   block   executed with   left behind
//     0         1000           1000      (genesis)
//     1         1000           1125      (fee increases)
//     2         1125           1125      (fee unchanged)
//     3         1125            984      (fee decreases)
//     4          984            984      (fee unchanged)
const FEES_AFTER_BLOCK: [Option<u64>; 5] =
	[Some(1_000), Some(1_125), Some(1_125), Some(984), Some(984)];
const FEES_EXECUTED_WITH: [u64; 5] = [1_000, 1_000, 1_125, 1_125, 984];

fn mock_client(fee_after_block: &[Option<u64>]) -> (Arc<MockChain>, MockClient) {
	let chain = Arc::new(MockChain::new(fee_after_block));
	(chain.clone(), MockClient(chain))
}

#[test]
fn execution_base_fee_is_the_fee_left_by_the_parent() {
	let (chain, client) = mock_client(&FEES_AFTER_BLOCK);

	for (number, expected) in FEES_EXECUTED_WITH.iter().enumerate() {
		assert_eq!(
			execution_base_fee::<Block, _>(&client, chain.hash(number)),
			Some(U256::from(*expected)),
			"block {number}"
		);
	}
}

#[test]
fn execution_base_fee_differs_from_runtime_gas_price_when_fee_changes() {
	let (chain, client) = mock_client(&FEES_AFTER_BLOCK);
	let runtime_gas_price = |number: usize| {
		client
			.runtime_api()
			.gas_price(chain.hash(number))
			.expect("fee is available")
	};
	let executed_with = |number: usize| {
		execution_base_fee::<Block, _>(&client, chain.hash(number)).expect("fee is available")
	};

	// Increasing: the runtime already reports the adjusted fee.
	assert_eq!(runtime_gas_price(1), U256::from(1_125));
	assert_eq!(executed_with(1), U256::from(1_000));
	// Decreasing.
	assert_eq!(runtime_gas_price(3), U256::from(984));
	assert_eq!(executed_with(3), U256::from(1_125));
	// Unchanged: both views agree.
	assert_eq!(runtime_gas_price(2), executed_with(2));
	assert_eq!(runtime_gas_price(4), executed_with(4));
	// The genesis block reports the initial fee.
	assert_eq!(runtime_gas_price(0), executed_with(0));
}

#[test]
fn execution_base_fee_is_none_when_it_cannot_be_determined() {
	// State of block 1 is not available (e.g. pruned), so the fee of block 2 is unknown. The
	// fee left by block 2 itself must not be reported in its place.
	let (chain, client) = mock_client(&[Some(1_000), None, Some(1_125)]);
	assert_eq!(execution_base_fee::<Block, _>(&client, chain.hash(2)), None);
	// Unknown block.
	assert_eq!(
		execution_base_fee::<Block, _>(&client, Hash::repeat_byte(7)),
		None
	);
}

#[test]
fn next_block_base_fee_estimate_follows_the_gas_used_ratio() {
	let elasticity = Permill::from_parts(125_000);
	let estimate =
		|fee: u64, ratio: f64| estimate_next_base_fee(U256::from(fee), ratio, elasticity);

	// Above target: increases, up to elasticity when the block is full.
	assert_eq!(estimate(1_000, 1.0), U256::from(1_125));
	assert_eq!(estimate(1_000, 0.75), U256::from(1_062));
	// At target: unchanged.
	assert_eq!(estimate(1_000, 0.5), U256::from(1_000));
	// Below target: decreases, down by elasticity when the block is empty.
	assert_eq!(estimate(1_000, 0.0), U256::from(875));
	assert_eq!(estimate(1_000, 0.25), U256::from(937));
	// Without elasticity the fee is constant.
	assert_eq!(
		estimate_next_base_fee(U256::from(1_000), 1.0, Permill::zero()),
		U256::from(1_000)
	);
}

fn signature() -> eip2930::TransactionSignature {
	eip2930::TransactionSignature::new(false, H256::from_low_u64_be(1), H256::from_low_u64_be(1))
		.expect("valid signature")
}

fn eip1559_tx(max_priority_fee_per_gas: u64, max_fee_per_gas: u64) -> EthereumTransaction {
	EthereumTransaction::EIP1559(ethereum::EIP1559Transaction {
		chain_id: 42,
		nonce: U256::zero(),
		max_priority_fee_per_gas: U256::from(max_priority_fee_per_gas),
		max_fee_per_gas: U256::from(max_fee_per_gas),
		gas_limit: U256::from(TX_GAS),
		action: TransactionAction::Call(H160::repeat_byte(1)),
		value: U256::zero(),
		input: Vec::new(),
		access_list: Vec::new(),
		signature: signature(),
	})
}

fn legacy_tx(gas_price: u64) -> EthereumTransaction {
	EthereumTransaction::Legacy(ethereum::LegacyTransaction {
		nonce: U256::zero(),
		gas_price: U256::from(gas_price),
		gas_limit: U256::from(TX_GAS),
		action: TransactionAction::Call(H160::repeat_byte(1)),
		value: U256::zero(),
		input: Vec::new(),
		signature: legacy::TransactionSignature::new(
			27,
			H256::from_low_u64_be(1),
			H256::from_low_u64_be(1),
		)
		.expect("valid signature"),
	})
}

/// Transactions that can be included in a block executed with any fee of `FEES_EXECUTED_WITH`.
fn transactions() -> Vec<EthereumTransaction> {
	vec![
		// Zero tip.
		eip1559_tx(0, 2_000),
		// Nonzero tip below the fee cap.
		eip1559_tx(7, 2_000),
		// Tip limited by the fee cap when the base fee is 1125.
		eip1559_tx(50, 1_130),
		// Legacy transaction pays its gas price, whatever the base fee is.
		legacy_tx(1_130),
	]
}

/// The tip each transaction of `transactions()` pays on top of `base_fee`.
fn expected_tips(base_fee: u64) -> Vec<u64> {
	vec![0, 7, 50.min(1_130 - base_fee), 1_130 - base_fee]
}

fn build_block(number: u64, transactions: Vec<EthereumTransaction>) -> EthereumBlock {
	let gas_used = TX_GAS * transactions.len() as u64;
	EthereumBlock::new(
		PartialHeader {
			parent_hash: H256::zero(),
			beneficiary: H160::zero(),
			state_root: H256::zero(),
			receipts_root: H256::zero(),
			logs_bloom: Bloom::default(),
			difficulty: U256::zero(),
			number: U256::from(number),
			gas_limit: U256::from(GAS_LIMIT),
			gas_used: U256::from(gas_used),
			timestamp: 1_000 * number,
			extra_data: Vec::new(),
			mix_hash: H256::zero(),
			nonce: H64::zero(),
		},
		transactions,
		Vec::new(),
	)
}

fn build_receipts(transactions: &[EthereumTransaction]) -> Vec<ReceiptV4> {
	transactions
		.iter()
		.enumerate()
		.map(|(i, transaction)| {
			let data = EIP658ReceiptData {
				status_code: 1,
				used_gas: U256::from(TX_GAS * (i as u64 + 1)),
				logs_bloom: Bloom::default(),
				logs: Vec::new(),
			};
			match transaction {
				EthereumTransaction::Legacy(_) => ReceiptV4::Legacy(data),
				_ => ReceiptV4::EIP1559(data),
			}
		})
		.collect()
}

fn build_statuses(count: usize) -> Vec<Option<TransactionStatus>> {
	(0..count)
		.map(|i| {
			Some(TransactionStatus {
				transaction_hash: H256::from_low_u64_be(i as u64 + 1),
				transaction_index: i as u32,
				..Default::default()
			})
		})
		.collect()
}

/// Block, transaction and fee history views of a block agree on the fee it was executed with,
/// and the base fee together with the effective priority fee explains the effective gas price
/// of every transaction.
#[test]
fn block_transaction_and_fee_history_agree_on_the_execution_base_fee() {
	let (chain, client) = mock_client(&FEES_AFTER_BLOCK);

	// Blocks 1 to 4 cover an increasing, unchanged, decreasing and unchanged fee.
	for (number, executed_with) in FEES_EXECUTED_WITH.iter().copied().enumerate().skip(1) {
		let base_fee =
			execution_base_fee::<Block, _>(&client, chain.hash(number)).expect("fee is available");
		assert_eq!(base_fee, U256::from(executed_with));
		let tips = expected_tips(executed_with);

		let transactions = transactions();
		let block = build_block(number as u64, transactions.clone());
		let receipts = build_receipts(&transactions);

		// Block (`eth_getBlockByNumber` / `eth_getBlockByHash`).
		let rich_block = rich_block_build(
			block.clone(),
			build_statuses(transactions.len()),
			None,
			true,
			Some(base_fee),
			false,
		);
		assert_eq!(rich_block.inner.base_fee_per_gas, Some(base_fee));

		// Fee history entry of the block.
		let (cache_item, cache_number) =
			build_fee_history_cache_item(Some(block.clone()), Some(receipts), base_fee);
		assert_eq!(cache_number, Some(number as u64));
		assert_eq!(
			U256::from(cache_item.base_fee),
			rich_block.inner.base_fee_per_gas.unwrap()
		);

		let BlockTransactions::Full(rpc_transactions) = &rich_block.inner.transactions else {
			panic!("full transactions were requested");
		};
		assert_eq!(rpc_transactions.len(), transactions.len());
		for (i, rpc_transaction) in rpc_transactions.iter().enumerate() {
			// Effective gas price as reported by the receipt.
			let receipt_price = match &transactions[i] {
				EthereumTransaction::EIP1559(t) => eip1559_effective_gas_price(
					base_fee,
					t.max_priority_fee_per_gas,
					t.max_fee_per_gas,
				),
				EthereumTransaction::Legacy(t) => t.gas_price,
				_ => unreachable!(),
			};
			let price = rpc_transaction
				.gas_price
				.expect("mined transactions report a price");
			// Transaction object and receipt agree.
			let transaction = transaction_build(
				&transactions[i],
				Some(&block),
				build_statuses(transactions.len())[i].as_ref(),
				Some(base_fee),
			);
			assert_eq!(transaction.gas_price, Some(price), "block {number} tx {i}");
			assert_eq!(receipt_price, price, "block {number} tx {i}");
			// Reported base fee and effective priority fee add up to the effective gas price.
			assert_eq!(
				base_fee + U256::from(tips[i]),
				price,
				"block {number} tx {i}"
			);
		}

		// Fee history rewards are the effective priority fees (all transactions use the same
		// gas, so the percentiles pick the sorted tips in order).
		let mut sorted_tips = tips.clone();
		sorted_tips.sort();
		assert_eq!(cache_item.rewards.len(), 201);
		assert_eq!(cache_item.rewards[0], sorted_tips[0]);
		assert_eq!(cache_item.rewards[50], sorted_tips[0]);
		assert_eq!(cache_item.rewards[100], sorted_tips[1]);
		assert_eq!(cache_item.rewards[150], sorted_tips[2]);
		assert_eq!(cache_item.rewards[200], sorted_tips[3]);
	}
}

/// Documents the effect of building the entry with the fee left behind by the block instead of
/// the fee it was executed with.
#[test]
fn fee_history_rewards_depend_on_the_base_fee_used() {
	let transactions = transactions();
	let block = build_block(1, transactions.clone());
	let receipts = build_receipts(&transactions);

	// Block 1 is executed with 1000 and leaves 1125 behind.
	let (executed, _) = build_fee_history_cache_item(
		Some(block.clone()),
		Some(receipts.clone()),
		U256::from(1_000),
	);
	let (adjusted, _) =
		build_fee_history_cache_item(Some(block), Some(receipts), U256::from(1_125));

	assert_eq!(executed.base_fee, 1_000);
	assert_eq!(executed.rewards[0], 0);
	assert_eq!(executed.rewards[150], 50);
	assert_eq!(executed.rewards[200], 130);
	assert_eq!(adjusted.base_fee, 1_125);
	assert_eq!(adjusted.rewards[0], 0);
	assert_eq!(adjusted.rewards[150], 5);
	assert_eq!(adjusted.rewards[200], 7);
	// Unchanged by the base fee: the zero tip.
	assert_eq!(executed.gas_used_ratio, adjusted.gas_used_ratio);
}

#[test]
fn fee_history_entry_without_block_data_has_zero_rewards() {
	let (cache_item, cache_number) = build_fee_history_cache_item(None, None, U256::from(1_000));
	assert_eq!(cache_number, None);
	assert_eq!(cache_item.base_fee, 1_000);
	assert_eq!(cache_item.gas_used_ratio, 0f64);
	assert_eq!(cache_item.rewards, vec![0; 201]);
}
