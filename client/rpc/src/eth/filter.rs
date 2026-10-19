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

use std::{
	collections::{BTreeMap, HashSet},
	marker::PhantomData,
	sync::Arc,
	time::{Duration, Instant},
};

use ethereum::BlockV3 as EthereumBlock;
use ethereum_types::{H256, U256};
use jsonrpsee::core::{async_trait, RpcResult};
// Substrate
use sc_client_api::backend::{Backend, StorageProvider};
use sc_transaction_pool_api::{InPoolTransaction, TransactionPool};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_core::hashing::keccak_256;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, NumberFor, One, Saturating, UniqueSaturatedInto},
};
// Frontier
use fc_rpc_core::{types::*, EthFilterApiServer};
use fp_rpc::{EthereumRuntimeRPCApi, TransactionStatus};

use crate::{cache::EthBlockDataCacheTask, frontier_backend_client, internal_err};

/// Maximum number of addresses a log filter may match on.
const MAX_FILTER_ADDRESSES: usize = 1024;
/// Maximum number of alternatives a single log filter topic position may hold.
const MAX_FILTER_TOPIC_OPTIONS: usize = 1024;
/// Default budget for a single filter poll response, matching the RPC server's default
/// maximum response size (15 MiB).
const DEFAULT_MAX_RESPONSE_BYTES: usize = 15 * 1024 * 1024;
/// Upper bound on the serialized size of one block or transaction hash in a response.
const HASH_JSON_BYTES: usize = 70;

fn variadic_len<T: sp_runtime::DeserializeOwned>(value: &VariadicValue<T>) -> usize {
	match value {
		VariadicValue::Single(_) => 1,
		VariadicValue::Multiple(items) => items.len(),
		_ => 0,
	}
}

/// Rejects log filters whose address or topic lists exceed the configured limits.
fn validate_filter_size(filter: &Filter) -> RpcResult<()> {
	if filter.address.as_ref().map_or(0, variadic_len) > MAX_FILTER_ADDRESSES {
		return Err(internal_err(format!(
			"too many filter addresses (maximum {MAX_FILTER_ADDRESSES})"
		)));
	}
	if filter
		.topics()
		.iter()
		.any(|topic| variadic_len(topic) > MAX_FILTER_TOPIC_OPTIONS)
	{
		return Err(internal_err(format!(
			"too many filter topic options (maximum {MAX_FILTER_TOPIC_OPTIONS})"
		)));
	}
	Ok(())
}

/// Rejects block-hash log queries whose result exceeds `max_past_logs`, matching range queries.
fn ensure_within_max_past_logs(logs: &[Log], max_past_logs: u32) -> RpcResult<()> {
	if logs.len() > max_past_logs as usize {
		return Err(internal_err(format!(
			"query returned more than {max_past_logs} results"
		)));
	}
	Ok(())
}

/// Upper bound on the JSON-serialized size of a log: fixed fields plus its topics and data.
fn log_json_bytes(log: &Log) -> usize {
	512usize
		.saturating_add(log.topics.len().saturating_mul(HASH_JSON_BYTES))
		.saturating_add(log.data.0.len().saturating_mul(2))
}

/// Trims a poll's logs, which must be ordered by block number, to whole blocks fitting
/// `max_bytes`.
///
/// Returns the retained logs and, if any were dropped, the first block number left out, so the
/// cursor can stop there and the next poll picks those blocks up. Fails if the first block
/// alone does not fit, as no progress can be made.
fn cap_logs_to_response_size(
	mut logs: Vec<Log>,
	max_bytes: usize,
) -> RpcResult<(Vec<Log>, Option<u64>)> {
	let mut total = 0usize;
	let mut cutoff = None;
	for log in &logs {
		total = total.saturating_add(log_json_bytes(log));
		if total > max_bytes {
			cutoff = Some(log.block_number.unwrap_or_default().low_u64());
			break;
		}
	}
	let Some(cutoff) = cutoff else {
		return Ok((logs, None));
	};
	logs.retain(|log| log.block_number.unwrap_or_default().low_u64() < cutoff);
	if logs.is_empty() {
		return Err(internal_err(format!(
			"logs of block {cutoff} exceed the maximum response size of {max_bytes} bytes"
		)));
	}
	Ok((logs, Some(cutoff)))
}

