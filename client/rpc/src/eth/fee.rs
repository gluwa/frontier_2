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

use ethereum_types::U256;
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
	let last_fee_per_gas =
		UniqueSaturatedInto::<u64>::unique_saturated_into(last_fee_per_gas) as f64;
	let new_base_fee = if last_gas_used > 0.5 {
		// Increase base gas
		let increase = ((last_gas_used - 0.5) * 2f64) * elasticity;
		(last_fee_per_gas + (last_fee_per_gas * increase)) as u64
	} else if last_gas_used < 0.5 {
		// Decrease base gas
		let decrease = ((0.5 - last_gas_used) * 2f64) * elasticity;
		(last_fee_per_gas - (last_fee_per_gas * decrease)) as u64
	} else {
		// Same base gas
		last_fee_per_gas as u64
	};
	U256::from(new_base_fee)
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
			let mut response = FeeHistory {
				oldest_block: U256::from(lowest),
				base_fee_per_gas: Vec::new(),
				gas_used_ratio: Vec::new(),
				reward: None,
			};
			let mut rewards = Vec::new();
			// Everything needed is copied out of the cache while holding the lock, which is released
			// before querying the runtime below.
			{
				let Ok(fee_history_cache) = self.fee_history_cache.lock() else {
					return Err(internal_err("Failed to read fee history cache."));
				};
				// Iterate over the requested block range.
				for n in lowest..highest + 1 {
					if let Some(block) = fee_history_cache.get(&n) {
						response.base_fee_per_gas.push(U256::from(block.base_fee));
						response.gas_used_ratio.push(block.gas_used_ratio);
						// If the request includes reward percentiles, get them from the cache.
						if let Some(ref requested_percentiles) = reward_percentiles {
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
			}
			if rewards.len() > 0 {
				response.reward = Some(rewards);
			}
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

#[cfg(test)]
mod tests;
