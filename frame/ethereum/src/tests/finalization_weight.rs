use super::*;
use crate::FinalizationWeight;
use frame_support::{
	dispatch::DispatchClass,
	traits::{Get, Hooks},
	weights::Weight,
};
use pallet_evm::GasWeightMapping;

pub(super) struct ResetWeights;
impl Drop for ResetWeights {
	fn drop(&mut self) {
		FinalizationBase::set(Weight::zero());
		FinalizationPerGas::set(Weight::zero());
	}
}

fn transfer(sender: &AccountInfo, recipient: H160, gas_limit: u64) -> Transaction {
	LegacyUnsignedTransaction {
		nonce: U256::zero(),
		gas_price: U256::one(),
		gas_limit: gas_limit.into(),
		action: TransactionAction::Call(recipient),
		value: U256::one(),
		input: Vec::new(),
	}
	.sign(&sender.private_key)
}

#[test]
fn block_initialization_reserves_the_fixed_finalization_cost() {
	let (_, mut ext) = new_test_ext(0);
	ext.execute_with(|| {
		let _reset = ResetWeights;
		let before = Ethereum::on_initialize(1);
		let extra = Weight::from_parts(123_456, 456);
		FinalizationBase::set(extra);
		assert_eq!(Ethereum::on_initialize(1), before.saturating_add(extra));
	});
}

#[test]
fn finalization_reservation_can_reject_an_otherwise_admissible_transaction() {
	let (accounts, mut ext) = new_test_ext(2);
	ext.execute_with(|| {
		let _reset = ResetWeights;
		let call = RuntimeCall::Ethereum(crate::Call::<Test>::transact {
			transaction: transfer(&accounts[0], accounts[1].address, 21_000),
		});
		let before = call.get_dispatch_info();
		assert_ok!(frame_system::CheckWeight::<Test>::do_validate(&before, 0));
		let limits: frame_system::limits::BlockWeights =
			<Test as frame_system::Config>::BlockWeights::get();
		let maximum = limits.get(DispatchClass::Normal).max_extrinsic.unwrap();
		FinalizationPerGas::set(Weight::from_parts(maximum.ref_time() / 21_000 + 1, 0));
		let RuntimeCall::Ethereum(ethereum) = &call else {
			unreachable!()
		};
		assert_eq!(
			ethereum.pre_dispatch_self_contained(
				&accounts[0].address,
				&call.get_dispatch_info(),
				0
			),
			Some(Err(InvalidTransaction::ExhaustsResources.into()))
		);
		assert_eq!(crate::Pending::<Test>::count(), 0);
	});
}

#[test]
fn reclaim_preserves_finalization_weight_for_consumed_gas_and_proof_size() {
	let (accounts, mut ext) = new_test_ext(2);
	ext.execute_with(|| {
		let _reset = ResetWeights;
		FinalizationPerGas::set(Weight::from_parts(1_000, 2));
		let call = RuntimeCall::Ethereum(crate::Call::<Test>::transact {
			transaction: transfer(&accounts[0], accounts[1].address, 100_000),
		});
		let extrinsic = CheckedExtrinsic::<_, _, frame_system::CheckWeight<Test>, _> {
			signed: fp_self_contained::CheckedSignature::SelfContained(accounts[0].address),
			function: call,
		};
		let declared = extrinsic.get_dispatch_info();
		let actual = extrinsic
			.apply::<Test>(&declared, 0)
			.unwrap()
			.unwrap()
			.actual_weight
			.unwrap();
		let execution = <Test as pallet_evm::Config>::GasWeightMapping::gas_to_weight(21_000, true);
		let reserved = TestFinalizationWeight::transaction(21_000);
		assert_eq!(
			actual.ref_time(),
			execution.saturating_add(reserved).ref_time()
		);
		// The runner reclaims unused execution proof, but not the deferred reservation.
		assert!(actual.proof_size() >= reserved.proof_size());
		assert!(actual.proof_size() <= execution.saturating_add(reserved).proof_size());
		assert!(actual.all_lt(declared.call_weight));
		let limits: frame_system::limits::BlockWeights =
			<Test as frame_system::Config>::BlockWeights::get();
		let base = limits.get(DispatchClass::Normal).base_extrinsic;
		assert_eq!(
			*System::block_weight().get(DispatchClass::Normal),
			base.saturating_add(actual)
		);
		assert_eq!(crate::Pending::<Test>::count(), 1);
	});
}