/// Moves a filter's poll cursor forward to `next`, once its changes were retrieved.
///
/// The cursor never moves backwards, so concurrent polls of the same filter cannot rewind it.
fn advance_filter_cursor(filter_pool: &FilterPool, key: U256, next: u64) {
	if let Ok(mut locked) = filter_pool.lock() {
		if let Some(item) = locked.get_mut(&key) {
			let current = item.last_poll.to_min_block_num().unwrap_or_default();
			if next > current {
				item.last_poll = BlockNumberOrHash::Num(next);
			}
		}
	}
}

pub struct EthFilter<B: BlockT, C, BE, P> {
	client: Arc<C>,
	backend: Arc<dyn fc_api::Backend<B>>,
	pool: Arc<P>,
	filter_pool: FilterPool,
	max_stored_filters: usize,
	max_past_logs: u32,
	max_block_range: u32,
	max_response_bytes: usize,
	block_data_cache: Arc<EthBlockDataCacheTask<B>>,
	_marker: PhantomData<BE>,
}

impl<B: BlockT, C, BE, P: TransactionPool> EthFilter<B, C, BE, P> {
	pub fn new(
		client: Arc<C>,
		backend: Arc<dyn fc_api::Backend<B>>,
		pool: Arc<P>,
		filter_pool: FilterPool,
		max_stored_filters: usize,
		max_past_logs: u32,
		max_block_range: u32,
		block_data_cache: Arc<EthBlockDataCacheTask<B>>,
	) -> Self {
		Self {
			client,
			backend,
			pool,
			filter_pool,
			max_stored_filters,
			max_past_logs,
			max_block_range,
			max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
			block_data_cache,
			_marker: PhantomData,
		}
	}

	/// Sets the response size budget for filter polls. Should match the RPC server's maximum
	/// response size, so an oversized poll is trimmed instead of rejected after its cursor moved.
	pub fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
		self.max_response_bytes = max_response_bytes;
		self
	}
}

impl<B, C, BE, P> EthFilter<B, C, BE, P>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + 'static,
	P: TransactionPool<Block = B, Hash = B::Hash> + 'static,
{
	fn create_filter(&self, filter_type: FilterType) -> RpcResult<U256> {
		let info = self.client.info();
		let best_hash = info.best_hash;
		let best_number = UniqueSaturatedInto::<u64>::unique_saturated_into(info.best_number);
		// Reject Log filters whose block range exceeds `max_block_range`, mirroring the
		// `eth_getLogs` cap so it cannot be bypassed via the filter API (upstream #1824).
		if let FilterType::Log(ref filter) = filter_type {
			validate_filter_size(filter)?;
			let from_number = filter
				.from_block
				.and_then(|v| v.to_min_block_num())
				.map(UniqueSaturatedInto::<u64>::unique_saturated_into)
				.unwrap_or(best_number);
			let to_number = filter
				.to_block
				.and_then(|v| v.to_min_block_num())
				.map(UniqueSaturatedInto::<u64>::unique_saturated_into)
				.unwrap_or(best_number);
			if to_number.saturating_sub(from_number) > self.max_block_range.into() {
				return Err(internal_err(format!(
					"block range is too wide (maximum {})",
					self.max_block_range
				)));
			}
		}
		let pool = self.filter_pool.clone();
		let response = if let Ok(locked) = &mut pool.lock() {
			if locked.len() >= self.max_stored_filters {
				return Err(internal_err(format!(
					"Filter pool is full (limit {:?}).",
					self.max_stored_filters
				)));
			}
			let pending_transaction_hashes = if let FilterType::PendingTransaction = filter_type {
				let txs_ready = self
					.pool
					.ready()
					.map(|in_pool_tx| in_pool_tx.data().as_ref().clone())
					.collect();
				// Use the runtime to match the (here) opaque extrinsics against ethereum transactions.
				let api = self.client.runtime_api();
				api.extrinsic_filter(best_hash, txs_ready)
					.map_err(|err| {
						internal_err(format!("fetch ready transactions failed: {err:?}"))
					})?
					.into_iter()
					.map(|tx| tx.hash())
					.collect::<HashSet<_>>()
			} else {
				HashSet::new()
			};

			// Random, non-zero identifiers that fit the `Index` (usize) used by the filter
			// methods, so an identifier reveals nothing about other filters.
			let key = loop {
				let candidate = U256::from(rand::random::<u64>() as usize);
				if !candidate.is_zero() && !locked.contains_key(&candidate) {
					break candidate;
				}
			};
			locked.insert(
				key,
				FilterPoolItem {
					last_poll: BlockNumberOrHash::Num(best_number),
					filter_type,
					at_block: best_number,
					pending_transaction_hashes,
				},
			);
			Ok(key)
		} else {
			Err(internal_err("Filter pool is not available."))
		};
		response
	}
}

