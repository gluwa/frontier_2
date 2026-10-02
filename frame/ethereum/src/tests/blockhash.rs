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

//! BLOCKHASH opcode tests against the Ethereum block hash mapping

use frame_support::traits::Hooks;
use pallet_evm::Runner;

use super::*;
use crate::BlockHash;

// Runtime code (12 bytes): PUSH1 0, CALLDATALOAD, BLOCKHASH, PUSH1 0, MSTORE,
// PUSH1 32, PUSH1 0, RETURN. Returns BLOCKHASH(calldata[0..32]).
const BLOCKHASH_CONTRACT: &str = "6000354060005260206000f3";

/// Installs a contract executing BLOCKHASH(calldata[0..32]) and returns its address.
fn deploy_blockhash_contract() -> H160 {
	let contract_addr = H160::from_str("2000000000000000000000000000000000000001").unwrap();
	EVM::create_account(
		contract_addr,
		hex::decode(BLOCKHASH_CONTRACT).expect("Failed to decode contract"),
		None,
	)
	.expect("contract creation should succeed");
	contract_addr
}

/// Executes BLOCKHASH(number) through the EVM runner and returns the result.
fn block_hash_opcode(contract_addr: H160, number: u64) -> H256 {
	let info = <Test as pallet_evm::Config>::Runner::call(
		H160::default(),
		contract_addr,
		U256::from(number).to_big_endian().to_vec(),
		U256::zero(),
		1_000_000,
		None,
		None,
		None,
		Vec::new(),
		Vec::new(),
		false, // transactional
		false, // must be validated
		None,
		None,
		<Test as pallet_evm::Config>::config(),
	)
	.expect("contract call should succeed");
	assert!(info.exit_reason.is_succeed());
	H256::from_slice(&info.value)
}

#[test]
fn blockhash_opcode_does_not_expose_retained_genesis_hash() {
	let (_, mut ext) = new_test_ext(1);

	ext.execute_with(|| {
		let contract_addr = deploy_blockhash_contract();

		// Store the genesis block like the pallet genesis build does, then produce more blocks
		// than the BlockHash pruning window.
		Ethereum::store_block(None, U256::zero());
		let genesis_hash = BlockHash::<Test>::get(U256::zero());
		assert_ne!(genesis_hash, H256::zero());
		for number in 1..=300u64 {
			System::set_block_number(number);
			Ethereum::on_finalize(number);
		}

		// Block hash pruning keeps the genesis entry, but drops the entries that are out of the
		// pruning window.
		assert_eq!(BlockHash::<Test>::get(U256::zero()), genesis_hash);
		assert_eq!(BlockHash::<Test>::get(U256::from(49)), H256::zero());
		let parent_hash = BlockHash::<Test>::get(U256::from(300));
		assert_ne!(parent_hash, H256::zero());

		// A transaction executing in block 301: genesis is 301 blocks old, so BLOCKHASH(0) is
		// zero although the mapping still retains it.
		System::set_block_number(301);
		assert_eq!(block_hash_opcode(contract_addr, 0), H256::zero());
		// The parent and the oldest retained ancestors are still returned.
		assert_eq!(block_hash_opcode(contract_addr, 300), parent_hash);
		assert_eq!(
			block_hash_opcode(contract_addr, 50),
			BlockHash::<Test>::get(U256::from(50))
		);
		// The current and future blocks never are.
		assert_eq!(block_hash_opcode(contract_addr, 301), H256::zero());
		assert_eq!(block_hash_opcode(contract_addr, 302), H256::zero());

		// Simulating on top of block 300 (e.g. eth_call): its own hash is stored in the mapping,
		// but it is the current block.
		System::set_block_number(300);
		assert_eq!(block_hash_opcode(contract_addr, 300), H256::zero());
		assert_eq!(block_hash_opcode(contract_addr, 0), H256::zero());
		assert_eq!(
			block_hash_opcode(contract_addr, 299),
			BlockHash::<Test>::get(U256::from(299))
		);
	});
}

#[test]
fn blockhash_opcode_returns_genesis_hash_within_window() {
	let (_, mut ext) = new_test_ext(1);

	ext.execute_with(|| {
		let contract_addr = deploy_blockhash_contract();

		Ethereum::store_block(None, U256::zero());
		let genesis_hash = BlockHash::<Test>::get(U256::zero());
		assert_ne!(genesis_hash, H256::zero());
		for number in 1..=100u64 {
			System::set_block_number(number);
			Ethereum::on_finalize(number);
		}

		// Genesis is 101 blocks old: still within the 256 blocks window.
		System::set_block_number(101);
		assert_eq!(block_hash_opcode(contract_addr, 0), genesis_hash);
	});
}
