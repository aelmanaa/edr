//! Restores locally mined blocks from the representation a saved chain would
//! use: the block and its receipts as the chain's RPC types, and whether each
//! transaction comes from an impersonated account.
//!
//! The header goes through the RPC block, like remote blocks do, because
//! neither of `BlockHeader`'s own encodings round-trips: RLP decoding misreads
//! a blob gas of zero, and JSON writes `gasUsed` twice.

use std::sync::Arc;

use edr_block_api::{Block as _, BlockReceipts as _, EthBlockData, FetchBlockReceipts as _};
use edr_block_local::LocalBlockFromPartsError;
use edr_chain_l1::{
    rpc::{
        block::L1RpcBlock, receipt::L1RpcTransactionReceipt,
        transaction::L1RpcTransactionWithSignature,
    },
    L1ChainSpec, L1SignedTransaction,
};
use edr_chain_spec_rpc::RpcTypeFrom as _;
use edr_eth::PreEip1898BlockSpec;
use edr_primitives::{Address, U256};
use edr_signer::FakeableSignature;
use edr_transaction::{request::TransactionRequestAndSender, TxKind};

use crate::{requests::eth::handle_get_block_by_number_request, test_utils::ProviderTestFixture};

type L1LocalBlock = <L1ChainSpec as edr_block_api::GenesisBlockFactory>::LocalBlock;
type L1BlockReceipt = <L1ChainSpec as edr_chain_spec_receipt::ReceiptChainSpec>::Receipt;

fn is_impersonated(transaction: &L1SignedTransaction) -> bool {
    match transaction {
        L1SignedTransaction::PreEip155Legacy(transaction) => transaction.signature.is_fake(),
        L1SignedTransaction::PostEip155Legacy(transaction) => transaction.signature.is_fake(),
        L1SignedTransaction::Eip2930(transaction) => transaction.signature.is_fake(),
        L1SignedTransaction::Eip1559(transaction) => transaction.signature.is_fake(),
        L1SignedTransaction::Eip4844(transaction) => transaction.signature.is_fake(),
        L1SignedTransaction::Eip7702(transaction) => transaction.signature.is_fake(),
    }
}

/// Restores a fake signature, which the RPC type can't express: converting
/// it back recovers a real signature with the RPC `from` as its address, and
/// a typed transaction's `v` becomes its y-parity. `rpc_v` is the `v` that the
/// RPC type had.
fn restore_fake_signature(transaction: &mut L1SignedTransaction, rpc_v: u64) {
    fn fake<SignatureT: edr_signer::Recoverable + edr_signer::Signature>(
        signature: &mut FakeableSignature<SignatureT>,
        rpc_v: u64,
    ) {
        let recovery_id = rpc_v.checked_sub(27).expect("fake signatures have v >= 27");
        *signature = FakeableSignature::fake(*signature.caller(), Some(recovery_id));
    }

    match transaction {
        L1SignedTransaction::PreEip155Legacy(transaction) => {
            fake(&mut transaction.signature, rpc_v);
        }
        L1SignedTransaction::PostEip155Legacy(transaction) => {
            fake(&mut transaction.signature, rpc_v);
        }
        L1SignedTransaction::Eip2930(transaction) => fake(&mut transaction.signature, rpc_v),
        L1SignedTransaction::Eip1559(transaction) => fake(&mut transaction.signature, rpc_v),
        L1SignedTransaction::Eip4844(transaction) => fake(&mut transaction.signature, rpc_v),
        L1SignedTransaction::Eip7702(transaction) => fake(&mut transaction.signature, rpc_v),
    }
}

/// Mines a block with a signed transaction and two impersonated ones, one of
/// them typed, and returns the fixture.
fn fixture_with_mined_block() -> anyhow::Result<ProviderTestFixture<L1ChainSpec>> {
    let mut fixture = ProviderTestFixture::<L1ChainSpec>::new_local()?;

    let impersonated_eip1559 =
        fixture
            .provider_data
            .sign_transaction_request(TransactionRequestAndSender {
                request: edr_chain_l1::L1TransactionRequest::Eip1559(
                    edr_chain_l1::request::Eip1559 {
                        chain_id: fixture.config.chain_id,
                        nonce: 1,
                        max_priority_fee_per_gas: 1_000_000_000,
                        max_fee_per_gas: 100_000_000_000,
                        gas_limit: 30_000,
                        kind: TxKind::Call(Address::ZERO),
                        value: U256::from(2),
                        input: edr_primitives::Bytes::new(),
                        access_list: Vec::new(),
                    },
                ),
                sender: fixture.impersonated_account,
            })?;

    for transaction in [
        fixture.signed_dummy_transaction(0, None)?,
        fixture.impersonated_dummy_transaction()?,
        impersonated_eip1559,
    ] {
        fixture.provider_data.add_pending_transaction(transaction)?;
    }
    let result = fixture
        .provider_data
        .mine_and_commit_block(fixture.provider_data.header_overrides())?;
    assert_eq!(result.block.transactions().len(), 3);

    Ok(fixture)
}

