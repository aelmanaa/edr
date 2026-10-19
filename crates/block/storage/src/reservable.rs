use core::fmt::Debug;
use std::num::NonZeroU64;

use edr_block_api::{Block, BlockReceipts, EmptyBlock, FetchBlockReceipts, LocalBlock};
use edr_block_header::{BlockConfig, HeaderOverrides, PartialHeader};
use edr_chain_spec::{EvmSpecId, ExecutableTransaction, ProtocolHardfork};
use edr_primitives::{Address, HashMap, HashSet, B256, U256};
use edr_receipt::{log::FilterLog, ExecutionReceipt, ReceiptTrait};
use edr_state_api::StateDiff;
use parking_lot::{RwLock, RwLockUpgradableReadGuard, RwLockWriteGuard};

use super::{sparse, InsertBlockError, SparseBlockStorage};
use crate::{InsertBlockAndReceiptsError, InsertReceiptError};

/// A reservation for a sequence of blocks that have not yet been inserted into
/// storage.
#[derive(Clone, Debug)]
struct Reservation<HardforkT> {
    first_number: u64,
    last_number: u64,
    interval: u64,
    previous_base_fee_per_gas: Option<u128>,
    previous_state_root: B256,
    previous_total_difficulty: U256,
    previous_diff_index: usize,
    block_config: BlockConfig<HardforkT>,
}

/// A block in [`ReservableSparseBlockStorageParts`].
#[derive(Clone, Debug)]
pub struct StoredBlockParts<BlockT> {
    /// The block.
    pub block: BlockT,
    /// The total difficulty of the chain up to and including the block.
    pub total_difficulty: U256,
    /// The state diff of the block, or `None` for an empty block that was
    /// created from a reservation when it was first read.
    pub state_diff: Option<StateDiff>,
}

/// A reservation in [`ReservableSparseBlockStorageParts`]: a range of empty
/// blocks that are created when they are first read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservationParts<HardforkT> {
    /// The number of the first reserved block.
    pub first_number: u64,
    /// The number of the last reserved block.
    pub last_number: u64,
    /// The interval between the timestamps of the reserved blocks.
    pub interval: u64,
    /// The base fee per gas of the block before the reservation.
    pub previous_base_fee_per_gas: Option<u128>,
    /// The state root of the block before the reservation.
    pub previous_state_root: B256,
    /// The total difficulty of the block before the reservation.
    pub previous_total_difficulty: U256,
    /// The number of the last block before the reservation that has a state
    /// diff.
    pub previous_state_diff_block_number: u64,
    /// The hardfork of the reserved blocks.
    pub hardfork: HardforkT,
}

/// The contents of a [`ReservableSparseBlockStorage`], from which an equal
/// storage can be constructed with
/// [`ReservableSparseBlockStorage::from_parts`].
#[derive(Clone, Debug)]
pub struct ReservableSparseBlockStorageParts<BlockT, HardforkT> {
    /// The stored blocks, ordered by number.
    pub blocks: Vec<StoredBlockParts<BlockT>>,
    /// The reservations, ordered by their first block number.
    pub reservations: Vec<ReservationParts<HardforkT>>,
    /// The number of the last block, stored or reserved.
    pub last_block_number: u64,
}