#[async_trait]
impl<B, C, BE, P> EthFilterApiServer for EthFilter<B, C, BE, P>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
	P: TransactionPool<Block = B, Hash = B::Hash> + 'static,
{
	fn new_filter(&self, filter: Filter) -> RpcResult<U256> {
		self.create_filter(FilterType::Log(filter))
	}

	fn new_block_filter(&self) -> RpcResult<U256> {
		self.create_filter(FilterType::Block)
	}

	fn new_pending_transaction_filter(&self) -> RpcResult<U256> {
		self.create_filter(FilterType::PendingTransaction)
	}

	async fn filter_changes(&self, index: Index) -> RpcResult<FilterChanges> {
		// There are multiple branches that needs to return async blocks.
		// Also, each branch need to (synchronously) do stuff with the pool
		// (behind a lock), and the lock should be released before entering
		// an async block.
		//
		// To avoid issues with multiple async blocks (having different
		// anonymous types) we collect all necessary data in this enum then have
		// a single async block.
		enum FuturePath<B: BlockT> {
			Block {
				last: u64,
				next: u64,
			},
			PendingTransaction {
				new_hashes: Vec<H256>,
			},
			Log {
				filter: Filter,
				from_number: NumberFor<B>,
				current_number: NumberFor<B>,
				next: u64,
			},
			Error(jsonrpsee::types::ErrorObjectOwned),
		}

		let key = U256::from(index.value());
		let info = self.client.info();
		let best_hash = info.best_hash;
		let best_number = UniqueSaturatedInto::<u64>::unique_saturated_into(info.best_number);
		let pool = self.filter_pool.clone();
		// Try to lock.
		let path = if let Ok(locked) = &mut pool.lock() {
			// Try to get key.
			if let Some(pool_item) = locked.get(&key).cloned() {
				match &pool_item.filter_type {
					// For each block created since last poll, get a vector of ethereum hashes.
					// At most `max_block_range` blocks are returned per poll; the cursor is only
					// advanced once they were retrieved, so a lagging client catches up over
					// several polls without skipping any block.
					FilterType::Block => {
						let last = pool_item.last_poll.to_min_block_num().unwrap_or_default();
						let range_end = last
							.saturating_add(u64::from(self.max_block_range))
							.saturating_add(1);
						let size_end = last.saturating_add(
							(self.max_response_bytes / HASH_JSON_BYTES).max(1) as u64,
						);
						let next = best_number.saturating_add(1).min(range_end).min(size_end);

						FuturePath::<B>::Block { last, next }
					}
					FilterType::PendingTransaction => {
						let previous_hashes = pool_item.pending_transaction_hashes;
						let txs_ready = self
							.pool
							.ready()
							.map(|in_pool_tx| in_pool_tx.data().as_ref().clone())
							.collect();
						// Use the runtime to match the (here) opaque extrinsics against ethereum transactions.
						let api = self.client.runtime_api();
						let current_hashes = api
							.extrinsic_filter(best_hash, txs_ready)
							.map_err(|err| {
								internal_err(format!("fetch ready transactions failed: {err:?}"))
							})?
							.into_iter()
							.map(|tx| tx.hash())
							.collect::<HashSet<_>>();

						// Update filter `last_poll`.
						locked.insert(
							key,
							FilterPoolItem {
								last_poll: BlockNumberOrHash::Num(best_number + 1),
								filter_type: pool_item.filter_type.clone(),
								at_block: pool_item.at_block,
								pending_transaction_hashes: current_hashes.clone(),
							},
						);

						let mew_hashes = current_hashes
							.difference(&previous_hashes)
							.collect::<HashSet<&H256>>();
						FuturePath::PendingTransaction {
							new_hashes: mew_hashes.into_iter().copied().collect(),
						}
					}
					// For each event since last poll, get a vector of ethereum logs.
					// Same bounding and cursor rules as block filters.
					FilterType::Log(filter) => {
						// Either the filter-specific `to` block or best block.
						let best_number = self.client.info().best_number;
						let mut current_number = filter
							.to_block
							.and_then(|v| v.to_min_block_num())
							.map(|s| s.unique_saturated_into())
							.unwrap_or(best_number);

						if current_number > best_number {
							current_number = best_number;
						}

						// The from clause is the max(last_poll, filter_from).
						let last_poll: NumberFor<B> = pool_item
							.last_poll
							.to_min_block_num()
							.unwrap_or_default()
							.unique_saturated_into();

						let filter_from = filter
							.from_block
							.and_then(|v| v.to_min_block_num())
							.map(|s| s.unique_saturated_into())
							.unwrap_or(last_poll);

						let from_number = std::cmp::max(last_poll, filter_from);
						let range_end =
							from_number.saturating_add(NumberFor::<B>::from(self.max_block_range));
						if current_number > range_end {
							current_number = range_end;
						}
						let next =
							UniqueSaturatedInto::<u64>::unique_saturated_into(current_number)
								.saturating_add(1);

						// Build the response.
						FuturePath::Log {
							filter: filter.clone(),
							from_number,
							current_number,
							next,
						}
					}
				}
			} else {
				FuturePath::Error(internal_err(format!("Filter id {key:?} does not exist.")))
			}
		} else {
			FuturePath::Error(internal_err("Filter pool is not available."))
		};

		let client = Arc::clone(&self.client);
		let backend = Arc::clone(&self.backend);
		let block_data_cache = Arc::clone(&self.block_data_cache);
		let max_past_logs = self.max_past_logs;
		let max_response_bytes = self.max_response_bytes;

		match path {
			FuturePath::Error(err) => Err(err),
			FuturePath::Block { last, next } => {
				let mut ethereum_hashes: Vec<H256> = Vec::new();
				for n in last..next {
					let id = BlockId::Number(n.unique_saturated_into());
					let substrate_hash = client
						.expect_block_hash_from_id(&id)
						.map_err(|_| internal_err(format!("Expect block number from id: {id}")))?;

					// Fail rather than skip, so the cursor stays put and the block is retried.
					let block = block_data_cache
						.current_block(substrate_hash)
						.await
						.ok_or_else(|| {
							internal_err(format!("block data unavailable for block {n}"))
						})?;
					ethereum_hashes.push(block.header.hash());
				}
				advance_filter_cursor(&pool, key, next);
				Ok(FilterChanges::Hashes(ethereum_hashes))
			}
			FuturePath::PendingTransaction { new_hashes } => Ok(FilterChanges::Hashes(new_hashes)),
			FuturePath::Log {
				filter,
				from_number,
				current_number,
				next,
			} => {
				let logs = if backend.is_indexed() {
					filter_range_logs_indexed(
						client.as_ref(),
						backend.log_indexer(),
						&block_data_cache,
						max_past_logs,
						&filter,
						from_number,
						current_number,
						true,
					)
					.await?
				} else {
					filter_range_logs(
						client.as_ref(),
						&block_data_cache,
						max_past_logs,
						&filter,
						from_number,
						current_number,
						true,
					)
					.await?
				};

				let (logs, cutoff) = cap_logs_to_response_size(logs, max_response_bytes)?;
				advance_filter_cursor(&pool, key, cutoff.unwrap_or(next));
				Ok(FilterChanges::Logs(logs))
			}
		}
	}

	async fn filter_logs(&self, index: Index) -> RpcResult<Vec<Log>> {
		let key = U256::from(index.value());
		let pool = self.filter_pool.clone();

		// We want to get the filter, while releasing the pool lock outside
		// of the async block.
		let filter_result: RpcResult<Filter> = (|| {
			let pool = pool
				.lock()
				.map_err(|_| internal_err("Filter pool is not available."))?;

			let pool_item = pool
				.get(&key)
				.ok_or_else(|| internal_err(format!("Filter id {key:?} does not exist.")))?;

			match &pool_item.filter_type {
				FilterType::Log(filter) => Ok(filter.clone()),
				_ => Err(internal_err(format!(
					"Filter id {key:?} is not a Log filter."
				))),
			}
		})();

		let client = Arc::clone(&self.client);
		let backend = Arc::clone(&self.backend);
		let block_data_cache = Arc::clone(&self.block_data_cache);
		let max_past_logs = self.max_past_logs;

		let filter = filter_result?;

		let best_number = client.info().best_number;
		let mut current_number = filter
			.to_block
			.and_then(|v| v.to_min_block_num())
			.map(|s| s.unique_saturated_into())
			.unwrap_or(best_number);

		if current_number > best_number {
			current_number = best_number;
		}

		let from_number = filter
			.from_block
			.and_then(|v| v.to_min_block_num())
			.map(|s| s.unique_saturated_into())
			.unwrap_or(best_number);

		if current_number.saturating_sub(from_number) > self.max_block_range.into() {
			return Err(internal_err(format!(
				"block range is too wide (maximum {})",
				self.max_block_range
			)));
		}

		let logs = if backend.is_indexed() {
			filter_range_logs_indexed(
				client.as_ref(),
				backend.log_indexer(),
				&block_data_cache,
				max_past_logs,
				&filter,
				from_number,
				current_number,
				false,
			)
			.await?
		} else {
			filter_range_logs(
				client.as_ref(),
				&block_data_cache,
				max_past_logs,
				&filter,
				from_number,
				current_number,
				false,
			)
			.await?
		};
		Ok(logs)
	}

	fn uninstall_filter(&self, index: Index) -> RpcResult<bool> {
		let key = U256::from(index.value());
		let pool = self.filter_pool.clone();
		// Try to lock.
		let response = if let Ok(locked) = &mut pool.lock() {
			if locked.remove(&key).is_some() {
				Ok(true)
			} else {
				Err(internal_err(format!("Filter id {key:?} does not exist.")))
			}
		} else {
			Err(internal_err("Filter pool is not available."))
		};
		response
	}

	async fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
		validate_filter_size(&filter)?;
		let client = Arc::clone(&self.client);
		let block_data_cache = Arc::clone(&self.block_data_cache);
		let backend = Arc::clone(&self.backend);
		let max_past_logs = self.max_past_logs;

		let mut logs = Vec::new();
		if let Some(hash) = filter.block_hash {
			let substrate_hash = match frontier_backend_client::load_hash::<B, C>(
				client.as_ref(),
				backend.as_ref(),
				hash,
			)
			.await
			.map_err(|err| internal_err(format!("{err:?}")))?
			{
				Some(hash) => hash,
				_ => return Err(crate::err(-32000, "unknown block", None)),
			};

			let block = block_data_cache.current_block(substrate_hash).await;
			let statuses = block_data_cache
				.current_transaction_statuses(substrate_hash)
				.await;
			if let (Some(block), Some(statuses)) = (block, statuses) {
				logs = filter_block_logs(&filter, block, statuses);
				ensure_within_max_past_logs(&logs, max_past_logs)?;
			}
		} else {
			let best_number = client.info().best_number;
			let mut current_number = filter
				.to_block
				.and_then(|v| v.to_min_block_num())
				.map(|s| s.unique_saturated_into())
				.unwrap_or(best_number);

			if current_number > best_number {
				current_number = best_number;
			}

			let from_number = filter
				.from_block
				.and_then(|v| v.to_min_block_num())
				.map(|s| s.unique_saturated_into())
				.unwrap_or(best_number);

			let block_range = current_number.saturating_sub(from_number);
			if block_range > self.max_block_range.into() {
				return Err(internal_err(format!(
					"block range is too wide (maximum {})",
					self.max_block_range
				)));
			}

			logs = if backend.is_indexed() {
				filter_range_logs_indexed(
					client.as_ref(),
					backend.log_indexer(),
					&block_data_cache,
					max_past_logs,
					&filter,
					from_number,
					current_number,
					false,
				)
				.await?
			} else {
				filter_range_logs(
					client.as_ref(),
					&block_data_cache,
					max_past_logs,
					&filter,
					from_number,
					current_number,
					false,
				)
				.await?
			};
		}
		Ok(logs)
	}
}

