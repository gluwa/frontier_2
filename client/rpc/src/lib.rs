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

#![allow(
	clippy::too_many_arguments,
	clippy::large_enum_variant,
	clippy::manual_range_contains,
	clippy::explicit_counter_loop,
	clippy::len_zero,
	clippy::new_without_default
)]
#![warn(unused_crate_dependencies)]

mod cache;
mod debug;
mod eth;
mod eth_pubsub;
mod net;
mod signer;
#[cfg(feature = "txpool")]
mod txpool;
mod web3;

#[cfg(feature = "txpool")]
pub use self::txpool::TxPool;
pub use self::{
	cache::{EthBlockDataCacheTask, EthTask},
	debug::Debug,
	eth::{format, pending, EstimateGasAdapter, Eth, EthConfig, EthFilter},
	eth_pubsub::{EthPubSub, EthereumSubIdProvider},
	net::Net,
	signer::{EthDevSigner, EthSigner},
	web3::Web3,
};
pub use ethereum::TransactionV3 as EthereumTransaction;
#[cfg(feature = "txpool")]
pub use fc_rpc_core::TxPoolApiServer;
pub use fc_rpc_core::{
	DebugApiServer, EthApiServer, EthFilterApiServer, EthPubSubApiServer, NetApiServer,
	Web3ApiServer,
};
pub use fc_storage::{overrides::*, StorageOverrideHandler};

pub mod frontier_backend_client {
	use super::{err, internal_err, RESOURCE_NOT_FOUND_CODE};

	use ethereum_types::{H160, H256, U256};
	use jsonrpsee::core::RpcResult;
	use scale_codec::Encode;
	// Substrate
	use sc_client_api::{
		backend::{Backend, StorageProvider},
		StorageKey,
	};
	use sp_blockchain::HeaderBackend;
	use sp_io::hashing::{blake2_128, twox_128};
	use sp_runtime::{
		generic::BlockId,
		traits::{Block as BlockT, HashingFor, UniqueSaturatedInto},
	};
	use sp_state_machine::OverlayedChanges;
	// Frontier
	use fc_rpc_core::types::BlockNumberOrHash;

	/// SCALE encoding of `frame_system::AccountInfo<u32, pallet_balances::AccountData<u128>>`:
	/// `nonce`, `consumers`, `providers`, `sufficients` (`u32` each) followed by `free`,
	/// `reserved`, `frozen` and `flags` (`u128` each).
	const ACCOUNT_INFO_LEN: usize = 80;
	const ACCOUNT_NONCE: std::ops::Range<usize> = 0..4;
	const ACCOUNT_PROVIDERS: std::ops::Range<usize> = 8..12;
	const ACCOUNT_FREE: std::ops::Range<usize> = 16..32;

	/// Encoded `AccountInfo::default()`. Everything is zero except `AccountData::flags`, which
	/// defaults to `ExtraFlags::IS_NEW_LOGIC` (the most significant bit) rather than zero.
	fn default_account_info() -> Vec<u8> {
		let mut item = vec![0u8; ACCOUNT_INFO_LEN];
		item[ACCOUNT_INFO_LEN - 1] = 0x80;
		item
	}

	/// Applies `balance` and `nonce` on top of the `System::Account` record of `account_id`.
	///
	/// An account that is absent from state gets a default record to apply them on, so that
	/// overriding a previously unused address takes effect. Nothing is written for an absent
	/// account when neither field is overridden.
	fn override_system_account<B, C, BE>(
		client: &C,
		overlayed_changes: &mut OverlayedChanges<HashingFor<B>>,
		block: B::Hash,
		account_id: &[u8],
		balance: Option<U256>,
		nonce: Option<U256>,
	) where
		B: BlockT,
		C: StorageProvider<B, BE> + Send + Sync,
		BE: Backend<B>,
	{
		let mut key = [twox_128(b"System"), twox_128(b"Account")]
			.concat()
			.to_vec();
		key.extend(blake2_128(account_id));
		key.extend(account_id);

		let mut new_item = match client.storage(block, &StorageKey(key.clone())) {
			Ok(Some(item)) => item.0,
			Ok(None) if balance.is_some() || nonce.is_some() => {
				let mut item = default_account_info();
				// A funded account is kept alive by a provider reference, as it would be on chain.
				// Judge it by the balance that is actually stored, which `low_u128` truncates.
				if balance.is_some_and(|balance| balance.low_u128() != 0) {
					item.splice(ACCOUNT_PROVIDERS, 1u32.encode());
				}
				item
			}
			Ok(None) => return,
			Err(e) => {
				log::warn!(target: "rpc", "Failed to read System::Account for a state override: {e:?}");
				return;
			}
		};

		if let Some(nonce) = nonce {
			new_item.splice(ACCOUNT_NONCE, nonce.low_u32().encode());
		}

		if let Some(balance) = balance {
			new_item.splice(ACCOUNT_FREE, balance.low_u128().encode());
		}

		overlayed_changes.set_storage(key, Some(new_item));
	}