/// An error that occurs when constructing a [`ReservableSparseBlockStorage`]
/// from its parts.
#[derive(Debug, thiserror::Error)]
pub enum FromPartsError {
    /// A block isn't ordered after the previous block.
    #[error("Block {block_number} isn't after the previous block.")]
    BlocksOutOfOrder {
        /// The number of the block.
        block_number: u64,
    },
    /// A block or reservation is after the last block number.
    #[error("Block {block_number} is after the last block number {last_block_number}.")]
    AfterLastBlock {
        /// The number of the block.
        block_number: u64,
        /// The last block number.
        last_block_number: u64,
    },
    /// A reservation is empty, or overlaps a block or another reservation.
    #[error("The reservation of blocks {first_number} to {last_number} is empty or overlaps another block.")]
    InvalidReservation {
        /// The number of the first reserved block.
        first_number: u64,
        /// The number of the last reserved block.
        last_number: u64,
    },
    /// A reservation refers to a block without a state diff, or to a block
    /// that isn't before it.
    #[error("The reservation of blocks {first_number} to {last_number} refers to block {block_number}, which isn't an earlier block with a state diff.")]
    UnknownPreviousStateDiff {
        /// The number of the first reserved block.
        first_number: u64,
        /// The number of the last reserved block.
        last_number: u64,
        /// The number of the block the reservation refers to.
        block_number: u64,
    },
    /// A block or receipt couldn't be inserted.
    #[error(transparent)]
    Insert(#[from] InsertBlockAndReceiptsError),
}

impl From<InsertReceiptError> for FromPartsError {
    fn from(error: InsertReceiptError) -> Self {
        Self::Insert(error.into())
    }
}

impl From<InsertBlockError> for FromPartsError {
    fn from(error: InsertBlockError) -> Self {
        Self::Insert(error.into())
    }
}

/// A storage solution for storing a subset of a blockchain's blocks in-memory,
/// while lazily loading blocks that have been reserved.
#[derive(Debug)]
pub struct ReservableSparseBlockStorage<
    BlockReceiptT: ReceiptTrait,
    BlockT,
    HardforkT,
    SignedTransactionT,
> {
    reservations: RwLock<Vec<Reservation<HardforkT>>>,
    storage: RwLock<SparseBlockStorage<BlockReceiptT, BlockT, SignedTransactionT>>,
    // We can store the state diffs contiguously, as reservations don't contain any diffs.
    // Diffs are a mapping from one state to the next, so the genesis block contains the initial
    // state.
    state_diffs: Vec<(u64, StateDiff)>,
    number_to_diff_index: HashMap<u64, usize>,
    last_block_number: u64,
}

impl<BlockReceiptT: ReceiptTrait, BlockT, HardforkT, SignedTransactionT>
    ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Constructs a new instance with no blocks.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn empty(last_block_number: u64) -> Self {
        Self {
            reservations: RwLock::new(Vec::new()),
            storage: RwLock::new(SparseBlockStorage::default()),
            state_diffs: Vec::new(),
            number_to_diff_index: HashMap::default(),
            last_block_number,
        }
    }

    /// Retrieves the state diffs of all stored blocks, ordered by block number.
    pub fn state_diffs(&self) -> &[(u64, StateDiff)] {
        &self.state_diffs
    }
}