async fn filter_range_logs_indexed<B, C, BE>(
	_client: &C,
	backend: &dyn fc_api::LogIndexerBackend<B>,
	block_data_cache: &EthBlockDataCacheTask<B>,
	max_past_logs: u32,
	filter: &Filter,
	from: NumberFor<B>,
	to: NumberFor<B>,
	require_block_data: bool,
) -> RpcResult<Vec<Log>>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
{
	let timer_start = Instant::now();
	let timer_prepare = Instant::now();

	// Max request duration of 10 seconds.
	let max_duration = Duration::from_secs(10);
	let begin_request = Instant::now();

	// Normalize filter data
	let addresses = match &filter.address {
		Some(VariadicValue::Single(item)) => vec![*item],
		Some(VariadicValue::Multiple(items)) => items.clone(),
		_ => vec![],
	};
	let topics = filter
		.topics()
		.iter()
		.map(|flat| match flat {
			VariadicValue::Single(item) => vec![*item],
			VariadicValue::Multiple(items) => items.clone(),
			_ => vec![],
		})
		.collect::<Vec<Vec<H256>>>();

	let time_prepare = timer_prepare.elapsed().as_millis();
	let timer_fetch = Instant::now();

	let mut logs_to_return = Vec::new();
	{
		let logs = backend
			.filter_logs(
				UniqueSaturatedInto::<u64>::unique_saturated_into(from),
				UniqueSaturatedInto::<u64>::unique_saturated_into(to),
				addresses,
				topics,
			)
			.await
			.map_err(|err| internal_err(format!("log query failed: {err}")))?;
		let time_fetch = timer_fetch.elapsed().as_millis();
		let timer_post = Instant::now();

		let mut statuses_cache: BTreeMap<B::Hash, Option<Vec<TransactionStatus>>> = BTreeMap::new();

		for log in logs.iter() {
			let substrate_hash = log.substrate_block_hash;

			let ethereum_block_hash = log.ethereum_block_hash;
			let block_number = log.block_number;
			let db_transaction_index = log.transaction_index;
			let db_log_index = log.log_index;

			let statuses = if let Some(statuses) = statuses_cache.get(&log.substrate_block_hash) {
				statuses.clone()
			} else {
				let statuses = block_data_cache
					.current_transaction_statuses(substrate_hash)
					.await;
				statuses_cache.insert(log.substrate_block_hash, statuses.clone());
				statuses
			};
			if statuses.is_none() && require_block_data {
				return Err(internal_err(format!(
					"block data unavailable for block {block_number}"
				)));
			}
			if let Some(statuses) = statuses {
				let mut block_log_index: u32 = 0;
				for status in statuses.iter() {
					let mut transaction_log_index: u32 = 0;
					let transaction_hash = status.transaction_hash;
					let transaction_index = status.transaction_index;
					for ethereum_log in &status.logs {
						if transaction_index == db_transaction_index
							&& transaction_log_index == db_log_index
						{
							logs_to_return.push(Log {
								address: ethereum_log.address,
								topics: ethereum_log.topics.clone(),
								data: Bytes(ethereum_log.data.clone()),
								block_hash: Some(ethereum_block_hash),
								block_number: Some(U256::from(block_number)),
								transaction_hash: Some(transaction_hash),
								transaction_index: Some(U256::from(transaction_index)),
								log_index: Some(U256::from(block_log_index)),
								transaction_log_index: Some(U256::from(transaction_log_index)),
								removed: false,
							});
						}
						transaction_log_index += 1;
						block_log_index += 1;
					}
				}
			}
			// Check for restrictions
			if logs_to_return.len() as u32 > max_past_logs {
				return Err(internal_err(format!(
					"query returned more than {max_past_logs} results",
				)));
			}
			if begin_request.elapsed() > max_duration {
				return Err(internal_err(format!(
					"query timeout of {} seconds exceeded",
					max_duration.as_secs()
				)));
			}
		}

		let time_post = timer_post.elapsed().as_millis();

		log::info!(
			target: "frontier-sql",
			"OUTER-TIMER fetch={time_fetch}, post={time_post}"
		);
	}

	log::info!(
		target: "frontier-sql",
		"OUTER-TIMER start={}, prepare={}, all_fetch = {}",
		timer_start.elapsed().as_millis(),
		time_prepare,
		timer_fetch.elapsed().as_millis(),
	);
	Ok(logs_to_return)
}