	/// Implements a default runtime storage override.
	/// It assumes that the balances and nonces are stored in pallet `system.account`, and
	/// have `nonce: Index` = `u32` for  and `free: Balance` = `u128`.
	/// Uses IdentityAddressMapping for the address.
	pub struct SystemAccountId20StorageOverride<B, C, BE>(pub std::marker::PhantomData<(B, C, BE)>);
	impl<B, C, BE> fp_rpc::RuntimeStorageOverride<B, C> for SystemAccountId20StorageOverride<B, C, BE>
	where
		B: BlockT,
		C: StorageProvider<B, BE> + Send + Sync,
		BE: Backend<B>,
	{
		fn is_enabled() -> bool {
			true
		}

		fn set_overlayed_changes(
			client: &C,
			overlayed_changes: &mut OverlayedChanges<HashingFor<B>>,
			block: B::Hash,
			_version: u32,
			address: H160,
			balance: Option<U256>,
			nonce: Option<U256>,
		) {
			let account_id = Self::into_account_id_bytes(address);
			override_system_account::<B, C, BE>(
				client,
				overlayed_changes,
				block,
				&account_id,
				balance,
				nonce,
			);
		}

		fn into_account_id_bytes(address: H160) -> Vec<u8> {
			use pallet_evm::AddressMapping;
			let address: H160 = pallet_evm::IdentityAddressMapping::into_account_id(address);
			address.as_ref().to_owned()
		}
	}

	/// Implements a runtime storage override.
	/// It assumes that the balances and nonces are stored in pallet `system.account`, and
	/// have `nonce: Index` = `u32` for  and `free: Balance` = `u128`.
	/// USes HashedAddressMapping for the address.
	pub struct SystemAccountId32StorageOverride<B, C, BE>(pub std::marker::PhantomData<(B, C, BE)>);
	impl<B, C, BE> fp_rpc::RuntimeStorageOverride<B, C> for SystemAccountId32StorageOverride<B, C, BE>
	where
		B: BlockT,
		C: StorageProvider<B, BE> + Send + Sync,
		BE: Backend<B>,
	{
		fn is_enabled() -> bool {
			true
		}

		fn set_overlayed_changes(
			client: &C,
			overlayed_changes: &mut OverlayedChanges<HashingFor<B>>,
			block: B::Hash,
			_version: u32,
			address: H160,
			balance: Option<U256>,
			nonce: Option<U256>,
		) {
			let account_id = Self::into_account_id_bytes(address);
			override_system_account::<B, C, BE>(
				client,
				overlayed_changes,
				block,
				&account_id,
				balance,
				nonce,
			);
		}

		fn into_account_id_bytes(address: H160) -> Vec<u8> {
			use pallet_evm::AddressMapping;
			use sp_core::crypto::ByteArray;
			use sp_runtime::traits::BlakeTwo256;

			pallet_evm::HashedAddressMapping::<BlakeTwo256>::into_account_id(address)
				.as_slice()
				.to_owned()
		}
	}