impl<
        BlockReceiptT: ReceiptTrait,
        BlockT: Block<SignedTransactionT> + Clone,
        HardforkT,
        SignedTransactionT: ExecutableTransaction,
    > ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Constructs a new instance with the provided block as genesis block.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn with_genesis_block(block: BlockT, diff: StateDiff, total_difficulty: U256) -> Self {
        Self {
            reservations: RwLock::new(Vec::new()),
            storage: RwLock::new(SparseBlockStorage::with_block(block, total_difficulty)),
            state_diffs: vec![(0, diff)],
            number_to_diff_index: std::iter::once((0, 0)).collect(),
            last_block_number: 0,
        }
    }

    /// Reverts to the block with the provided number, deleting all later
    /// blocks.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn revert_to_block(&mut self, block_number: u64) -> bool {
        if block_number > self.last_block_number {
            return false;
        }

        self.last_block_number = block_number;

        self.storage.get_mut().revert_to_block(block_number);

        if block_number == 0 {
            // Reservations and state diffs can only occur after the genesis block,
            // so we can clear them all
            self.reservations.get_mut().clear();

            // Keep the genesis block's diff
            self.state_diffs.truncate(1);

            // Keep the genesis block's mapping
            self.number_to_diff_index.clear();
            self.number_to_diff_index.insert(0, 0);
        } else {
            // Only retain reservations that are not fully reverted
            self.reservations.get_mut().retain_mut(|reservation| {
                if reservation.last_number <= block_number {
                    true
                } else if reservation.first_number <= block_number {
                    reservation.last_number = block_number;
                    true
                } else {
                    false
                }
            });

            // Remove all diffs that are newer than the reverted block
            let diff_index = self
                .number_to_diff_index
                .get(&block_number)
                .copied()
                .unwrap_or_else(|| {
                    let reservations = self.reservations.get_mut();

                    find_reservation(reservations, block_number)
                    .expect("There must either be a block or a reservation matching the block number").previous_diff_index
                });

            self.state_diffs.truncate(diff_index + 1);

            self.number_to_diff_index
                .retain(|number, _| *number <= block_number);
        }

        true
    }
}
impl<
        BlockReceiptT: ReceiptTrait,
        BlockT: Block<SignedTransactionT> + Clone,
        HardforkT: Clone,
        SignedTransactionT,
    > ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Retrieves the contents of the storage, without creating reserved
    /// blocks.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn to_parts(&self) -> ReservableSparseBlockStorageParts<BlockT, HardforkT> {
        let storage = self.storage.read();
        let mut blocks: Vec<_> = storage
            .blocks()
            .map(|block| {
                let block_number = block.block_header().number;
                let total_difficulty = *storage
                    .total_difficulty_by_hash(block.block_hash())
                    .expect("Every stored block has a total difficulty");
                let state_diff = self.number_to_diff_index.get(&block_number).map(|index| {
                    self.state_diffs
                        .get(*index)
                        .expect("Diff indices are valid")
                        .1
                        .clone()
                });

                StoredBlockParts {
                    block: block.clone(),
                    total_difficulty,
                    state_diff,
                }
            })
            .collect();
        blocks.sort_unstable_by_key(|block| block.block.block_header().number);

        let mut reservations: Vec<_> = self
            .reservations
            .read()
            .iter()
            .map(|reservation| ReservationParts {
                first_number: reservation.first_number,
                last_number: reservation.last_number,
                interval: reservation.interval,
                previous_base_fee_per_gas: reservation.previous_base_fee_per_gas,
                previous_state_root: reservation.previous_state_root,
                previous_total_difficulty: reservation.previous_total_difficulty,
                previous_state_diff_block_number: self
                    .state_diffs
                    .get(reservation.previous_diff_index)
                    .expect("Diff indices are valid")
                    .0,
                hardfork: reservation.block_config.hardfork.clone(),
            })
            .collect();
        reservations.sort_unstable_by_key(|reservation| reservation.first_number);

        ReservableSparseBlockStorageParts {
            blocks,
            reservations,
            last_block_number: self.last_block_number,
        }
    }
}

impl<
        BlockReceiptT: ReceiptTrait,
        BlockT: Block<SignedTransactionT> + Clone + LocalBlock<BlockReceiptT>,
        HardforkT: Clone,
        SignedTransactionT: ExecutableTransaction,
    > ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
