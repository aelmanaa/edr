use std::{collections::BTreeMap, sync::Arc};

use edr_block_api::Block as _;
use edr_block_header::BlockConfig;
use edr_block_storage::FromPartsError;
use edr_blockchain_api::{
    BlockchainMetadata as _, GetBlockchainBlock as _, GetBlockchainLogs as _, InsertBlock,
    LocalStateDiffs as _, ReceiptByTransactionHash as _, ReserveBlocks as _, StateAtBlock as _,
    TotalDifficultyByBlockHash as _,
};
use edr_chain_l1::L1ChainSpec;
use edr_primitives::{Address, HashSet, B256, U256};
use edr_provider::spec::LocalBlockchainForChainSpec;
use edr_state_api::{
    account::{Account, AccountInfo, AccountStatus},
    EvmState, StateDebug as _, StateDiff,
};
use edr_test_blockchain::{create_dummy_block, insert_dummy_block_with_transaction, DynSyncBlock};

use crate::common::{create_local_blockchain_with_genesis_diff, dummy_block_config};

type L1LocalBlockchain = LocalBlockchainForChainSpec<L1ChainSpec>;

fn block(
    blockchain: &L1LocalBlockchain,
    number: u64,
) -> anyhow::Result<Option<Arc<DynSyncBlock<L1ChainSpec>>>> {
    Ok(blockchain.block_by_number(number)?)
}

fn balance_diff(address: Address, balance: u64) -> StateDiff {
    let mut account = Account::from(AccountInfo {
        balance: U256::from(balance),
        ..AccountInfo::default()
    });
    account.status = AccountStatus::Created | AccountStatus::Touched;

    EvmState::from_iter([(address, account)]).into()
}

/// A chain with blocks that have transactions, a reservation that was partly
/// read, and a reservation that wasn't read.
fn blockchain_with_blocks_and_reservations() -> anyhow::Result<(L1LocalBlockchain, Vec<B256>)> {
    let block_config = dummy_block_config();
    let mut blockchain = create_local_blockchain_with_genesis_diff(balance_diff(
        Address::repeat_byte(0xaa),
        1_000_000_000,
    ));

    let mut transaction_hashes = Vec::new();
    for _ in 0..2 {
        transaction_hashes
            .push(insert_dummy_block_with_transaction(&mut blockchain)?.transaction_hash);
    }

    // Blocks 3 to 7, of which 5 is read and so created.
    blockchain.reserve_blocks(&block_config, 5, 12)?;
    assert!(block(&blockchain, 5)?.is_some());

    // Block 8 changes the state. Creating it reads the last block, which
    // creates block 7 too.
    let block = create_dummy_block(&blockchain);
    InsertBlock::<DynSyncBlock<L1ChainSpec>, _, _>::insert_block(
        &mut blockchain,
        block,
        balance_diff(Address::repeat_byte(0xbb), 7),
    )?;

    // Blocks 9 to 1008, none of which is read.
    blockchain.reserve_blocks(&block_config, 1000, 1)?;

    Ok((blockchain, transaction_hashes))
}

fn assert_same_block(
    expected: &L1LocalBlockchain,
    actual: &L1LocalBlockchain,
    number: u64,
) -> anyhow::Result<()> {
    // The state is compared before the block is read: reading a reserved block
    // creates it, and EDR then loses its state (fixed separately).
    let overrides = BTreeMap::default();
    assert_eq!(
        actual
            .state_at_block_number(number, &overrides)?
            .state_root()?,
        expected
            .state_at_block_number(number, &overrides)?
            .state_root()?,
        "block {number}"
    );

    let expected_block = block(expected, number)?.expect("block exists");
    let actual_block = block(actual, number)?.expect("block exists");
    assert_eq!(
        actual_block.block_hash(),
        expected_block.block_hash(),
        "block {number}"
    );
    assert_eq!(
        actual_block.block_header(),
        expected_block.block_header(),
        "block {number}"
    );
    assert_eq!(
        actual.total_difficulty_by_hash(actual_block.block_hash())?,
        expected.total_difficulty_by_hash(expected_block.block_hash())?,
        "block {number}"
    );

    Ok(())
}