	pub async fn native_block_id<B, C>(
		client: &C,
		backend: &dyn fc_api::Backend<B>,
		number: Option<BlockNumberOrHash>,
	) -> RpcResult<Option<BlockId<B>>>
	where
		B: BlockT,
		C: HeaderBackend<B> + 'static,
	{
		Ok(match number.unwrap_or(BlockNumberOrHash::Latest) {
			BlockNumberOrHash::Hash { hash, .. } => {
				match load_hash::<B, C>(client, backend, hash).await? {
					Some(hash) => Some(BlockId::Hash(hash)),
					// EIP-1898: an explicit block hash that cannot be resolved must
					// raise a JSON-RPC error (recommended code -32001 "Resource not
					// found") rather than falling through to a zero/default/empty
					// result or being treated as pending.
					None => {
						return Err(err(
							RESOURCE_NOT_FOUND_CODE,
							format!("block hash not found: {hash:?}"),
							None,
						))
					}
				}
			}
			BlockNumberOrHash::Num(number) => Some(BlockId::Number(number.unique_saturated_into())),
			BlockNumberOrHash::Latest => match backend.latest_block_hash().await {
				Ok(hash) => Some(BlockId::Hash(hash)),
				Err(e) => {
					log::warn!(target: "rpc", "Failed to get latest block hash from the sql db: {e:?}");
					Some(BlockId::Hash(client.info().best_hash))
				}
			},
			BlockNumberOrHash::Earliest => Some(BlockId::Hash(client.info().genesis_hash)),
			BlockNumberOrHash::Pending => None,
			BlockNumberOrHash::Safe => Some(BlockId::Hash(client.info().finalized_hash)),
			BlockNumberOrHash::Finalized => Some(BlockId::Hash(client.info().finalized_hash)),
		})
	}

	pub async fn load_hash<B, C>(
		client: &C,
		backend: &dyn fc_api::Backend<B>,
		hash: H256,
	) -> RpcResult<Option<B::Hash>>
	where
		B: BlockT,
		C: HeaderBackend<B> + 'static,
	{
		let substrate_hashes = backend
			.block_hash(&hash)
			.await
			.map_err(|err| internal_err(format!("fetch aux store failed: {err:?}")))?;

		if let Some(substrate_hashes) = substrate_hashes {
			for substrate_hash in substrate_hashes {
				if is_canon::<B, C>(client, substrate_hash) {
					return Ok(Some(substrate_hash));
				}
			}
		}
		Ok(None)
	}

	pub fn is_canon<B, C>(client: &C, target_hash: B::Hash) -> bool
	where
		B: BlockT,
		C: HeaderBackend<B> + 'static,
	{
		if let Ok(Some(number)) = client.number(target_hash) {
			if let Ok(Some(hash)) = client.hash(number) {
				return hash == target_hash;
			}
		}
		false
	}

	pub async fn load_transactions<B, C>(
		client: &C,
		backend: &dyn fc_api::Backend<B>,
		transaction_hash: H256,
		only_canonical: bool,
	) -> RpcResult<Option<(H256, u32)>>
	where
		B: BlockT,
		C: HeaderBackend<B> + 'static,
	{
		let transaction_metadata = backend
			.transaction_metadata(&transaction_hash)
			.await
			.map_err(|err| internal_err(format!("fetch aux store failed: {err:?}")))?;

		transaction_metadata
			.iter()
			.find(|meta| is_canon::<B, C>(client, meta.substrate_block_hash))
			.map_or_else(
				|| {
					if !only_canonical && transaction_metadata.len() > 0 {
						Ok(Some((
							transaction_metadata[0].ethereum_block_hash,
							transaction_metadata[0].ethereum_index,
						)))
					} else {
						Ok(None)
					}
				},
				|meta| Ok(Some((meta.ethereum_block_hash, meta.ethereum_index))),
			)
	}
}

pub fn err<T: ToString>(
	code: i32,
	message: T,
	data: Option<&[u8]>,
) -> jsonrpsee::types::error::ErrorObjectOwned {
	jsonrpsee::types::error::ErrorObject::owned(
		code,
		message.to_string(),
		data.map(|bytes| {
			jsonrpsee::core::to_json_raw_value(&format!("0x{}", hex::encode(bytes)))
				.expect("fail to serialize data")
		}),
	)
}

/// EIP-1898 "Resource not found" error code. Returned when an explicit block
/// hash supplied as a block parameter cannot be found.
/// See <https://eips.ethereum.org/EIPS/eip-1898>.
pub const RESOURCE_NOT_FOUND_CODE: i32 = -32001;

pub fn internal_err<T: ToString>(message: T) -> jsonrpsee::types::error::ErrorObjectOwned {
	err(jsonrpsee::types::error::INTERNAL_ERROR_CODE, message, None)
}

pub fn internal_err_with_data<T: ToString>(
	message: T,
	data: &[u8],
) -> jsonrpsee::types::error::ErrorObjectOwned {
	err(
		jsonrpsee::types::error::INTERNAL_ERROR_CODE,
		message,
		Some(data),
	)
}

