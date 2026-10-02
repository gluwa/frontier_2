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

use std::collections::BTreeMap;

use ethereum_types::{U256, U512};
use jsonrpsee::core::RpcResult;
// Substrate
use sc_client_api::backend::{Backend, StorageProvider};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::{
	traits::{Block as BlockT, Header as HeaderT, UniqueSaturatedInto, Zero},
	Permill,
};
// Frontier
use fc_rpc_core::types::*;
use fp_rpc::EthereumRuntimeRPCApi;

use crate::{eth::Eth, frontier_backend_client, internal_err};

/// Returns the base fee the transactions of block `hash` were executed with.
///
/// The base fee is adjusted when a block is finalized, after its transactions have run. The
/// runtime's `gas_price` at `hash` therefore already holds the fee of the following block, while the
/// fee that governed `hash` is the one its parent left behind. The genesis block has no parent, so
/// its own (initial) value is used.
pub(crate) fn execution_base_fee<B, C>(client: &C, hash: B::Hash) -> Option<U256>
where
	B: BlockT,
	C: ProvideRuntimeApi<B> + HeaderBackend<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
{
	let header = client.header(hash).ok().flatten()?;
	let fee_hash = if header.number().is_zero() {
		hash
	} else {
		*header.parent_hash()
	};
	client.runtime_api().gas_price(fee_hash).ok()
}

/// Estimates the base fee following a block from its base fee and gas used ratio.
///
/// Only an approximation of the runtime's adjustment, which is driven by the block weight.
fn estimate_next_base_fee(last_fee_per_gas: U256, last_gas_used: f64, elasticity: Permill) -> U256 {
	let elasticity = elasticity.deconstruct() as f64 / 1_000_000f64;
	next_base_fee(last_fee_per_gas, last_gas_used, elasticity)
}

impl<B, C, P, CT, BE, CIDP, EC> Eth<B, C, P, CT, BE, CIDP, EC>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
{
	pub fn gas_price(&self) -> RpcResult<U256> {
		let block_hash = self.client.info().best_hash;

		self.client
			.runtime_api()
			.gas_price(block_hash)
			.map_err(|err| internal_err(format!("fetch runtime chain id failed: {err:?}")))
	}

	pub async fn fee_history(
		&self,
		block_count: u64,
		newest_block: BlockNumberOrHash,
		reward_percentiles: Option<Vec<f64>>,
	) -> RpcResult<FeeHistory> {
		// The max supported range size is 1024 by spec.
		let range_limit: u64 = 1024;
		let block_count: u64 = u64::min(block_count, range_limit);

		if let Some(id) = frontier_backend_client::native_block_id::<B, C>(
			self.client.as_ref(),
			self.backend.as_ref(),
			Some(newest_block),
		)
		.await?
		{
			let Ok(number) = self.client.expect_block_number_from_id(&id) else {
				return Err(internal_err(format!(
					"Failed to retrieve block number at {id}"
				)));
			};
			// Highest and lowest block number within the requested range.
			let highest = UniqueSaturatedInto::<u64>::unique_saturated_into(number);
			let lowest = highest.saturating_sub(block_count.saturating_sub(1));
			// Tip of the chain.
			let best_number =
				UniqueSaturatedInto::<u64>::unique_saturated_into(self.client.info().best_number);
			// Only support in-cache queries.
			if lowest < best_number.saturating_sub(self.fee_history_cache_limit) {
				return Err(internal_err("Block range out of bounds."));
			}
			// Everything needed is copied out of the cache while holding the lock, which is released
			// before querying the runtime below.
			let mut response = {
				let Ok(fee_history_cache) = self.fee_history_cache.lock() else {
					return Err(internal_err("Failed to read fee history cache."));
				};
				fee_history_from_cache(
					&fee_history_cache,
					lowest,
					highest,
					reward_percentiles.as_deref(),
				)
			};
			// Calculate next base fee.
			if let (Some(last_gas_used), Some(last_fee_per_gas)) = (
				response.gas_used_ratio.last(),
				response.base_fee_per_gas.last(),
			) {
				let substrate_hash = self
					.client
					.expect_block_hash_from_id(&id)
					.map_err(|_| internal_err(format!("Expect block number from id: {id}")))?;
				// The cached entries hold the fee each block was executed with, so the fee of the
				// block following the newest one is not among them. It is the value the runtime
				// holds once the newest block has been finalized, which is what `gas_price`
				// reports at that block.
				let next_base_fee = match self.client.runtime_api().gas_price(substrate_hash) {
					Ok(next_base_fee) => next_base_fee,
					// Estimate when the runtime cannot be queried, e.g. for pruned state.
					Err(_) => {
						let elasticity = self
							.storage_override
							.elasticity(substrate_hash)
							.unwrap_or(Permill::from_parts(125_000));
						estimate_next_base_fee(*last_fee_per_gas, *last_gas_used, elasticity)
					}
				};
				response.base_fee_per_gas.push(next_base_fee);
			}
			return Ok(response);
		}
		Err(internal_err(format!(
			"Failed to retrieve requested block {newest_block:?}."
		)))
	}