async fn filter_range_logs<B, C, BE>(
	client: &C,
	block_data_cache: &EthBlockDataCacheTask<B>,
	max_past_logs: u32,
	filter: &Filter,
	from: NumberFor<B>,
	to: NumberFor<B>,
	require_block_data: bool,
) -> RpcResult<Vec<Log>>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
{
	// Max request duration of 10 seconds.
	let max_duration = Duration::from_secs(10);
	let begin_request = Instant::now();

	let mut current_number = from;

	// Pre-calculate BloomInput for reuse.
	let address_bloom_filter = FilteredParams::address_bloom_filter(&filter.address);
	let topics_bloom_filter = FilteredParams::topics_bloom_filter(&filter.topics());

	let mut logs = Vec::new();
	while current_number <= to {
		let id = BlockId::Number(current_number);
		let substrate_hash = client
			.expect_block_hash_from_id(&id)
			.map_err(|_| internal_err(format!("Expect block number from id: {id}")))?;

		let block = block_data_cache.current_block(substrate_hash).await;
		if block.is_none() && require_block_data {
			return Err(internal_err(format!(
				"block data unavailable for block {id}"
			)));
		}

		if let Some(block) = block {
			if FilteredParams::address_in_bloom(block.header.logs_bloom, &address_bloom_filter)
				&& FilteredParams::topics_in_bloom(block.header.logs_bloom, &topics_bloom_filter)
			{
				let statuses = block_data_cache
					.current_transaction_statuses(substrate_hash)
					.await;
				match statuses {
					Some(statuses) => logs.extend(filter_block_logs(filter, block, statuses)),
					None if require_block_data => {
						return Err(internal_err(format!(
							"block data unavailable for block {id}"
						)));
					}
					None => {}
				}
			}
		}
		// Check for restrictions
		if logs.len() as u32 > max_past_logs {
			return Err(internal_err(format!(
				"query returned more than {max_past_logs} results"
			)));
		}
		if begin_request.elapsed() > max_duration {
			return Err(internal_err(format!(
				"query timeout of {} seconds exceeded",
				max_duration.as_secs()
			)));
		}
		if current_number == to {
			break;
		} else {
			current_number = current_number.saturating_add(One::one());
		}
	}
	Ok(logs)
}