pub fn public_key(transaction: &EthereumTransaction) -> Result<[u8; 64], sp_io::EcdsaVerifyError> {
	let mut sig = [0u8; 65];
	let mut msg = [0u8; 32];
	match transaction {
		EthereumTransaction::Legacy(t) => {
			sig[0..32].copy_from_slice(&t.signature.r()[..]);
			sig[32..64].copy_from_slice(&t.signature.s()[..]);
			sig[64] = t.signature.standard_v();
			msg.copy_from_slice(&ethereum::LegacyTransactionMessage::from(t.clone()).hash()[..]);
		}
		EthereumTransaction::EIP2930(t) => {
			sig[0..32].copy_from_slice(&t.signature.r()[..]);
			sig[32..64].copy_from_slice(&t.signature.s()[..]);
			sig[64] = t.signature.odd_y_parity() as u8;
			msg.copy_from_slice(&ethereum::EIP2930TransactionMessage::from(t.clone()).hash()[..]);
		}
		EthereumTransaction::EIP1559(t) => {
			sig[0..32].copy_from_slice(&t.signature.r()[..]);
			sig[32..64].copy_from_slice(&t.signature.s()[..]);
			sig[64] = t.signature.odd_y_parity() as u8;
			msg.copy_from_slice(&ethereum::EIP1559TransactionMessage::from(t.clone()).hash()[..]);
		}
		EthereumTransaction::EIP7702(t) => {
			sig[0..32].copy_from_slice(&t.signature.r()[..]);
			sig[32..64].copy_from_slice(&t.signature.s()[..]);
			sig[64] = t.signature.odd_y_parity() as u8;
			msg.copy_from_slice(&ethereum::EIP7702TransactionMessage::from(t.clone()).hash()[..]);
		}
	}
	sp_io::crypto::secp256k1_ecdsa_recover(&sig, &msg)
}

#[cfg(test)]
mod tests {
	use std::{path::PathBuf, sync::Arc};

	use futures::executor;
	use sc_block_builder::BlockBuilderBuilder;
	use sp_blockchain::HeaderBackend;
	use sp_consensus::BlockOrigin;
	use sp_runtime::{
		generic::{Block, Header},
		traits::{BlakeTwo256, Block as BlockT},
	};
	use substrate_test_runtime_client::{
		prelude::*, DefaultTestClientBuilderExt, TestClientBuilder,
	};
	use tempfile::tempdir;

	type OpaqueBlock =
		Block<Header<u64, BlakeTwo256>, substrate_test_runtime_client::runtime::Extrinsic>;

	fn open_frontier_backend<Block: BlockT, C: HeaderBackend<Block>>(
		client: Arc<C>,
		path: PathBuf,
	) -> Result<Arc<fc_db::kv::Backend<Block, C>>, String> {
		Ok(Arc::new(fc_db::kv::Backend::<Block, C>::new(
			client,
			&fc_db::kv::DatabaseSettings {
				#[cfg(feature = "rocksdb")]
				source: sc_client_db::DatabaseSource::RocksDb {
					path,
					cache_size: 0,
				},
				#[cfg(not(feature = "rocksdb"))]
				source: sc_client_db::DatabaseSource::ParityDb { path },
			},
		)?))
	}