/// The block as a saved chain stores it.
struct SavedBlock {
    /// `eth_getBlockByNumber` with full transactions.
    block_json: String,
    receipts_json: Vec<String>,
    /// Whether each transaction comes from an impersonated account.
    impersonated: Vec<bool>,
}

fn save_last_block(fixture: &mut ProviderTestFixture<L1ChainSpec>) -> anyhow::Result<SavedBlock> {
    let data = &mut fixture.provider_data;
    let block = data.blockchain.last_block()?;
    assert!(
        block.ommer_hashes().is_empty(),
        "mined blocks have no ommers"
    );

    let rpc_block = handle_get_block_by_number_request(
        data,
        PreEip1898BlockSpec::Number(block.block_header().number),
        true,
    )?
    .expect("the block exists");

    let receipts_json = block
        .fetch_transaction_receipts()?
        .iter()
        .map(|receipt| {
            serde_json::to_string(&L1RpcTransactionReceipt::rpc_type_from(
                receipt.as_ref(),
                data.hardfork(),
            ))
        })
        .collect::<Result<_, _>>()?;

    Ok(SavedBlock {
        block_json: serde_json::to_string(&rpc_block)?,
        receipts_json,
        impersonated: block.transactions().iter().map(is_impersonated).collect(),
    })
}

fn restore_block(saved: &SavedBlock) -> anyhow::Result<L1LocalBlock> {
    let rpc_block: L1RpcBlock<L1RpcTransactionWithSignature> =
        serde_json::from_str(&saved.block_json)?;
    let rpc_vs: Vec<u64> = rpc_block
        .transactions
        .iter()
        .map(|transaction| transaction.v)
        .collect();

    let EthBlockData {
        header,
        mut transactions,
        ommer_hashes,
        withdrawals,
        hash,
        ..
    } = EthBlockData::<L1SignedTransaction>::try_from(rpc_block)?;
    assert!(ommer_hashes.is_empty());

    for ((transaction, rpc_v), is_impersonated) in
        transactions.iter_mut().zip(rpc_vs).zip(&saved.impersonated)
    {
        if *is_impersonated {
            restore_fake_signature(transaction, rpc_v);
        }
    }

    let receipts = saved
        .receipts_json
        .iter()
        .map(|json| {
            let rpc: L1RpcTransactionReceipt = serde_json::from_str(json)?;
            Ok(Arc::new(L1BlockReceipt::try_from(rpc)?))
        })
        .collect::<anyhow::Result<_>>()?;

    let block = L1LocalBlock::from_parts(header, transactions, receipts, Vec::new(), withdrawals)?;
    anyhow::ensure!(
        *block.block_hash() == hash,
        "the restored block has another hash"
    );

    Ok(block)
}

#[test]
fn mined_block_is_restored_from_rpc_types() -> anyhow::Result<()> {
    let mut fixture = fixture_with_mined_block()?;
    let saved = save_last_block(&mut fixture)?;
    assert_eq!(saved.impersonated, [false, true, true]);

    let data = &mut fixture.provider_data;
    let original = data.blockchain.last_block()?;

    let restored = restore_block(&saved)?;

    assert_eq!(restored.block_hash(), original.block_hash());
    assert_eq!(restored.block_header(), original.block_header());
    assert_eq!(restored.transactions(), original.transactions());
    assert_eq!(
        restored.transaction_receipts(),
        original.fetch_transaction_receipts()?.as_slice()
    );
    assert_eq!(restored.withdrawals(), original.withdrawals());

    let original_block = original.clone();
    let hardfork = data.hardfork();

    // The restored transactions give the same `eth_getTransactionByHash`
    // output, including `v` for impersonated transactions, and the restored
    // receipts the same `eth_getTransactionReceipt` output.
    for (index, (restored, original)) in restored
        .transactions()
        .iter()
        .zip(original.transactions())
        .enumerate()
    {
        let rpc_json = |transaction: &L1SignedTransaction| -> anyhow::Result<String> {
            let transaction_and_block = edr_transaction::TransactionAndBlock {
                transaction: transaction.clone(),
                block_data: Some(edr_transaction::BlockDataForTransaction {
                    block: original_block.clone(),
                    transaction_index: u64::try_from(index)?,
                }),
                is_pending: false,
            };
            Ok(serde_json::to_string(
                &L1RpcTransactionWithSignature::rpc_type_from(&transaction_and_block, hardfork),
            )?)
        };
        assert_eq!(
            rpc_json(restored)?,
            rpc_json(original)?,
            "transaction {index}"
        );
    }
    for (receipt, json) in restored
        .transaction_receipts()
        .iter()
        .zip(&saved.receipts_json)
    {
        let rpc = L1RpcTransactionReceipt::rpc_type_from(receipt.as_ref(), hardfork);
        assert_eq!(&serde_json::to_string(&rpc)?, json);
    }

    Ok(())
}