	pub fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
		// https://github.com/ethereum/go-ethereum/blob/master/eth/ethconfig/config.go#L44-L51
		let at_percentile = 60;
		let block_count = 20;
		let index = (at_percentile * 2) as usize;

		let highest =
			UniqueSaturatedInto::<u64>::unique_saturated_into(self.client.info().best_number);
		let lowest = highest.saturating_sub(block_count - 1);

		// https://github.com/ethereum/go-ethereum/blob/master/eth/gasprice/gasprice.go#L149
		let mut rewards = Vec::new();
		if let Ok(fee_history_cache) = &self.fee_history_cache.lock() {
			for n in lowest..highest + 1 {
				if let Some(block) = fee_history_cache.get(&n) {
					let reward = if let Some(r) = block.rewards.get(index) {
						U256::from(*r)
					} else {
						U256::zero()
					};
					rewards.push(reward);
				}
			}
		} else {
			return Err(internal_err("Failed to read fee oracle cache."));
		}
		Ok(*rewards.iter().min().unwrap_or(&U256::zero()))
	}
}

/// Collects the cached fee history of the blocks in `lowest..=highest`.
fn fee_history_from_cache(
	fee_history_cache: &BTreeMap<u64, FeeHistoryCacheItem>,
	lowest: u64,
	highest: u64,
	reward_percentiles: Option<&[f64]>,
) -> FeeHistory {
	let mut response = FeeHistory {
		oldest_block: U256::from(lowest),
		base_fee_per_gas: Vec::new(),
		gas_used_ratio: Vec::new(),
		reward: None,
	};
	let mut rewards = Vec::new();
	// Iterate over the requested block range.
	for n in lowest..highest + 1 {
		if let Some(block) = fee_history_cache.get(&n) {
			response.base_fee_per_gas.push(block.base_fee);
			response.gas_used_ratio.push(block.gas_used_ratio);
			// If the request includes reward percentiles, get them from the cache.
			if let Some(requested_percentiles) = reward_percentiles {
				let mut block_rewards = Vec::new();
				// Resolution is half a point. I.e. 1.0,1.5
				let resolution_per_percentile: f64 = 2.0;
				// Get cached reward for each provided percentile.
				for p in requested_percentiles {
					// Find the cache index from the user percentile.
					let p = p.clamp(0.0, 100.0);
					let index = ((p.round() / 2f64) * 2f64) * resolution_per_percentile;
					// Get and push the reward.
					let reward = if let Some(r) = block.rewards.get(index as usize) {
						U256::from(*r)
					} else {
						U256::zero()
					};
					block_rewards.push(reward);
				}
				// Push block rewards.
				if !block_rewards.is_empty() {
					// Push block rewards.
					rewards.push(block_rewards);
				}
			}
		}
	}
	if rewards.len() > 0 {
		response.reward = Some(rewards);
	}
	response
}

/// Base fee of the block following one with `last_base_fee` and `gas_used_ratio`.
///
/// The base fee is adjusted in `U256` arithmetic, so values above `u64::MAX` are neither clamped
/// nor rounded through a floating point representation. Only the adjustment factor, which is
/// bounded by `elasticity`, is derived from floating point values.
fn next_base_fee(last_base_fee: U256, gas_used_ratio: f64, elasticity: f64) -> U256 {
	if gas_used_ratio > 0.5 {
		// Increase base gas
		let increase = ((gas_used_ratio - 0.5) * 2f64) * elasticity;
		last_base_fee.saturating_add(scale_base_fee(last_base_fee, increase))
	} else if gas_used_ratio < 0.5 {
		// Decrease base gas
		let decrease = ((0.5 - gas_used_ratio) * 2f64) * elasticity;
		last_base_fee.saturating_sub(scale_base_fee(last_base_fee, decrease))
	} else {
		// Same base gas
		last_base_fee
	}
}

