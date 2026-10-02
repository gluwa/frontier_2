import { ethers } from "ethers";
import { expect } from "chai";
import { step } from "mocha-steps";

import { GENESIS_ACCOUNT_PRIVATE_KEY, CHAIN_ID } from "./config";
import { createAndFinalizeBlock, describeWithFrontier, customRequest } from "./util";

// The base fee is adjusted when a block is finalized, after its transactions have been executed.
// Blocks, transactions, receipts and fee history entries must all report the base fee the block was
// executed with (the one left by its parent), not the adjusted one.
describeWithFrontier("Frontier RPC (Base Fee History)", (context: any) => {
	// `DefaultBaseFeePerGas` of the runtime, which is the base fee of the first blocks.
	const INITIAL_BASE_FEE = BigInt(1_000_000_000);
	// The base fee does not decrease below `DefaultBaseFeePerGas * ideal block fullness`.
	const BASE_FEE_FLOOR = BigInt(500_000_000);
	// Init code looping until all the gas is consumed (JUMPDEST, PUSH1 0, JUMP).
	const GAS_BURNER_INIT_CODE = "0x5b600056";
	// Close to the largest transaction the runtime accepts: it fills more than half of the block, so the base fee increases.
	const GAS_BURNER_GAS_LIMIT = 54_000_000;

	const signer = () => new ethers.Wallet(GENESIS_ACCOUNT_PRIVATE_KEY, context.ethersjs);
	const hex = (value: number | bigint) => "0x" + value.toString(16);

	let nonce = 0;
	// Effective priority fee each sent transaction is expected to pay, by transaction hash.
	const expectedTips: { [hash: string]: bigint } = {};
	// Base fee each block reports, by block number.
	const baseFees: { [number: number]: bigint } = {};

	async function rpc(method: string, params: any[]) {
		const response = await customRequest(context.web3, method, params);
		if (response.error) {
			throw new Error(`${method} failed: ${JSON.stringify(response.error)}`);
		}
		return response.result;
	}

	// Fee the next block is executed with.
	async function nextBlockBaseFee(): Promise<bigint> {
		return BigInt(await rpc("eth_gasPrice", []));
	}

	// EIP-1559 transaction. The fee cap is `baseFee + capOverBaseFee`, or twice the base fee when not given.
	async function sendEip1559(maxPriorityFeePerGas: number, capOverBaseFee?: number) {
		const baseFee = await nextBlockBaseFee();
		const maxPriority = BigInt(maxPriorityFeePerGas);
		const maxFee = capOverBaseFee === undefined ? baseFee * BigInt(2) : baseFee + BigInt(capOverBaseFee);
		const tx = await signer().sendTransaction({
			to: "0x1111111111111111111111111111111111111111",
			value: "0x00",
			maxFeePerGas: hex(maxFee),
			maxPriorityFeePerGas: hex(maxPriority),
			nonce: nonce++,
			gasLimit: "0x5208",
			chainId: CHAIN_ID,
		});
		const capLeft = maxFee - baseFee;
		expectedTips[tx.hash] = maxPriority < capLeft ? maxPriority : capLeft;
	}

	async function sendLegacy(overBaseFee: number) {
		const baseFee = await nextBlockBaseFee();
		const tx = await signer().sendTransaction({
			type: 0,
			to: "0x1111111111111111111111111111111111111111",
			value: "0x00",
			gasPrice: hex(baseFee + BigInt(overBaseFee)),
			nonce: nonce++,
			gasLimit: "0x5208",
			chainId: CHAIN_ID,
		});
		expectedTips[tx.hash] = BigInt(overBaseFee);
	}

	// Fails with all its gas consumed, leaving the block more than half full.
	async function sendGasBurner(maxPriorityFeePerGas: number) {
		const baseFee = await nextBlockBaseFee();
		const maxPriority = BigInt(maxPriorityFeePerGas);
		const tx = await signer().sendTransaction({
			data: GAS_BURNER_INIT_CODE,
			value: "0x00",
			maxFeePerGas: hex(baseFee * BigInt(2)),
			maxPriorityFeePerGas: hex(maxPriority),
			nonce: nonce++,
			gasLimit: hex(GAS_BURNER_GAS_LIMIT),
			chainId: CHAIN_ID,
		});
		expectedTips[tx.hash] = maxPriority;
	}

	// A mix of zero and nonzero tips, including one limited by the fee cap and one legacy transaction.
	async function sendMixedTransactions() {
		await sendEip1559(0);
		await sendEip1559(7);
		await sendEip1559(1_000_000, 3);
		await sendLegacy(11);
	}

	async function latestBlockNumber(): Promise<number> {
		return parseInt(await rpc("eth_blockNumber", []), 16);
	}

	// Checks every block from 1 to the latest, and records the base fee they report.
	async function verifyBlocks() {
		const latest = await latestBlockNumber();
		const feeHistory = await rpc("eth_feeHistory", [hex(latest), "latest", [0, 100]]);
		expect(parseInt(feeHistory.oldestBlock, 16)).to.equal(1);
		// The requested blocks, plus the one following the latest.
		expect(feeHistory.baseFeePerGas.length).to.equal(latest + 1);
		expect(feeHistory.gasUsedRatio.length).to.equal(latest);

		for (let number = 1; number <= latest; number++) {
			const block = await rpc("eth_getBlockByNumber", [hex(number), true]);
			const blockByHash = await rpc("eth_getBlockByHash", [block.hash, true]);
			const baseFee = BigInt(block.baseFeePerGas);
			baseFees[number] = baseFee;

			// Block by number, block by hash and fee history report the same base fee.
			expect(BigInt(blockByHash.baseFeePerGas), `block ${number} by hash`).to.equal(baseFee);
			expect(BigInt(feeHistory.baseFeePerGas[number - 1]), `block ${number} fee history`).to.equal(baseFee);

			const tips: bigint[] = [];
			for (const transaction of block.transactions) {
				const receipt = await rpc("eth_getTransactionReceipt", [transaction.hash]);
				const effectiveGasPrice = BigInt(receipt.effectiveGasPrice);
				const tip = effectiveGasPrice - baseFee;
				tips.push(tip);

				// Transaction and receipt agree, and base fee plus tip explains the price paid.
				const description = `block ${number} transaction ${transaction.hash}`;
				expect(BigInt(transaction.gasPrice), description).to.equal(effectiveGasPrice);
				const byHash = await rpc("eth_getTransactionByHash", [transaction.hash]);
				expect(BigInt(byHash.gasPrice), description).to.equal(effectiveGasPrice);
				expect(tip, description).to.equal(expectedTips[transaction.hash]);
				if (transaction.type == "0x2") {
					const capLeft = BigInt(transaction.maxFeePerGas) - baseFee;
					const maxPriority = BigInt(transaction.maxPriorityFeePerGas);
					expect(tip, description).to.equal(maxPriority < capLeft ? maxPriority : capLeft);
				}
			}

			// The rewards of the fee history are the effective priority fees paid in the block.
			const [lowest, highest] =
				tips.length == 0
					? [BigInt(0), BigInt(0)]
					: [tips.reduce((a, b) => (a < b ? a : b)), tips.reduce((a, b) => (a > b ? a : b))];
			expect(BigInt(feeHistory.reward[number - 1][0]), `block ${number} lowest reward`).to.equal(lowest);
			expect(BigInt(feeHistory.reward[number - 1][1]), `block ${number} highest reward`).to.equal(highest);
		}
	}

	// The fee history entry after the latest block is the fee of the block to be created next.
	async function verifyNextBlockEntryAndCreateBlock() {
		const feeHistory = await rpc("eth_feeHistory", ["0x1", "latest", []]);
		expect(feeHistory.baseFeePerGas.length).to.equal(2);
		const nextBaseFee = BigInt(feeHistory.baseFeePerGas[1]);
		expect(nextBaseFee).to.equal(await nextBlockBaseFee());

		await createAndFinalizeBlock(context.web3);
		const number = await latestBlockNumber();
		const block = await rpc("eth_getBlockByNumber", [hex(number), false]);
		expect(BigInt(block.baseFeePerGas)).to.equal(nextBaseFee);
	}

	// Expects the base fee to move the same way between the given blocks, both included.
	function expectBaseFeeTrend(from: number, to: number, trend: "decreasing" | "unchanged" | "increasing") {
		for (let number = from + 1; number <= to; number++) {
			const previous = baseFees[number - 1];
			const current = baseFees[number];
			const description = `base fee of block ${number} (${current}) after block ${number - 1} (${previous})`;
			if (trend == "decreasing") {
				expect(current < previous, description).to.be.true;
			} else if (trend == "increasing") {
				expect(current > previous, description).to.be.true;
			} else {
				expect(current, description).to.equal(previous);
			}
		}
	}

	step("should report the base fee used to execute a decreasing base fee block", async function () {
		this.timeout(100000);
		// The genesis block reports the initial base fee.
		const genesis = await rpc("eth_getBlockByNumber", ["0x0", false]);
		expect(BigInt(genesis.baseFeePerGas)).to.equal(INITIAL_BASE_FEE);

		// Blocks 1 to 3 are less than half full, so every block leaves a lower base fee.
		for (let block = 0; block < 3; block++) {
			await sendMixedTransactions();
			await createAndFinalizeBlock(context.web3);
		}
		await verifyBlocks();

		// The first block is executed with the genesis base fee. Reporting the fee it leaves behind
		// would show an already decreased value.
		expect(baseFees[1]).to.equal(INITIAL_BASE_FEE);
		expectBaseFeeTrend(1, 3, "decreasing");
		await verifyNextBlockEntryAndCreateBlock();
		await verifyBlocks();
		expectBaseFeeTrend(1, 4, "decreasing");
	});

	step("should report the base fee used to execute an unchanged base fee block", async function () {
		this.timeout(100000);
		// Empty blocks keep decreasing the base fee until it reaches its floor.
		for (let blocks = 0; blocks < 20 && (await nextBlockBaseFee()) != BASE_FEE_FLOOR; blocks++) {
			await createAndFinalizeBlock(context.web3);
		}
		expect(await nextBlockBaseFee()).to.equal(BASE_FEE_FLOOR);

		// The fee left by every following block is the floor again.
		const first = (await latestBlockNumber()) + 1;
		for (let block = 0; block < 3; block++) {
			await sendMixedTransactions();
			await createAndFinalizeBlock(context.web3);
		}
		await verifyBlocks();
		expect(baseFees[first]).to.equal(BASE_FEE_FLOOR);
		expectBaseFeeTrend(first, first + 2, "unchanged");
		await verifyNextBlockEntryAndCreateBlock();
		await verifyBlocks();
		expectBaseFeeTrend(first, first + 3, "unchanged");
	});

	step("should report the base fee used to execute an increasing base fee block", async function () {
		this.timeout(100000);
		// Blocks more than half full leave a higher base fee.
		const first = (await latestBlockNumber()) + 1;
		for (let block = 0; block < 2; block++) {
			await sendGasBurner(5);
			await sendMixedTransactions();
			await createAndFinalizeBlock(context.web3);
		}
		await createAndFinalizeBlock(context.web3);
		await verifyBlocks();

		// The first burner block is executed with the floor: the increase shows up in the next block.
		expect(baseFees[first]).to.equal(BASE_FEE_FLOOR);
		expectBaseFeeTrend(first, first + 2, "increasing");
		await verifyNextBlockEntryAndCreateBlock();
		await verifyBlocks();
	});

	step("should keep the base fee of old blocks when new blocks are produced", async function () {
		this.timeout(100000);
		const recorded = { ...baseFees };
		await sendMixedTransactions();
		await createAndFinalizeBlock(context.web3);
		await verifyBlocks();
		for (const number of Object.keys(recorded).map(Number)) {
			expect(baseFees[number]).to.equal(recorded[number]);
		}
	});
});
