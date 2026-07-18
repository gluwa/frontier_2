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

#![allow(clippy::too_many_arguments)]

mod worker;

pub use worker::MappingSyncWorker;

use std::sync::Arc;

// Substrate
use sc_client_api::backend::{Backend, StorageProvider};
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_blockchain::{Backend as _, HeaderBackend};
use sp_consensus::SyncOracle;
use sp_core::{hashing::keccak_256, H256};
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, Zero};
// Frontier
use fc_storage::StorageOverride;
use fp_consensus::{FindLogError, Hashes, Log, PostLog, PreLog};
use fp_rpc::EthereumRuntimeRPCApi;

use crate::{EthereumBlockNotification, EthereumBlockNotificationSinks, SyncStrategy};

pub fn sync_block<Block: BlockT, C: HeaderBackend<Block>>(
	storage_override: Arc<dyn StorageOverride<Block>>,
	backend: &fc_db::kv::Backend<Block, C>,
	header: &Block::Header,
) -> Result<(), String> {
	let substrate_block_hash = header.hash();
	let rpc_compatible_parent_hash = if header.number().is_zero() {
		None
	} else {
		backend
			.mapping()
			.rpc_compatible_hash_by_substrate_hash(header.parent_hash())?
	};
	let rpc_compatible_hash = |block: &ethereum::BlockV2| {
		if header.number().is_zero() || rpc_compatible_parent_hash.is_some() {
			Some(rpc_compatible_block_hash(block, rpc_compatible_parent_hash))
		} else {
			None
		}
	};
	match fp_consensus::find_log(header.digest()) {
		Ok(log) => {
			let gen_from_hashes = |hashes: Hashes,
			                       block: Option<&ethereum::BlockV2>|
			 -> fc_db::kv::MappingCommitment<Block> {
				fc_db::kv::MappingCommitment {
					block_hash: substrate_block_hash,
					ethereum_block_hash: hashes.block_hash,
					rpc_compatible_block_hash: block.and_then(rpc_compatible_hash),
					ethereum_transaction_hashes: hashes.transaction_hashes,
				}
			};
			let gen_from_block = |block: ethereum::BlockV2| -> fc_db::kv::MappingCommitment<Block> {
				let hashes = Hashes::from_block(block.clone());
				gen_from_hashes(hashes, Some(&block))
			};

			match log {
				Log::Pre(PreLog::Block(block)) => {
					let mapping_commitment = gen_from_block(block);
					backend.mapping().write_hashes(mapping_commitment)
				}
				Log::Post(post_log) => match post_log {
					PostLog::Hashes(hashes) => {
						let block = storage_override.current_block(substrate_block_hash);
						let mapping_commitment = gen_from_hashes(hashes, block.as_ref());
						backend.mapping().write_hashes(mapping_commitment)
					}
					PostLog::Block(block) => {
						let mapping_commitment = gen_from_block(block);
						backend.mapping().write_hashes(mapping_commitment)
					}
					PostLog::BlockHash(expect_eth_block_hash) => {
						let ethereum_block = storage_override.current_block(substrate_block_hash);
						match ethereum_block {
							Some(block) => {
								let got_eth_block_hash = block.header.hash();
								if got_eth_block_hash != expect_eth_block_hash {
									Err(format!(
										"Ethereum block hash mismatch: \
										frontier consensus digest ({expect_eth_block_hash:?}), \
										db state ({got_eth_block_hash:?})"
									))
								} else {
									let mapping_commitment = gen_from_block(block);
									backend.mapping().write_hashes(mapping_commitment)
								}
							}
							None => backend.mapping().write_none(substrate_block_hash),
						}
					}
				},
			}
		}
		Err(FindLogError::NotFound) => backend.mapping().write_none(substrate_block_hash),
		Err(FindLogError::MultipleLogs) => Err("Multiple logs found".to_string()),
	}
}