where
    BlockReceiptT: Clone,
{
    /// Constructs a storage from the parts returned by
    /// [`ReservableSparseBlockStorage::to_parts`].
    ///
    /// The reserved blocks use the provided block config, with the hardfork of
    /// their reservation.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn from_parts(
        parts: ReservableSparseBlockStorageParts<BlockT, HardforkT>,
        block_config: &BlockConfig<HardforkT>,
    ) -> Result<Self, FromPartsError> {
        let ReservableSparseBlockStorageParts {
            blocks,
            reservations: reservation_parts,
            last_block_number,
        } = parts;

        let mut storage = SparseBlockStorage::default();
        let mut state_diffs = Vec::new();
        let mut number_to_diff_index = HashMap::default();
        let mut block_numbers = Vec::with_capacity(blocks.len());

        for StoredBlockParts {
            block,
            total_difficulty,
            state_diff,
        } in blocks
        {
            let block_number = block.block_header().number;
            if block_numbers
                .last()
                .is_some_and(|previous_number| block_number <= *previous_number)
            {
                return Err(FromPartsError::BlocksOutOfOrder { block_number });
            }
            if block_number > last_block_number {
                return Err(FromPartsError::AfterLastBlock {
                    block_number,
                    last_block_number,
                });
            }
            block_numbers.push(block_number);

            if let Some(state_diff) = state_diff {
                number_to_diff_index.insert(block_number, state_diffs.len());
                state_diffs.push((block_number, state_diff));
                storage.insert_receipts(block.transaction_receipts().to_vec())?;
            }

            storage.insert_block(block, total_difficulty)?;
        }

        let mut reservations: Vec<Reservation<HardforkT>> =
            Vec::with_capacity(reservation_parts.len());
        for reservation in reservation_parts {
            let ReservationParts {
                first_number,
                last_number,
                interval,
                previous_base_fee_per_gas,
                previous_state_root,
                previous_total_difficulty,
                previous_state_diff_block_number,
                hardfork,
            } = reservation;

            if last_number > last_block_number {
                return Err(FromPartsError::AfterLastBlock {
                    block_number: last_number,
                    last_block_number,
                });
            }

            let overlaps_block = block_numbers
                .iter()
                .any(|number| (first_number..=last_number).contains(number));
            let overlaps_reservation = reservations.iter().any(|other| {
                other.first_number <= last_number && first_number <= other.last_number
            });
            if first_number == 0
                || first_number > last_number
                || overlaps_block
                || overlaps_reservation
            {
                return Err(FromPartsError::InvalidReservation {
                    first_number,
                    last_number,
                });
            }

            let previous_diff_index = number_to_diff_index
                .get(&previous_state_diff_block_number)
                .filter(|_| previous_state_diff_block_number < first_number)
                .copied()
                .ok_or(FromPartsError::UnknownPreviousStateDiff {
                    first_number,
                    last_number,
                    block_number: previous_state_diff_block_number,
                })?;

            reservations.push(Reservation {
                first_number,
                last_number,
                interval,
                previous_base_fee_per_gas,
                previous_state_root,
                previous_total_difficulty,
                previous_diff_index,
                block_config: BlockConfig {
                    hardfork,
                    ..block_config.clone()
                },
            });
        }

        Ok(Self {
            reservations: RwLock::new(reservations),
            storage: RwLock::new(storage),
            state_diffs,
            number_to_diff_index,
            last_block_number,
        })
    }
}

impl<
        BlockReceiptT: ExecutionReceipt<Log = FilterLog> + ReceiptTrait,
        BlockT: BlockReceipts<BlockReceiptT>,
        HardforkT,
        SignedTransactionT,
    > ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Retrieves the logs that match the provided filter.
    pub fn logs(
        &self,
        from_block: u64,
        to_block: u64,
        addresses: &HashSet<Address>,
        normalized_topics: &[Option<Vec<B256>>],
    ) -> Vec<FilterLog> {
        let storage = self.storage.read();
        sparse::logs(&storage, from_block, to_block, addresses, normalized_topics)
    }
}

impl<
        BlockReceiptT: ExecutionReceipt<Log = FilterLog> + ReceiptTrait,
        BlockT: FetchBlockReceipts<BlockReceiptT>,
        HardforkT,
        SignedTransactionT,
    > ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Tries to fetch the logs that match the provided filter.
    pub fn try_fetch_logs(
        &self,
        from_block: u64,
        to_block: u64,
        addresses: &HashSet<Address>,
        normalized_topics: &[Option<Vec<B256>>],
    ) -> Result<Vec<FilterLog>, BlockT::Error> {
        let storage = self.storage.read();
        sparse::try_fetch_logs(&storage, from_block, to_block, addresses, normalized_topics)
    }
}

