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

use super::*;
use frame_support::traits::Get;

const CAP: usize = 30;

/// Build an ABI `Error(string)` payload, declaring `declared_len` as the string length
/// while only carrying `message` bytes of actual content.
fn error_payload(declared_len: usize, message: &[u8]) -> Vec<u8> {
	let mut data = vec![0x08, 0xc3, 0x79, 0xa0];
	let mut offset = [0u8; 32];
	offset[31] = 0x20;
	data.extend_from_slice(&offset);
	data.extend_from_slice(&U256::from(declared_len).to_big_endian());
	data.extend_from_slice(message);
	data
}

#[test]
fn cap_matches_mock_config() {
	assert_eq!(
		<<Test as crate::Config>::ExtraDataLength as Get<u32>>::get() as usize,
		CAP
	);
}

#[test]
fn well_formed_message_is_capped() {
	let message = vec![b'a'; 100];
	let out = Ethereum::revert_extra_data(error_payload(100, &message));
	assert_eq!(out, vec![b'a'; CAP]);
}

#[test]
fn short_message_is_returned_in_full() {
	let out = Ethereum::revert_extra_data(error_payload(5, b"hello"));
	assert_eq!(out, b"hello".to_vec());
}

#[test]
fn truncated_payload_with_oversized_declaration_is_capped() {
	// Declared length >= cap, but returndata is cut just below `68 + cap` bytes.
	let message = vec![b'a'; CAP - 1];
	let data = error_payload(CAP, &message);
	assert_eq!(data.len(), 68 + CAP - 1);
	let out = Ethereum::revert_extra_data(data);
	assert!(out.len() <= CAP);
}

#[test]
fn huge_declared_length_is_capped() {
	let data = error_payload(usize::MAX, &vec![b'a'; CAP - 1]);
	assert!(Ethereum::revert_extra_data(data).len() <= CAP);
}

#[test]
fn empty_error_string_decodes_to_empty() {
	let data = error_payload(0, b"");
	assert_eq!(data.len(), 68);
	assert!(Ethereum::revert_extra_data(data).is_empty());
}

#[test]
fn non_error_selector_returns_raw_prefix() {
	// Error-like layout (small length at 36..68) but a custom-error selector.
	let mut data = error_payload(5, b"hello");
	data[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
	assert_eq!(
		Ethereum::revert_extra_data(data.clone()),
		data[..CAP].to_vec()
	);
}

#[test]
fn non_canonical_offset_returns_raw_prefix() {
	let mut data = error_payload(5, b"hello");
	data[35] = 0x40;
	assert_eq!(
		Ethereum::revert_extra_data(data.clone()),
		data[..CAP].to_vec()
	);
}

#[test]
fn truncated_payload_returns_raw_prefix() {
	let data = error_payload(CAP, &vec![b'a'; CAP - 1]);
	assert_eq!(
		Ethereum::revert_extra_data(data.clone()),
		data[..CAP].to_vec()
	);
}

#[test]
fn short_raw_revert_data_is_capped() {
	// No `Error(string)` framing at all, e.g. a custom error with large arguments.
	for len in [0, 1, CAP, CAP + 1, 67, 68, 69, 1000] {
		let out = Ethereum::revert_extra_data(vec![0xff; len]);
		assert!(out.len() <= CAP, "len {len} produced {} bytes", out.len());
	}
}

#[test]
fn repeated_reverts_never_exceed_cap() {
	for declared in 0..200usize {
		for actual in 0..200usize {
			let data = error_payload(declared, &vec![b'x'; actual]);
			assert!(Ethereum::revert_extra_data(data).len() <= CAP);
		}
	}
}
