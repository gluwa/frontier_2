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

use ethereum_types::{H256, U256};
use jsonrpsee::core::RpcResult;
// Substrate
use sc_client_api::backend::{Backend, StorageProvider};
use sc_transaction_pool::ChainApi;
use sc_transaction_pool_api::InPoolTransaction;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, Zero};
// Frontier
use fc_rpc_core::types::*;
use fp_rpc::EthereumRuntimeRPCApi;

use crate::{
	eth::{
		rich_block_build, rich_block_build_with_parent_hash, rpc_compatible_block_hash, BlockInfo,
		Eth,
	},
	frontier_backend_client, internal_err,
};

const RPC_COMPATIBLE_HASH_CACHE_LIMIT: usize = 8192;
const RPC_COMPATIBLE_HASH_DERIVATION_LIMIT: usize = 8192;

impl<B, C, P, CT, BE, A, CIDP, EC> Eth<B, C, P, CT, BE, A, CIDP, EC>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
	A: ChainApi<Block = B>,
{
	fn cache_rpc_compatible_hash(&self, substrate_hash: B::Hash, compatible_hash: H256) {
		let substrate_hash_h256 = H256::from_slice(substrate_hash.as_ref());
		if let (Ok(mut cache), Ok(mut reverse_cache)) = (
			self.rpc_compatible_hash_cache.lock(),
			self.rpc_compatible_reverse_hash_cache.lock(),
		) {
			if cache.len() >= RPC_COMPATIBLE_HASH_CACHE_LIMIT
				|| reverse_cache.len() >= RPC_COMPATIBLE_HASH_CACHE_LIMIT
			{
				cache.clear();
				reverse_cache.clear();
			}

			cache.insert(substrate_hash_h256, compatible_hash);
			reverse_cache.insert(compatible_hash, substrate_hash);
		}
	}

	pub(super) async fn rpc_compatible_hash(&self, substrate_hash: B::Hash) -> Option<H256> {
		if let Ok(Some(hash)) = self
			.backend
			.rpc_compatible_hash_by_substrate_hash(&substrate_hash)
			.await
		{
			self.cache_rpc_compatible_hash(substrate_hash, hash);
			return Some(hash);
		}

		let mut stack = Vec::new();
		let mut cursor = substrate_hash;

		let mut parent_hash = loop {
			let cursor_h256 = H256::from_slice(cursor.as_ref());
			if let Some(cached_hash) = self
				.rpc_compatible_hash_cache
				.lock()
				.ok()
				.and_then(|cache| cache.get(&cursor_h256).cloned())
			{
				break Some(cached_hash);
			}

			let header = self.client.header(cursor).ok().flatten()?;
			let forced_parent_hash = self
				.forced_parent_hashes
				.as_ref()
				.and_then(|parent_hashes| parent_hashes.get(&cursor_h256).cloned());
			stack.push((cursor, forced_parent_hash));
			if stack.len() > RPC_COMPATIBLE_HASH_DERIVATION_LIMIT {
				return None;
			}

			if header.number().is_zero() {
				break None;
			}

			if let Some(forced_parent_hash) = forced_parent_hash {
				break Some(forced_parent_hash);
			}

			cursor = *header.parent_hash();
		};

		for (ancestor_hash, forced_parent_hash) in stack.into_iter().rev() {
			let block = self.block_data_cache.current_block(ancestor_hash).await?;
			let compatible_hash =
				rpc_compatible_block_hash(&block, forced_parent_hash.or(parent_hash));
			self.cache_rpc_compatible_hash(ancestor_hash, compatible_hash);

			parent_hash = Some(compatible_hash);
		}

		parent_hash
	}

	pub(super) async fn rpc_compatible_parent_hash(&self, substrate_hash: B::Hash) -> Option<H256> {
		let substrate_hash_h256 = H256::from_slice(substrate_hash.as_ref());
		if let Some(parent_hash) = self
			.forced_parent_hashes
			.as_ref()
			.and_then(|parent_hashes| parent_hashes.get(&substrate_hash_h256).cloned())
		{
			return Some(parent_hash);
		}

		let header = self.client.header(substrate_hash).ok().flatten()?;
		if header.number().is_zero() {
			return None;
		}

		self.rpc_compatible_hash(*header.parent_hash()).await
	}

	pub(super) async fn block_info_by_rpc_compatible_hash(
		&self,
		hash: H256,
	) -> RpcResult<BlockInfo<B::Hash>> {
		let substrate_hash = self
			.rpc_compatible_reverse_hash_cache
			.lock()
			.ok()
			.and_then(|cache| cache.get(&hash).cloned());

		if let Some(substrate_hash) = substrate_hash {
			if frontier_backend_client::is_canon::<B, C>(self.client.as_ref(), substrate_hash) {
				return self.block_info_by_substrate_hash(substrate_hash).await;
			}
		}

		let substrate_hashes = self
			.backend
			.rpc_compatible_block_hash(&hash)
			.await
			.map_err(|err| internal_err(format!("fetch aux store failed: {:?}", err)))?;
		if let Some(substrate_hashes) = substrate_hashes {
			for substrate_hash in substrate_hashes {
				if frontier_backend_client::is_canon::<B, C>(self.client.as_ref(), substrate_hash) {
					self.cache_rpc_compatible_hash(substrate_hash, hash);
					let block_info = self.block_info_by_substrate_hash(substrate_hash).await?;
					if block_info.block.is_some() {
						return Ok(block_info);
					}
				}
			}
		}

		Ok(BlockInfo::default())
	}

	pub async fn block_by_hash(&self, hash: H256, full: bool) -> RpcResult<Option<RichBlock>> {
		let mut block_info = self.block_info_by_eth_block_hash(hash).await?;
		if block_info.block.is_none() {
			block_info = self.block_info_by_rpc_compatible_hash(hash).await?;
		}

		let BlockInfo {
			block,
			statuses,
			substrate_hash,
			base_fee,
			..
		} = block_info;

		match (block, statuses) {
			(Some(block), Some(statuses)) => {
				let parent_hash = self.rpc_compatible_parent_hash(substrate_hash).await;
				let hash = self.rpc_compatible_hash(substrate_hash).await;

				let rich_block = rich_block_build_with_parent_hash(
					block,
					statuses.into_iter().map(Option::Some).collect(),
					hash,
					full,
					Some(base_fee),
					false,
					parent_hash,
				);

				Ok(Some(rich_block))
			}
			_ => Ok(None),
		}
	}

	pub async fn block_by_number(
		&self,
		number_or_hash: BlockNumberOrHash,
		full: bool,
	) -> RpcResult<Option<RichBlock>> {
		let client = Arc::clone(&self.client);
		let block_data_cache = Arc::clone(&self.block_data_cache);
		let backend = Arc::clone(&self.backend);
		let graph = Arc::clone(&self.graph);

		match frontier_backend_client::native_block_id::<B, C>(
			client.as_ref(),
			backend.as_ref(),
			Some(number_or_hash),
		)
		.await?
		{
			Some(id) => {
				let substrate_hash = client
					.expect_block_hash_from_id(&id)
					.map_err(|_| internal_err(format!("Expect block number from id: {}", id)))?;

				let block = block_data_cache.current_block(substrate_hash).await;
				let statuses = block_data_cache
					.current_transaction_statuses(substrate_hash)
					.await;

				let base_fee = client.runtime_api().gas_price(substrate_hash).ok();

				match (block, statuses) {
					(Some(block), Some(statuses)) => {
						let parent_hash = self.rpc_compatible_parent_hash(substrate_hash).await;
						let hash = self.rpc_compatible_hash(substrate_hash).await;

						let rich_block = rich_block_build_with_parent_hash(
							block,
							statuses.into_iter().map(Option::Some).collect(),
							hash,
							full,
							base_fee,
							false,
							parent_hash,
						);

						Ok(Some(rich_block))
					}
					_ => Ok(None),
				}
			}
			None if number_or_hash == BlockNumberOrHash::Pending => {
				let api = client.runtime_api();
				let best_hash = client.info().best_hash;

				// Get current in-pool transactions
				let mut xts: Vec<<B as BlockT>::Extrinsic> = Vec::new();
				// ready validated pool
				xts.extend(
					graph
						.validated_pool()
						.ready()
						.map(|in_pool_tx| in_pool_tx.data().clone())
						.collect::<Vec<<B as BlockT>::Extrinsic>>(),
				);

				// future validated pool
				xts.extend(
					graph
						.validated_pool()
						.futures()
						.iter()
						.map(|(_hash, extrinsic)| extrinsic.clone())
						.collect::<Vec<<B as BlockT>::Extrinsic>>(),
				);

				let (block, statuses) = api
					.pending_block(best_hash, xts)
					.map_err(|_| internal_err(format!("Runtime access error at {}", best_hash)))?;

				let base_fee = api.gas_price(best_hash).ok();

				match (block, statuses) {
					(Some(block), Some(statuses)) => Ok(Some(rich_block_build(
						block,
						statuses.into_iter().map(Option::Some).collect(),
						None,
						full,
						base_fee,
						true,
					))),
					_ => Ok(None),
				}
			}
			None => Ok(None),
		}
	}

	pub async fn block_transaction_count_by_hash(&self, hash: H256) -> RpcResult<Option<U256>> {
		let mut blockinfo = self.block_info_by_eth_block_hash(hash).await?;
		if blockinfo.block.is_none() {
			blockinfo = self.block_info_by_rpc_compatible_hash(hash).await?;
		}
		match blockinfo.block {
			Some(block) => Ok(Some(U256::from(block.transactions.len()))),
			None => Ok(None),
		}
	}

	pub async fn block_transaction_count_by_number(
		&self,
		number_or_hash: BlockNumberOrHash,
	) -> RpcResult<Option<U256>> {
		if let BlockNumberOrHash::Pending = number_or_hash {
			// get the pending transactions count
			return Ok(Some(U256::from(
				self.graph.validated_pool().ready().count(),
			)));
		}

		let block_info = self.block_info_by_number(number_or_hash).await?;
		match block_info.block {
			Some(block) => Ok(Some(U256::from(block.transactions.len()))),
			None => Ok(None),
		}
	}

	pub async fn block_transaction_receipts(
		&self,
		number_or_hash: BlockNumberOrHash,
	) -> RpcResult<Option<Vec<Receipt>>> {
		let block_info = self.block_info_by_number(number_or_hash).await?;
		let Some(statuses) = block_info.clone().statuses else {
			return Ok(None);
		};

		let mut receipts = Vec::new();
		let transactions: Vec<(H256, usize)> = statuses
			.iter()
			.map(|tx| (tx.transaction_hash, tx.transaction_index as usize))
			.collect();
		for (hash, index) in transactions {
			if let Some(receipt) = self.transaction_receipt(&block_info, hash, index).await? {
				receipts.push(receipt);
			}
		}

		Ok(Some(receipts))
	}

	pub fn block_uncles_count_by_hash(&self, _: H256) -> RpcResult<U256> {
		Ok(U256::zero())
	}

	pub fn block_uncles_count_by_number(&self, _: BlockNumberOrHash) -> RpcResult<U256> {
		Ok(U256::zero())
	}

	pub fn uncle_by_block_hash_and_index(&self, _: H256, _: Index) -> RpcResult<Option<RichBlock>> {
		Ok(None)
	}

	pub fn uncle_by_block_number_and_index(
		&self,
		_: BlockNumberOrHash,
		_: Index,
	) -> RpcResult<Option<RichBlock>> {
		Ok(None)
	}
}
