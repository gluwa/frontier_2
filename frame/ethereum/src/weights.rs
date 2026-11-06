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

//! Weights the pallet reserves for its own hooks.

use core::marker::PhantomData;
use frame_support::{
	traits::Get,
	weights::{constants::RocksDbWeight, Weight},
};

pub trait WeightInfo {
	/// `on_finalize` for a block without Ethereum transactions, which
	/// `on_initialize` reserves for every block.
	///
	/// Each transaction adds to `on_finalize` too (its receipt, logs and bloom,
	/// and the state it changed for the intermediate state root), but
	/// transactions are charged by their gas alone. The runtime's
	/// `GasWeightMapping` has to price that work in, and the runtime should
	/// check that it does against benchmarks of whole blocks.
	fn on_finalize_base() -> Weight;
}

/// Upstream's estimate, priced with the runtime's `DbWeight`: reading the
/// digest and block number, and writing the block and the pruned block hash.
pub struct SubstrateWeight<T>(PhantomData<T>);

impl<T: frame_system::Config> WeightInfo for SubstrateWeight<T> {
	fn on_finalize_base() -> Weight {
		T::DbWeight::get().reads_writes(2, 2)
	}
}

/// The same estimate, priced with `RocksDbWeight`, for tests.
impl WeightInfo for () {
	fn on_finalize_base() -> Weight {
		RocksDbWeight::get().reads_writes(2, 2)
	}
}