#[test]
fn from_parts_restores_blocks_receipts_logs_and_reservations() -> anyhow::Result<()> {
    let (expected, transaction_hashes) = blockchain_with_blocks_and_reservations()?;

    let parts = expected.to_parts();
    assert_eq!(parts.storage.reservations.len(), 3, "3-4, 6, and 9-1008");
    // Reserved blocks keep the hardfork of their reservation, not the one of
    // the config.
    let block_config = BlockConfig {
        hardfork: edr_chain_l1::Hardfork::Shanghai,
        ..dummy_block_config()
    };
    let actual = L1LocalBlockchain::from_parts(parts, &block_config)?;

    assert_eq!(actual.chain_id(), expected.chain_id());
    assert_eq!(actual.hardfork(), expected.hardfork());
    assert_eq!(actual.last_block_number(), expected.last_block_number());

    let diff_numbers = |blockchain: &L1LocalBlockchain| -> Vec<u64> {
        blockchain
            .local_state_diffs()
            .iter()
            .map(|(number, _)| *number)
            .collect()
    };
    assert_eq!(diff_numbers(&actual), diff_numbers(&expected));
    for ((_, actual_diff), (_, expected_diff)) in actual
        .local_state_diffs()
        .iter()
        .zip(expected.local_state_diffs())
    {
        assert_eq!(actual_diff.as_inner(), expected_diff.as_inner());
    }

    for transaction_hash in &transaction_hashes {
        let expected_receipt = expected
            .receipt_by_transaction_hash(transaction_hash)?
            .expect("receipt exists");
        let actual_receipt = actual
            .receipt_by_transaction_hash(transaction_hash)?
            .expect("receipt exists");
        assert_eq!(actual_receipt, expected_receipt);
    }

    let last = expected.last_block_number();
    assert_eq!(
        actual.logs(0, last, &HashSet::default(), &[])?,
        expected.logs(0, last, &HashSet::default(), &[])?
    );

    // Stored blocks, reserved blocks that are created on first read (3, 4, 6,
    // and 9 to 1008), and the last block.
    for number in (0..=10).chain([500, last]) {
        assert_same_block(&expected, &actual, number)?;
    }

    Ok(())
}

#[test]
fn to_parts_does_not_create_reserved_blocks() -> anyhow::Result<()> {
    let (blockchain, _) = blockchain_with_blocks_and_reservations()?;

    let parts = blockchain.to_parts();
    let numbers: Vec<u64> = parts
        .storage
        .blocks
        .iter()
        .map(|block| block.block.block_header().number)
        .collect();
    assert_eq!(numbers, [0, 1, 2, 5, 7, 8]);

    let numbers_with_diff: Vec<u64> = parts
        .storage
        .blocks
        .iter()
        .filter(|block| block.state_diff.is_some())
        .map(|block| block.block.block_header().number)
        .collect();
    assert_eq!(numbers_with_diff, [0, 1, 2, 8]);

    let reserved: Vec<(u64, u64)> = parts
        .storage
        .reservations
        .iter()
        .map(|reservation| (reservation.first_number, reservation.last_number))
        .collect();
    assert_eq!(reserved, [(3, 4), (6, 6), (9, 1008)]);

    Ok(())
}

#[test]
fn from_parts_rejects_inconsistent_parts() -> anyhow::Result<()> {
    let (blockchain, _) = blockchain_with_blocks_and_reservations()?;
    let block_config = dummy_block_config();

    let mut parts = blockchain.to_parts();
    parts.storage.blocks.swap(1, 2);
    assert!(matches!(
        L1LocalBlockchain::from_parts(parts, &block_config),
        Err(FromPartsError::BlocksOutOfOrder { block_number: 1 })
    ));

    let mut parts = blockchain.to_parts();
    parts.storage.last_block_number = 1007;
    assert!(matches!(
        L1LocalBlockchain::from_parts(parts, &block_config),
        Err(FromPartsError::AfterLastBlock {
            block_number: 1008,
            last_block_number: 1007
        })
    ));

    let mut parts = blockchain.to_parts();
    parts.storage.reservations[2].first_number = 8;
    assert!(matches!(
        L1LocalBlockchain::from_parts(parts, &block_config),
        Err(FromPartsError::InvalidReservation {
            first_number: 8,
            last_number: 1008
        })
    ));

    let mut parts = blockchain.to_parts();
    parts.storage.reservations[1].first_number = 4;
    assert!(matches!(
        L1LocalBlockchain::from_parts(parts, &block_config),
        Err(FromPartsError::InvalidReservation {
            first_number: 4,
            last_number: 6
        })
    ));

    let mut parts = blockchain.to_parts();
    parts.storage.reservations[2].previous_state_diff_block_number = 7;
    assert!(matches!(
        L1LocalBlockchain::from_parts(parts, &block_config),
        Err(FromPartsError::UnknownPreviousStateDiff {
            block_number: 7,
            ..
        })
    ));

    Ok(())
}