impl<BlockReceiptT: Clone + ReceiptTrait, BlockT: Clone, HardforkT: Clone, SignedTransactionT>
    ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Retrieves the block by hash, if it exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn block_by_hash(&self, hash: &B256) -> Option<BlockT> {
        self.storage.read().block_by_hash(hash).cloned()
    }

    /// Retrieves the block that contains the transaction with the provided
    /// hash, if it exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn block_by_transaction_hash(&self, transaction_hash: &B256) -> Option<BlockT> {
        self.storage
            .read()
            .block_by_transaction_hash(transaction_hash)
            .cloned()
    }

    /// Retrieves whether a block with the provided number exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn contains_block_number(&self, number: u64) -> bool {
        self.storage.read().contains_block_number(number)
    }

    /// Retrieves the last block number.
    pub fn last_block_number(&self) -> u64 {
        self.last_block_number
    }

    /// Retrieves the sequence of diffs from the genesis state to the state of
    /// the block with the provided number, if it exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn state_diffs_until_block(&self, block_number: u64) -> Option<&[(u64, StateDiff)]> {
        let diff_index = self
            .number_to_diff_index
            .get(&block_number)
            .copied()
            .or_else(|| {
                let reservations = self.reservations.read();
                find_reservation(&reservations, block_number)
                    .map(|reservation| reservation.previous_diff_index)
            })?;

        Some(
            self.state_diffs
                .get(0..=diff_index)
                .expect("diff_index is valid"),
        )
    }

    /// Retrieves the receipt of the transaction with the provided hash, if it
    /// exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn receipt_by_transaction_hash(&self, transaction_hash: &B256) -> Option<BlockReceiptT> {
        self.storage
            .read()
            .receipt_by_transaction_hash(transaction_hash)
            .cloned()
    }

    /// Reserves the provided number of blocks, starting from the next block
    /// number.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn reserve_blocks(
        &mut self,
        additional: NonZeroU64,
        interval: u64,
        previous_base_fee: Option<u128>,
        previous_state_root: B256,
        previous_total_difficulty: U256,
        block_config: &BlockConfig<HardforkT>,
    ) {
        let reservation = Reservation {
            first_number: self.last_block_number + 1,
            last_number: self.last_block_number + additional.get(),
            interval,
            previous_base_fee_per_gas: previous_base_fee,
            previous_state_root,
            previous_total_difficulty,
            previous_diff_index: self.state_diffs.len() - 1,
            block_config: block_config.clone(),
        };

        self.reservations.get_mut().push(reservation);
        self.last_block_number += additional.get();
    }

    /// Retrieves the total difficulty of the block with the provided hash.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn total_difficulty_by_hash(&self, hash: &B256) -> Option<U256> {
        self.storage.read().total_difficulty_by_hash(hash).cloned()
    }
}

