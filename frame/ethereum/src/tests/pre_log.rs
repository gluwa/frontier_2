// This file is part of Frontier.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for blocks imported through a `PreLog`.

use super::*;
use crate::{Bloom, DigestItem, Encode, PreLog, FRONTIER_ENGINE_ID, H64};
use frame_support::{traits::Hooks, weights::Weight};

// Gas used by a plain value transfer.
const TRANSFER_GAS: u64 = 21_000;

fn transfer(sender: &AccountInfo, recipient: H160, nonce: u64) -> Transaction {
	LegacyUnsignedTransaction {
		nonce: U256::from(nonce),
		gas_price: U256::from(1),
		gas_limit: U256::from(TRANSFER_GAS),
		action: TransactionAction::Call(recipient),
		value: U256::from(1),
		input: Default::default(),
	}
	.sign(&sender.private_key)
}

fn pre_log_block(transactions: Vec<Transaction>) -> ethereum::BlockV3 {
	ethereum::Block::new(
		ethereum::PartialHeader {
			parent_hash: H256::default(),
			beneficiary: H160::default(),
			state_root: H256::default(),
			receipts_root: H256::default(),
			logs_bloom: Bloom::default(),
			difficulty: U256::zero(),
			number: U256::one(),
			gas_limit: U256::zero(),
			gas_used: U256::zero(),
			timestamp: 0,
			extra_data: Vec::new(),
			mix_hash: H256::default(),
			nonce: H64::default(),
		},
		transactions,
		Vec::new(),
	)
}

// Imports `count` transfers from `sender` as a `PreLog` block, returning the weight used on
// initialization.
fn import_pre_log_block(sender: &AccountInfo, recipient: H160, count: u64) -> Weight {
	let transactions = (0..count)
		.map(|nonce| transfer(sender, recipient, nonce))
		.collect();

	System::set_block_number(1);
	System::deposit_log(DigestItem::PreRuntime(
		FRONTIER_ENGINE_ID,
		PreLog::Block(pre_log_block(transactions)).encode(),
	));

	let weight = Ethereum::on_initialize(1);
	Ethereum::on_finalize(1);
	weight
}

#[test]
fn pre_log_batch_within_block_gas_limit_works() {
	let (pairs, mut ext) = new_test_ext(2);
	let alice = &pairs[0];
	let bob = &pairs[1];

	ext.execute_with(|| {
		let weight = import_pre_log_block(alice, bob.address, 3);
		assert_ne!(weight, Weight::zero());

		let block = crate::CurrentBlock::<Test>::get().expect("block is stored");
		assert_eq!(block.transactions.len(), 3);
		assert_eq!(block.header.gas_used, U256::from(3 * TRANSFER_GAS));
		assert!(block.header.gas_used <= block.header.gas_limit);
	});
}

#[test]
fn pre_log_batch_at_block_gas_limit_works() {
	let (pairs, mut ext) = new_test_ext(2);
	let alice = &pairs[0];
	let bob = &pairs[1];

	ext.execute_with(|| {
		// The block gas limit is exactly filled by the batch.
		let _guard = set_block_gas_limit(3 * TRANSFER_GAS);
		import_pre_log_block(alice, bob.address, 3);

		let block = crate::CurrentBlock::<Test>::get().expect("block is stored");
		assert_eq!(block.transactions.len(), 3);
		assert_eq!(block.header.gas_limit, U256::from(3 * TRANSFER_GAS));
		assert_eq!(block.header.gas_used, block.header.gas_limit);
	});
}

#[test]
#[should_panic(expected = "pre-block gas used exceeds the block gas limit")]
fn pre_log_batch_one_gas_over_block_gas_limit_fails() {
	let (pairs, mut ext) = new_test_ext(2);
	let alice = &pairs[0];
	let bob = &pairs[1];

	ext.execute_with(|| {
		// Each transaction fits the block gas limit on its own, the batch does not.
		let _guard = set_block_gas_limit(3 * TRANSFER_GAS - 1);
		import_pre_log_block(alice, bob.address, 3);
	});
}

#[test]
#[should_panic(expected = "pre-block gas used exceeds the block gas limit")]
fn pre_log_batch_over_block_gas_limit_fails() {
	let (pairs, mut ext) = new_test_ext(2);
	let alice = &pairs[0];
	let bob = &pairs[1];

	ext.execute_with(|| {
		let _guard = set_block_gas_limit(2 * TRANSFER_GAS);
		import_pre_log_block(alice, bob.address, 5);
	});
}