pub(crate) fn filter_block_logs(
	filter: &Filter,
	block: EthereumBlock,
	transaction_statuses: Vec<TransactionStatus>,
) -> Vec<Log> {
	let params = FilteredParams::new(filter.clone());
	let mut block_log_index: u32 = 0;
	let block_hash = H256::from(keccak_256(&rlp::encode(&block.header)));

	let mut logs = Vec::new();
	for status in transaction_statuses.iter() {
		let mut transaction_log_index: u32 = 0;
		let transaction_hash = status.transaction_hash;
		for ethereum_log in &status.logs {
			let mut log = Log {
				address: ethereum_log.address,
				topics: ethereum_log.topics.clone(),
				data: Bytes(ethereum_log.data.clone()),
				block_hash: None,
				block_number: None,
				transaction_hash: None,
				transaction_index: None,
				log_index: None,
				transaction_log_index: None,
				removed: false,
			};

			let topics_match = filter.topics().is_empty() || params.filter_topics(&log.topics);
			let address_match = filter
				.address
				.as_ref()
				.is_none_or(|_| params.filter_address(&log.address));
			if topics_match && address_match {
				log.block_hash = Some(block_hash);
				log.block_number = Some(block.header.number);
				log.transaction_hash = Some(transaction_hash);
				log.transaction_index = Some(U256::from(status.transaction_index));
				log.log_index = Some(U256::from(block_log_index));
				log.transaction_log_index = Some(U256::from(transaction_log_index));
				logs.push(log);
			}
			transaction_log_index += 1;
			block_log_index += 1;
		}
	}
	logs
}