impl<
        BlockReceiptT: Clone + ReceiptTrait,
        BlockT: Block<SignedTransactionT> + Clone + EmptyBlock<HardforkT> + LocalBlock<BlockReceiptT>,
        HardforkT: ProtocolHardfork,
        SignedTransactionT: ExecutableTransaction,
    > ReservableSparseBlockStorage<BlockReceiptT, BlockT, HardforkT, SignedTransactionT>
{
    /// Retrieves the block by number, if it exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn block_by_number(&self, number: u64) -> Result<Option<BlockT>, InsertBlockError> {
        Ok(self
            .try_fulfilling_reservation(number)?
            .or_else(|| self.storage.read().block_by_number(number).cloned()))
    }

    /// Insert a block into the storage. Errors if a block with the same hash or
    /// number already exists.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub fn insert_block_and_receipts(
        &mut self,
        block: BlockT,
        state_diff: StateDiff,
        total_difficulty: U256,
    ) -> Result<&BlockT, InsertBlockAndReceiptsError> {
        self.last_block_number = block.block_header().number;
        self.number_to_diff_index
            .insert(self.last_block_number, self.state_diffs.len());

        self.state_diffs.push((self.last_block_number, state_diff));

        let receipts: Vec<_> = block.transaction_receipts().to_vec();
        self.storage.get_mut().insert_receipts(receipts)?;

        self.storage
            .get_mut()
            .insert_block(block, total_difficulty)
            .map_err(InsertBlockAndReceiptsError::from)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn try_fulfilling_reservation(
        &self,
        block_number: u64,
    ) -> Result<Option<BlockT>, InsertBlockError> {
        let reservations = self.reservations.upgradable_read();

        reservations
            .iter()
            .enumerate()
            .find_map(|(idx, reservation)| {
                if reservation.first_number <= block_number
                    && block_number <= reservation.last_number
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .map(|idx| {
                let mut reservations = RwLockUpgradableReadGuard::upgrade(reservations);
                let reservation = reservations.remove(idx);

                if block_number != reservation.first_number {
                    reservations.push(Reservation {
                        last_number: block_number - 1,
                        ..reservation.clone()
                    });
                }

                if block_number != reservation.last_number {
                    reservations.push(Reservation {
                        first_number: block_number + 1,
                        ..reservation.clone()
                    });
                }

                let reservations = RwLockWriteGuard::downgrade(reservations);
                let storage = self.storage.upgradable_read();

                let timestamp = calculate_timestamp_for_reserved_block(
                    &storage,
                    &reservations,
                    &reservation,
                    block_number,
                );

                let slot_number = calculate_slot_number_for_reserved_block(
                    &storage,
                    &reservations,
                    &reservation,
                    block_number,
                );

                let block = BlockT::empty(
                    reservation.block_config.hardfork.clone(),
                    PartialHeader::new(
                        &reservation.block_config,
                        HeaderOverrides {
                            number: Some(block_number),
                            state_root: Some(reservation.previous_state_root),
                            base_fee: reservation.previous_base_fee_per_gas,
                            timestamp: Some(timestamp),
                            slot_number,
                            ..HeaderOverrides::default()
                        },
                        None,
                        &Vec::new(),
                        None,
                    ),
                );

                {
                    let mut storage = RwLockUpgradableReadGuard::upgrade(storage);
                    Ok(storage
                        .insert_block(block, reservation.previous_total_difficulty)?
                        .clone())
                }
            })
            .transpose()
    }
}

#[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
fn calculate_timestamp_for_reserved_block<
    BlockReceiptT: ReceiptTrait,
    BlockT: Block<SignedTransactionT>,
    HardforkT,
    SignedTransactionT,
>(
    storage: &SparseBlockStorage<BlockReceiptT, BlockT, SignedTransactionT>,
    reservations: &[Reservation<HardforkT>],
    reservation: &Reservation<HardforkT>,
    block_number: u64,
) -> u64 {
    let previous_block_number = reservation.first_number - 1;
    let previous_timestamp =
        if let Some(previous_reservation) = find_reservation(reservations, previous_block_number) {
            calculate_timestamp_for_reserved_block(
                storage,
                reservations,
                previous_reservation,
                previous_block_number,
            )
        } else {
            let previous_block = storage
                .block_by_number(previous_block_number)
                .expect("Block must exist");

            previous_block.block_header().timestamp
        };

    previous_timestamp + reservation.interval * (block_number - reservation.first_number + 1)
}

/// Calculates the slot number for a reserved block ([EIP-7843]).
///
/// EDR simulates one slot per block, so the slot number continues from the
/// block preceding the reservation instead of resetting, anchoring at 0 when
/// that block carries no slot number (a pre-Amsterdam parent).
///
/// [EIP-7843]: https://eips.ethereum.org/EIPS/eip-7843
#[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
fn calculate_slot_number_for_reserved_block<
    BlockReceiptT: ReceiptTrait,
    BlockT: Block<SignedTransactionT>,
    HardforkT: Clone + Into<EvmSpecId>,
    SignedTransactionT,
>(
    storage: &SparseBlockStorage<BlockReceiptT, BlockT, SignedTransactionT>,
    reservations: &[Reservation<HardforkT>],
    reservation: &Reservation<HardforkT>,
    block_number: u64,
) -> Option<u64> {
    // The slot number exists only from Amsterdam onwards.
    if reservation.block_config.hardfork.clone().into() < EvmSpecId::AMSTERDAM {
        return None;
    }

    let previous_block_number = reservation.first_number - 1;
    let previous_slot_number =
        if let Some(previous_reservation) = find_reservation(reservations, previous_block_number) {
            calculate_slot_number_for_reserved_block(
                storage,
                reservations,
                previous_reservation,
                previous_block_number,
            )
        } else {
            storage
                .block_by_number(previous_block_number)
                .expect("Block must exist")
                .block_header()
                .slot_number
        };

    // One slot per block, anchoring at 0 when the preceding block has no slot
    // number.
    Some(
        previous_slot_number.map_or(block_number - reservation.first_number, |slot_number| {
            slot_number + (block_number - previous_block_number)
        }),
    )
}

fn find_reservation<HardforkT>(
    reservations: &[Reservation<HardforkT>],
    number: u64,
) -> Option<&Reservation<HardforkT>> {
    reservations
        .iter()
        .find(|reservation| reservation.first_number <= number && number <= reservation.last_number)
}