fn rpc_compatible_block_hash(block: &ethereum::BlockV2, parent_hash: Option<H256>) -> H256 {
	let mut header = block.header.clone();
	if let Some(parent_hash) = parent_hash {
		header.parent_hash = parent_hash;
	}
	header.timestamp /= 1000;
	H256::from(keccak_256(&rlp::encode(&header)))
}

pub fn sync_genesis_block<Block: BlockT, C>(
	client: &C,
	backend: &fc_db::kv::Backend<Block, C>,
	header: &Block::Header,
) -> Result<(), String>
where
	C: HeaderBackend<Block> + ProvideRuntimeApi<Block>,
	C::Api: EthereumRuntimeRPCApi<Block>,
{
	let substrate_block_hash = header.hash();

	if let Some(api_version) = client
		.runtime_api()
		.api_version::<dyn EthereumRuntimeRPCApi<Block>>(substrate_block_hash)
		.map_err(|e| format!("{:?}", e))?
	{
		let block = if api_version > 1 {
			client
				.runtime_api()
				.current_block(substrate_block_hash)
				.map_err(|e| format!("{:?}", e))?
		} else {
			#[allow(deprecated)]
			let legacy_block = client
				.runtime_api()
				.current_block_before_version_2(substrate_block_hash)
				.map_err(|e| format!("{:?}", e))?;
			legacy_block.map(|block| block.into())
		};
		let block = block.ok_or_else(|| "Ethereum genesis block not found".to_string())?;
		let block_hash = block.header.hash();
		let mapping_commitment = fc_db::kv::MappingCommitment::<Block> {
			block_hash: substrate_block_hash,
			ethereum_block_hash: block_hash,
			rpc_compatible_block_hash: Some(rpc_compatible_block_hash(&block, None)),
			ethereum_transaction_hashes: Vec::new(),
		};
		backend.mapping().write_hashes(mapping_commitment)?;
	} else {
		backend.mapping().write_none(substrate_block_hash)?;
	};

	Ok(())
}

pub fn sync_one_block<Block: BlockT, C, BE>(
	client: &C,
	substrate_backend: &BE,
	storage_override: Arc<dyn StorageOverride<Block>>,
	frontier_backend: &fc_db::kv::Backend<Block, C>,
	sync_from: <Block::Header as HeaderT>::Number,
	strategy: SyncStrategy,
	sync_oracle: Arc<dyn SyncOracle + Send + Sync + 'static>,
	pubsub_notification_sinks: Arc<
		EthereumBlockNotificationSinks<EthereumBlockNotification<Block>>,
	>,
) -> Result<bool, String>
where
	C: ProvideRuntimeApi<Block>,
	C::Api: EthereumRuntimeRPCApi<Block>,
	C: HeaderBackend<Block> + StorageProvider<Block, BE>,
	BE: Backend<Block>,
{
	let mut current_syncing_tips = frontier_backend.meta().current_syncing_tips()?;

	if current_syncing_tips.is_empty() {
		let mut leaves = substrate_backend
			.blockchain()
			.leaves()
			.map_err(|e| format!("{:?}", e))?;
		if leaves.is_empty() {
			return Ok(false);
		}
		current_syncing_tips.append(&mut leaves);
	}

	let mut operating_header = None;
	while let Some(checking_tip) = current_syncing_tips.pop() {
		if let Some(checking_header) = fetch_header(
			substrate_backend.blockchain(),
			frontier_backend,
			checking_tip,
			sync_from,
		)? {
			operating_header = Some(checking_header);
			break;
		}
	}
	let operating_header = match operating_header {
		Some(operating_header) => operating_header,
		None => {
			frontier_backend
				.meta()
				.write_current_syncing_tips(current_syncing_tips)?;
			return Ok(false);
		}
	};

	if operating_header.number() == &Zero::zero() {
		sync_genesis_block(client, frontier_backend, &operating_header)?;

		frontier_backend
			.meta()
			.write_current_syncing_tips(current_syncing_tips)?;
	} else {
		if SyncStrategy::Parachain == strategy
			&& operating_header.number() > &client.info().best_number
		{
			return Ok(false);
		}
		let parent_has_rpc_compatible_hash = frontier_backend
			.mapping()
			.rpc_compatible_hash_by_substrate_hash(operating_header.parent_hash())?
			.is_some();
		if !frontier_backend
			.mapping()
			.is_synced(operating_header.parent_hash())?
			|| !parent_has_rpc_compatible_hash
		{
			current_syncing_tips.push(operating_header.hash());
			current_syncing_tips.push(*operating_header.parent_hash());
			frontier_backend
				.meta()
				.write_current_syncing_tips(current_syncing_tips)?;
			return Ok(true);
		}
		sync_block(storage_override, frontier_backend, &operating_header)?;

		current_syncing_tips.push(*operating_header.parent_hash());
		frontier_backend
			.meta()
			.write_current_syncing_tips(current_syncing_tips)?;
	}
	// Notify on import and remove closed channels.
	// Only notify when the node is node in major syncing.
	let sinks = &mut pubsub_notification_sinks.lock();
	sinks.retain(|sink| {
		if !sync_oracle.is_major_syncing() {
			let hash = operating_header.hash();
			let is_new_best = client.info().best_hash == hash;
			sink.unbounded_send(EthereumBlockNotification { is_new_best, hash })
				.is_ok()
		} else {
			// Remove from the pool if in major syncing.
			false
		}
	});
	Ok(true)
}