	#[test]
	fn substrate_block_hash_one_to_many_works() {
		let tmp = tempdir().expect("create a temporary directory");
		let (client, _) = TestClientBuilder::new()
			.build_with_native_executor::<substrate_test_runtime_client::runtime::RuntimeApi, _>(
			None,
		);

		let client = Arc::new(client);

		// Create a temporary frontier secondary DB.
		let backend = open_frontier_backend::<OpaqueBlock, _>(client.clone(), tmp.keep())
			.expect("a temporary db was created");

		// A random ethereum block hash to use
		let ethereum_block_hash = sp_core::H256::random();

		// G -> A1.
		let chain = client.chain_info();
		let mut builder = BlockBuilderBuilder::new(&*client)
			.on_parent_block(chain.best_hash)
			.with_parent_block_number(chain.best_number)
			.build()
			.unwrap();
		builder.push_storage_change(vec![1], None).unwrap();
		let a1 = builder.build().unwrap().block;
		let a1_hash = a1.header.hash();
		executor::block_on(client.import(BlockOrigin::Own, a1)).unwrap();

		// A1 -> B1
		let mut builder = BlockBuilderBuilder::new(&*client)
			.on_parent_block(a1_hash)
			.fetch_parent_block_number(&*client)
			.unwrap()
			.build()
			.unwrap();
		builder.push_storage_change(vec![1], None).unwrap();
		let b1 = builder.build().unwrap().block;
		let b1_hash = b1.header.hash();
		executor::block_on(client.import(BlockOrigin::Own, b1)).unwrap();

		// Map B1
		let commitment = fc_db::kv::MappingCommitment::<OpaqueBlock> {
			block_hash: b1_hash,
			ethereum_block_hash,
			ethereum_transaction_hashes: vec![],
		};
		let _ = backend.mapping().write_hashes(commitment);

		// Expect B1 to be canon
		assert_eq!(
			futures::executor::block_on(super::frontier_backend_client::load_hash(
				client.as_ref(),
				backend.as_ref(),
				ethereum_block_hash
			))
			.unwrap()
			.unwrap(),
			b1_hash,
		);

		// A1 -> B2
		let mut builder = BlockBuilderBuilder::new(&*client)
			.on_parent_block(a1_hash)
			.fetch_parent_block_number(&*client)
			.unwrap()
			.build()
			.unwrap();
		builder.push_storage_change(vec![2], None).unwrap();
		let b2 = builder.build().unwrap().block;
		let b2_hash = b2.header.hash();
		executor::block_on(client.import(BlockOrigin::Own, b2)).unwrap();

		// Map B2 to same ethereum hash
		let commitment = fc_db::kv::MappingCommitment::<OpaqueBlock> {
			block_hash: b2_hash,
			ethereum_block_hash,
			ethereum_transaction_hashes: vec![],
		};
		let _ = backend.mapping().write_hashes(commitment);

		// Still expect B1 to be canon
		assert_eq!(
			futures::executor::block_on(super::frontier_backend_client::load_hash(
				client.as_ref(),
				backend.as_ref(),
				ethereum_block_hash
			))
			.unwrap()
			.unwrap(),
			b1_hash,
		);

		// B2 -> C1. B2 branch is now canon.
		let mut builder = BlockBuilderBuilder::new(&*client)
			.on_parent_block(b2_hash)
			.fetch_parent_block_number(&*client)
			.unwrap()
			.build()
			.unwrap();
		builder.push_storage_change(vec![1], None).unwrap();
		let c1 = builder.build().unwrap().block;
		executor::block_on(client.import(BlockOrigin::Own, c1)).unwrap();

		// Expect B2 to be new canon
		assert_eq!(
			futures::executor::block_on(super::frontier_backend_client::load_hash(
				client.as_ref(),
				backend.as_ref(),
				ethereum_block_hash
			))
			.unwrap()
			.unwrap(),
			b2_hash,
		);
	}

	mod system_account_override {
		use super::*;
		use ethereum_types::{H160, U256};
		use fp_rpc::RuntimeStorageOverride;
		use sc_client_api::StorageProvider;
		use scale_codec::Encode;
		use sp_io::hashing::{blake2_128, twox_128};
		use sp_runtime::traits::HashingFor;
		use sp_state_machine::OverlayedChanges;
		use substrate_test_runtime_client::{
			runtime::{Block as TestBlock, Hash},
			Backend, Client,
		};

		use crate::frontier_backend_client::{
			SystemAccountId20StorageOverride, SystemAccountId32StorageOverride,
		};

		type TestClient = Client<Backend>;
		type Id20 = SystemAccountId20StorageOverride<TestBlock, TestClient, Backend>;
		type Id32 = SystemAccountId32StorageOverride<TestBlock, TestClient, Backend>;

		// `AccountInfo { nonce, consumers, providers, sufficients, data: AccountData { free,
		// reserved, frozen, flags } }`, where `flags` defaults to `IS_NEW_LOGIC`.
		type AccountInfo = (u32, u32, u32, u32, u128, u128, u128, u128);
		const IS_NEW_LOGIC: u128 = 1 << 127;

		const ADDRESS: H160 = H160([0x12; 20]);

		fn account_key(account_id: &[u8]) -> Vec<u8> {
			let mut key = [twox_128(b"System"), twox_128(b"Account")].concat();
			key.extend(blake2_128(account_id));
			key.extend(account_id);
			key
		}

		fn new_client() -> Arc<TestClient> {
			let (client, _) = TestClientBuilder::new()
				.build_with_native_executor::<substrate_test_runtime_client::runtime::RuntimeApi, _>(
				None,
			);
			Arc::new(client)
		}