#[cfg(test)]
mod tests {
	use super::*;
	use ethereum_types::H160;
	use std::sync::Mutex;

	fn pool_with(key: U256, last_poll: u64) -> FilterPool {
		let pool: FilterPool = Arc::new(Mutex::new(BTreeMap::new()));
		pool.lock().unwrap().insert(
			key,
			FilterPoolItem {
				last_poll: BlockNumberOrHash::Num(last_poll),
				filter_type: FilterType::Block,
				at_block: last_poll,
				pending_transaction_hashes: HashSet::new(),
			},
		);
		pool
	}

	fn last_poll(pool: &FilterPool, key: U256) -> u64 {
		pool.lock().unwrap()[&key]
			.last_poll
			.to_min_block_num()
			.unwrap()
	}

	#[test]
	fn filter_cursor_only_moves_forward() {
		let key = U256::from(7);
		let pool = pool_with(key, 10);

		advance_filter_cursor(&pool, key, 20);
		assert_eq!(last_poll(&pool, key), 20);

		advance_filter_cursor(&pool, key, 15);
		assert_eq!(last_poll(&pool, key), 20);
	}

	#[test]
	fn filter_cursor_ignores_removed_filters() {
		let pool = pool_with(U256::from(7), 10);
		advance_filter_cursor(&pool, U256::from(8), 20);
		assert_eq!(pool.lock().unwrap().len(), 1);
		assert_eq!(last_poll(&pool, U256::from(7)), 10);
	}