pub fn sync_blocks<Block: BlockT, C, BE>(
	client: &C,
	substrate_backend: &BE,
	storage_override: Arc<dyn StorageOverride<Block>>,
	frontier_backend: &fc_db::kv::Backend<Block, C>,
	limit: usize,
	sync_from: <Block::Header as HeaderT>::Number,
	strategy: SyncStrategy,
	sync_oracle: Arc<dyn SyncOracle + Send + Sync + 'static>,
	pubsub_notification_sinks: Arc<
		EthereumBlockNotificationSinks<EthereumBlockNotification<Block>>,
	>,
) -> Result<bool, String>
where
	C: ProvideRuntimeApi<Block>,
	C::Api: EthereumRuntimeRPCApi<Block>,
	C: HeaderBackend<Block> + StorageProvider<Block, BE>,
	BE: Backend<Block>,
{
	let mut synced_any = false;

	for _ in 0..limit {
		synced_any = synced_any
			|| sync_one_block(
				client,
				substrate_backend,
				storage_override.clone(),
				frontier_backend,
				sync_from,
				strategy,
				sync_oracle.clone(),
				pubsub_notification_sinks.clone(),
			)?;
	}

	Ok(synced_any)
}

pub fn fetch_header<Block: BlockT, C, BE>(
	substrate_backend: &BE,
	frontier_backend: &fc_db::kv::Backend<Block, C>,
	checking_tip: Block::Hash,
	sync_from: <Block::Header as HeaderT>::Number,
) -> Result<Option<Block::Header>, String>
where
	C: HeaderBackend<Block>,
	BE: HeaderBackend<Block>,
{
	let is_synced = frontier_backend.mapping().is_synced(&checking_tip)?;
	let has_rpc_compatible_hash = frontier_backend
		.mapping()
		.rpc_compatible_hash_by_substrate_hash(&checking_tip)?
		.is_some();
	if is_synced && has_rpc_compatible_hash {
		return Ok(None);
	}

	match substrate_backend.header(checking_tip) {
		Ok(Some(checking_header)) if is_synced || checking_header.number() >= &sync_from => {
			Ok(Some(checking_header))
		}
		Ok(Some(_)) => Ok(None),
		Ok(None) | Err(_) => Err("Header not found".to_string()),
	}
}