		/// Imports a block that writes `value` under `key`, returning the new block hash.
		fn import_storage(client: &Arc<TestClient>, key: Vec<u8>, value: Vec<u8>) -> Hash {
			let chain = client.chain_info();
			let mut builder = BlockBuilderBuilder::new(&**client)
				.on_parent_block(chain.best_hash)
				.with_parent_block_number(chain.best_number)
				.build()
				.unwrap();
			builder.push_storage_change(key, Some(value)).unwrap();
			let block = builder.build().unwrap().block;
			let hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			hash
		}

		/// Runs the override for `ADDRESS` at `block` and returns the `System::Account` value it
		/// wrote to the overlay, if any.
		fn overridden_account<O: RuntimeStorageOverride<TestBlock, TestClient>>(
			client: &TestClient,
			block: Hash,
			balance: Option<U256>,
			nonce: Option<u32>,
		) -> Option<AccountInfo> {
			let mut overlay = OverlayedChanges::<HashingFor<TestBlock>>::default();
			O::set_overlayed_changes(
				client,
				&mut overlay,
				block,
				6,
				ADDRESS,
				balance,
				nonce.map(U256::from),
			);
			let key = account_key(&O::into_account_id_bytes(ADDRESS));
			overlay
				.storage(&key)
				.flatten()
				.map(|value| scale_codec::Decode::decode(&mut &value[..]).unwrap())
		}

		fn absent_account_override<O: RuntimeStorageOverride<TestBlock, TestClient>>() {
			let client = new_client();
			let best = client.chain_info().best_hash;
			let key = account_key(&O::into_account_id_bytes(ADDRESS));
			assert!(
				client
					.storage(best, &sp_storage::StorageKey(key))
					.unwrap()
					.is_none(),
				"the account must be absent for this test"
			);

			// Balance and nonce: a funded account is kept alive by a provider reference.
			assert_eq!(
				overridden_account::<O>(&client, best, Some(U256::from(1_000)), Some(7)),
				Some((7, 0, 1, 0, 1_000, 0, 0, IS_NEW_LOGIC)),
			);
			// Balance only.
			assert_eq!(
				overridden_account::<O>(&client, best, Some(U256::from(1_000)), None),
				Some((0, 0, 1, 0, 1_000, 0, 0, IS_NEW_LOGIC)),
			);
			// Nonce only.
			assert_eq!(
				overridden_account::<O>(&client, best, None, Some(7)),
				Some((7, 0, 0, 0, 0, 0, 0, IS_NEW_LOGIC)),
			);
			// A zero balance does not fund the account.
			assert_eq!(
				overridden_account::<O>(&client, best, Some(U256::zero()), None),
				Some((0, 0, 0, 0, 0, 0, 0, IS_NEW_LOGIC)),
			);
			// A balance that truncates to zero does not fund the account either.
			assert_eq!(
				overridden_account::<O>(&client, best, Some(U256::one() << 128), None),
				Some((0, 0, 0, 0, 0, 0, 0, IS_NEW_LOGIC)),
			);
			// Nothing to override: no account is conjured up (e.g. a code-only override).
			assert_eq!(overridden_account::<O>(&client, best, None, None), None);
		}

		fn existing_account_override<O: RuntimeStorageOverride<TestBlock, TestClient>>() {
			let client = new_client();
			let key = account_key(&O::into_account_id_bytes(ADDRESS));
			let existing: AccountInfo = (3, 4, 5, 6, 100, 200, 300, IS_NEW_LOGIC);
			let block = import_storage(&client, key, existing.encode());

			// Only the overridden fields change; the rest of the record is retained.
			assert_eq!(
				overridden_account::<O>(&client, block, Some(U256::from(1_000)), Some(7)),
				Some((7, 4, 5, 6, 1_000, 200, 300, IS_NEW_LOGIC)),
			);
			assert_eq!(
				overridden_account::<O>(&client, block, Some(U256::from(1_000)), None),
				Some((3, 4, 5, 6, 1_000, 200, 300, IS_NEW_LOGIC)),
			);
			assert_eq!(
				overridden_account::<O>(&client, block, None, Some(7)),
				Some((7, 4, 5, 6, 100, 200, 300, IS_NEW_LOGIC)),
			);
		}

		#[test]
		fn id20_overrides_absent_account() {
			absent_account_override::<Id20>();
		}

		#[test]
		fn id32_overrides_absent_account() {
			absent_account_override::<Id32>();
		}

		#[test]
		fn id20_overrides_existing_account() {
			existing_account_override::<Id20>();
		}

		#[test]
		fn id32_overrides_existing_account() {
			existing_account_override::<Id32>();
		}
	}
}