/// `base_fee * factor`, rounded down. Saturates at `U256::MAX`.
fn scale_base_fee(base_fee: U256, factor: f64) -> U256 {
	/// Fixed point denominator the factor is expressed in.
	const DENOMINATOR: u64 = 1_000_000_000_000_000_000;

	// Float to int casts saturate, and map NaN to zero.
	let numerator = (factor * DENOMINATOR as f64) as u128;
	let scaled = base_fee.full_mul(U256::from(numerator)) / U512::from(DENOMINATOR);
	U256::try_from(scaled).unwrap_or(U256::MAX)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod precision_tests {
	use ethereum_types::{H160, H256};

	use super::*;
	use crate::cache::build_fee_history_cache_item;

	/// Elasticity applied when the runtime does not report one.
	const ELASTICITY: f64 = 0.125;

	/// Base fees at, right above and well beyond the `u64` range.
	fn base_fees() -> Vec<U256> {
		vec![
			U256::from(u64::MAX),
			U256::from(u64::MAX) + 1,
			U256::from(u128::MAX) * 3,
			U256::from(10u64).pow(U256::from(40u64)),
		]
	}

	fn ethereum_block(number: u64, gas_limit: u64, gas_used: u64) -> ethereum::BlockV3 {
		let partial_header = ethereum::PartialHeader {
			parent_hash: H256::default(),
			beneficiary: H160::default(),
			state_root: H256::default(),
			receipts_root: H256::default(),
			logs_bloom: ethereum_types::Bloom::default(),
			difficulty: U256::zero(),
			number: U256::from(number),
			gas_limit: U256::from(gas_limit),
			gas_used: U256::from(gas_used),
			timestamp: 0u64,
			extra_data: Vec::new(),
			mix_hash: H256::default(),
			nonce: ethereum_types::H64::default(),
		};
		ethereum::Block::new(partial_header, vec![], vec![])
	}

	#[test]
	fn cache_item_keeps_runtime_base_fee() {
		for base_fee in base_fees() {
			// Block available.
			let block = ethereum_block(7, 100, 50);
			let (item, number) = build_fee_history_cache_item(Some(block), Some(vec![]), base_fee);
			assert_eq!(number, Some(7));
			assert_eq!(item.base_fee, base_fee);
			// Block not available.
			let (item, number) = build_fee_history_cache_item(None, None, base_fee);
			assert_eq!(number, None);
			assert_eq!(item.base_fee, base_fee);
		}
	}

	#[test]
	fn fee_history_reports_runtime_base_fee() {
		let base_fees = base_fees();
		let mut cache = BTreeMap::new();
		for (i, base_fee) in base_fees.iter().enumerate() {
			let number = i as u64 + 1;
			let block = ethereum_block(number, 100, 100);
			let (item, key) = build_fee_history_cache_item(Some(block), Some(vec![]), *base_fee);
			assert_eq!(key, Some(number));
			cache.insert(number, item);
		}
		let highest = base_fees.len() as u64;

		let response = fee_history_from_cache(&cache, 1, highest, None);
		assert_eq!(response.oldest_block, U256::one());
		// Every historical value matches the runtime one.
		assert_eq!(response.base_fee_per_gas, base_fees);
		assert_eq!(response.gas_used_ratio, vec![1.0; base_fees.len()]);

		// A single block range reports that block only.
		for (i, base_fee) in base_fees.iter().enumerate() {
			let number = i as u64 + 1;
			let response = fee_history_from_cache(&cache, number, number, None);
			assert_eq!(response.base_fee_per_gas, vec![*base_fee]);
		}
	}

	#[test]
	fn next_base_fee_keeps_full_precision() {
		for base_fee in base_fees() {
			// Full block: fee * (1 + elasticity).
			assert_eq!(
				next_base_fee(base_fee, 1.0, ELASTICITY),
				base_fee + base_fee / 8
			);
			// Three quarters full: half of the elasticity.
			assert_eq!(
				next_base_fee(base_fee, 0.75, ELASTICITY),
				base_fee + base_fee / 16
			);
			// On target.
			assert_eq!(next_base_fee(base_fee, 0.5, ELASTICITY), base_fee);
			// A quarter full: half of the elasticity.
			assert_eq!(
				next_base_fee(base_fee, 0.25, ELASTICITY),
				base_fee - base_fee / 16
			);
			// Empty block: fee * (1 - elasticity).
			assert_eq!(
				next_base_fee(base_fee, 0.0, ELASTICITY),
				base_fee - base_fee / 8
			);
		}
	}

	#[test]
	fn next_base_fee_follows_cached_base_fee() {
		for base_fee in base_fees() {
			let block = ethereum_block(1, 100, 100);
			let (item, key) = build_fee_history_cache_item(Some(block), Some(vec![]), base_fee);
			let mut cache = BTreeMap::new();
			cache.insert(key.unwrap(), item);

			let mut response = fee_history_from_cache(&cache, 1, 1, None);
			let next = next_base_fee(
				*response.base_fee_per_gas.last().unwrap(),
				*response.gas_used_ratio.last().unwrap(),
				ELASTICITY,
			);
			response.base_fee_per_gas.push(next);
			assert_eq!(
				response.base_fee_per_gas,
				vec![base_fee, base_fee + base_fee / 8]
			);
		}
	}

	#[test]
	fn next_base_fee_does_not_overflow() {
		assert_eq!(next_base_fee(U256::MAX, 1.0, ELASTICITY), U256::MAX);
		assert_eq!(
			next_base_fee(U256::MAX, 0.0, ELASTICITY),
			U256::MAX - U256::MAX / 8
		);
		// Degenerate ratios (e.g. a zero gas limit) never panic.
		assert_eq!(
			next_base_fee(U256::MAX, f64::INFINITY, ELASTICITY),
			U256::MAX
		);
		assert_eq!(
			next_base_fee(U256::from(1_000u64), f64::NAN, ELASTICITY),
			U256::from(1_000u64)
		);
	}
}