#[test]
fn without_the_impersonation_flag_the_restored_block_differs() -> anyhow::Result<()> {
    let mut fixture = fixture_with_mined_block()?;
    let original = fixture.provider_data.blockchain.last_block()?;

    let mut saved = save_last_block(&mut fixture)?;
    saved.impersonated.fill(false);
    let restored = restore_block(&saved)?;

    // Same hashes, but the signatures of the impersonated transactions are
    // no longer fake.
    assert_eq!(restored.block_hash(), original.block_hash());
    assert_ne!(restored.transactions(), original.transactions());

    Ok(())
}

#[test]
fn from_parts_checks_the_transactions_and_receipts() -> anyhow::Result<()> {
    let mut fixture = fixture_with_mined_block()?;

    let mut saved = save_last_block(&mut fixture)?;
    saved.receipts_json.pop();
    let error = restore_block(&saved)
        .expect_err("a receipt is missing")
        .downcast::<LocalBlockFromPartsError>()?;
    assert_eq!(
        error,
        LocalBlockFromPartsError::ReceiptCount {
            transactions: 3,
            receipts: 2,
        }
    );

    // A transaction removed from the block no longer matches the header's
    // transactions root.
    let mut saved = save_last_block(&mut fixture)?;
    let mut rpc_block: serde_json::Value = serde_json::from_str(&saved.block_json)?;
    let expected_root: edr_primitives::B256 =
        serde_json::from_value(rpc_block["transactionsRoot"].clone())?;
    rpc_block["transactions"]
        .as_array_mut()
        .expect("transactions are an array")
        .pop();
    saved.block_json = rpc_block.to_string();
    saved.receipts_json.pop();
    saved.impersonated.pop();
    let error = restore_block(&saved)
        .expect_err("a transaction is missing")
        .downcast::<LocalBlockFromPartsError>()?;
    assert!(matches!(
        error,
        LocalBlockFromPartsError::TransactionsRoot { expected, .. } if expected == expected_root
    ));

    Ok(())
}

#[test]
fn local_block_parts_restore_the_provider_blockchain() -> anyhow::Result<()> {
    use edr_blockchain_api::{GetBlockchainBlock as _, StateAtBlock as _};
    use edr_blockchain_local::{LocalBlockchain, LocalBlockchainParts};
    use edr_state_api::StateDebug as _;

    let mut fixture = fixture_with_mined_block()?;
    // Blocks 2 and 3 are mined, 4 to 10 reserved, and 11 mined.
    fixture.provider_data.mine_and_commit_blocks(10, 1)?;

    let data = &fixture.provider_data;
    let parts = LocalBlockchainParts {
        chain_id: data.blockchain.chain_id(),
        hardfork: data.blockchain.hardfork(),
        storage: data.blockchain.local_block_parts(),
    };
    assert!(!parts.storage.reservations.is_empty());

    let restored: LocalBlockchain<L1BlockReceipt, _, L1LocalBlock, L1SignedTransaction> =
        LocalBlockchain::from_parts(parts, &data.block_config)?;

    let overrides = data.irregular_state.state_overrides();
    for number in 0..=data.blockchain.last_block_number() {
        // The state first: reading a reserved block creates it.
        assert_eq!(
            restored
                .state_at_block_number(number, overrides)?
                .state_root()?,
            data.blockchain
                .state_at_block_number(number, overrides)?
                .state_root()?,
            "block {number}"
        );

        let restored_block: Arc<<L1ChainSpec as edr_chain_spec_block::BlockChainSpec>::Block> =
            restored.block_by_number(number)?.expect("block exists");
        let original = data
            .blockchain
            .block_by_number(number)?
            .expect("block exists");
        assert_eq!(
            restored_block.block_hash(),
            original.block_hash(),
            "block {number}"
        );
    }

    Ok(())
}