	#[test]
	fn filter_size_limits() {
		let addresses = |n: u64| {
			Some(VariadicValue::Multiple(
				(0..n).map(H160::from_low_u64_be).collect(),
			))
		};

		let filter = Filter {
			address: addresses(MAX_FILTER_ADDRESSES as u64),
			..Default::default()
		};
		assert!(validate_filter_size(&filter).is_ok());

		let filter = Filter {
			address: addresses(MAX_FILTER_ADDRESSES as u64 + 1),
			..Default::default()
		};
		assert!(validate_filter_size(&filter).is_err());

		let options = |n: u64| VariadicValue::Multiple((0..n).map(H256::from_low_u64_be).collect());
		let filter = Filter {
			topics: Some(
				vec![options(1), options(MAX_FILTER_TOPIC_OPTIONS as u64 + 1)]
					.try_into()
					.unwrap(),
			),
			..Default::default()
		};
		assert!(validate_filter_size(&filter).is_err());
	}

	#[test]
	fn max_past_logs_applies_to_single_block_results() {
		let log = Log {
			address: H160::zero(),
			topics: vec![],
			data: Bytes(vec![]),
			block_hash: None,
			block_number: None,
			transaction_hash: None,
			transaction_index: None,
			log_index: None,
			transaction_log_index: None,
			removed: false,
		};
		let logs = vec![log; 3];
		assert!(ensure_within_max_past_logs(&logs, 3).is_ok());
		assert!(ensure_within_max_past_logs(&logs, 2).is_err());
	}

	fn log_in_block(number: u64) -> Log {
		Log {
			address: H160::zero(),
			topics: vec![H256::zero()],
			data: Bytes(vec![0u8; 100]),
			block_hash: None,
			block_number: Some(U256::from(number)),
			transaction_hash: None,
			transaction_index: None,
			log_index: None,
			transaction_log_index: None,
			removed: false,
		}
	}

	#[test]
	fn response_cap_keeps_logs_within_budget() {
		let logs = vec![log_in_block(1), log_in_block(2)];
		let (kept, cutoff) = cap_logs_to_response_size(logs, 10_000).unwrap();
		assert_eq!(kept.len(), 2);
		assert_eq!(cutoff, None);
	}

	#[test]
	fn response_cap_trims_to_whole_blocks() {
		let one = log_json_bytes(&log_in_block(0));

		// The budget ends inside block 2, so block 2 is dropped whole and the cursor stops there.
		let logs = vec![log_in_block(1), log_in_block(2), log_in_block(2)];
		let (kept, cutoff) = cap_logs_to_response_size(logs, 2 * one).unwrap();
		assert_eq!(kept.len(), 1);
		assert_eq!(cutoff, Some(2));

		// A budget ending inside the first block leaves nothing to return.
		let logs = vec![log_in_block(1), log_in_block(1)];
		assert!(cap_logs_to_response_size(logs, one).is_err());
	}
}
