//! Serializable forms of the state changes a local blockchain keeps: the diff
//! of each block and the irregular state overrides.
//!
//! These types keep everything that [`StateCommit::commit`] reads, so a state
//! rebuilt from decoded diffs equals the state rebuilt from the originals.
//! They don't keep journal fields that only matter while a transaction runs:
//! transaction ids, warm/cold flags, and the account flags that are local to a
//! journal (`CreatedLocal`, `SelfDestructedLocal`, `Cold`).
//!
//! [`StateCommit::commit`]: edr_state_api::StateCommit::commit

use std::collections::BTreeMap;

use edr_primitives::{keccak256, Address, Bytecode, Bytes, B256, U256};
use edr_state_api::{
    account::{Account, AccountInfo, AccountStatus},
    irregular::IrregularState,
    EvmState, EvmStorageSlot, StateDiff, StateOverride, TransactionId,
};
use serde::{Deserialize, Serialize};

/// A storage slot in a [`DumpedAccountDiff`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpedStorageSlot {
    /// The value of the slot before the change.
    pub original_value: U256,
    /// The value of the slot after the change.
    pub present_value: U256,
}

/// The status flags of a [`DumpedAccountDiff`] that affect how it is
/// committed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpedAccountStatus {
    /// The account was created, so its previous storage is cleared.
    #[serde(default, skip_serializing_if = "is_false")]
    pub created: bool,
    /// The account was self-destructed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub self_destructed: bool,
    /// The account was touched, so the change is committed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub touched: bool,
    /// The account didn't exist when it was loaded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub loaded_as_not_existing: bool,
}

/// The change to one account in a [`DumpedStateDiff`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpedAccountDiff {
    /// The balance of the account.
    pub balance: U256,
    /// The nonce of the account.
    pub nonce: u64,
    /// The hash of the account's code.
    pub code_hash: B256,
    /// The code of the account, if the diff carries it. When it doesn't, the
    /// code is looked up by `code_hash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Bytes>,
    /// The status flags of the change.
    pub status: DumpedAccountStatus,
    /// The changed storage slots.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage: BTreeMap<U256, DumpedStorageSlot>,
}

/// A serializable [`StateDiff`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct DumpedStateDiff {
    /// The changed accounts, ordered by address.
    pub accounts: BTreeMap<Address, DumpedAccountDiff>,
}

/// A serializable [`StateOverride`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpedStateOverride {
    /// The diff to apply.
    pub diff: DumpedStateDiff,
    /// The state root after the diff is applied.
    pub state_root: B256,
}

/// A serializable [`IrregularState`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct DumpedIrregularState {
    /// The state overrides, by block number.
    pub overrides: BTreeMap<u64, DumpedStateOverride>,
}

/// An error that occurs when converting a dumped diff back to a [`StateDiff`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateDiffDecodeError {
    /// An account's code doesn't hash to its code hash.
    #[error("The code of account {address} doesn't match its code hash {code_hash}.")]
    CodeHashMismatch {
        /// The address of the account.
        address: Address,
        /// The code hash in the dump.
        code_hash: B256,
    },
}

impl From<&StateDiff> for DumpedStateDiff {
    fn from(diff: &StateDiff) -> Self {
        let accounts = diff
            .as_inner()
            .iter()
            .map(|(address, account)| (*address, DumpedAccountDiff::from(account)))
            .collect();

        Self { accounts }
    }
}

impl TryFrom<DumpedStateDiff> for StateDiff {
    type Error = StateDiffDecodeError;

    fn try_from(diff: DumpedStateDiff) -> Result<Self, Self::Error> {
        let state = diff
            .accounts
            .into_iter()
            .map(|(address, account)| Ok((address, account.into_account(address)?)))
            .collect::<Result<EvmState, StateDiffDecodeError>>()?;

        Ok(StateDiff::from(state))
    }
}

impl From<&StateOverride> for DumpedStateOverride {
    fn from(state_override: &StateOverride) -> Self {
        Self {
            diff: DumpedStateDiff::from(&state_override.diff),
            state_root: state_override.state_root,
        }
    }
}

impl TryFrom<DumpedStateOverride> for StateOverride {
    type Error = StateDiffDecodeError;

    fn try_from(state_override: DumpedStateOverride) -> Result<Self, Self::Error> {
        Ok(Self {
            diff: StateDiff::try_from(state_override.diff)?,
            state_root: state_override.state_root,
        })
    }
}

impl From<&IrregularState> for DumpedIrregularState {
    fn from(irregular_state: &IrregularState) -> Self {
        let overrides = irregular_state
            .state_overrides()
            .iter()
            .map(|(block_number, state_override)| {
                (*block_number, DumpedStateOverride::from(state_override))
            })
            .collect();

        Self { overrides }
    }
}

impl TryFrom<DumpedIrregularState> for IrregularState {
    type Error = StateDiffDecodeError;

    fn try_from(irregular_state: DumpedIrregularState) -> Result<Self, Self::Error> {
        let mut decoded = IrregularState::default();
        for (block_number, state_override) in irregular_state.overrides {
            decoded
                .state_override_at_block_number(block_number)
                .or_insert(StateOverride::try_from(state_override)?);
        }

        Ok(decoded)
    }
}

impl From<&Account> for DumpedAccountDiff {
    fn from(account: &Account) -> Self {
        let storage = account
            .storage
            .iter()
            .map(|(index, slot)| {
                (
                    *index,
                    DumpedStorageSlot {
                        original_value: slot.original_value,
                        present_value: slot.present_value,
                    },
                )
            })
            .collect();

        Self {
            balance: account.info.balance,
            nonce: account.info.nonce,
            code_hash: account.info.code_hash,
            code: account.info.code.as_ref().map(Bytecode::original_bytes),
            status: DumpedAccountStatus::from(account.status),
            storage,
        }
    }
}

impl DumpedAccountDiff {
    fn into_account(self, address: Address) -> Result<Account, StateDiffDecodeError> {
        let code = self
            .code
            .map(|code| {
                if keccak256(&code) == self.code_hash {
                    Ok(Bytecode::new_raw(code))
                } else {
                    Err(StateDiffDecodeError::CodeHashMismatch {
                        address,
                        code_hash: self.code_hash,
                    })
                }
            })
            .transpose()?;

        let mut account = Account::from(AccountInfo {
            balance: self.balance,
            nonce: self.nonce,
            code_hash: self.code_hash,
            account_id: None,
            code,
        });
        account.status = self.status.into();
        account.storage = self
            .storage
            .into_iter()
            .map(|(index, slot)| {
                (
                    index,
                    EvmStorageSlot::new_changed(
                        slot.original_value,
                        slot.present_value,
                        TransactionId::ZERO,
                    ),
                )
            })
            .collect();

        Ok(account)
    }
}

impl From<AccountStatus> for DumpedAccountStatus {
    fn from(status: AccountStatus) -> Self {
        Self {
            created: status.contains(AccountStatus::Created),
            self_destructed: status.contains(AccountStatus::SelfDestructed),
            touched: status.contains(AccountStatus::Touched),
            loaded_as_not_existing: status.contains(AccountStatus::LoadedAsNotExisting),
        }
    }
}

impl From<DumpedAccountStatus> for AccountStatus {
    fn from(status: DumpedAccountStatus) -> Self {
        let mut decoded = AccountStatus::empty();
        decoded.set(AccountStatus::Created, status.created);
        decoded.set(AccountStatus::SelfDestructed, status.self_destructed);
        decoded.set(AccountStatus::Touched, status.touched);
        decoded.set(
            AccountStatus::LoadedAsNotExisting,
            status.loaded_as_not_existing,
        );
        decoded
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use edr_primitives::{address, bytes, KECCAK_EMPTY};
    use edr_state_api::{State as _, StateCommit as _, StateDebug as _};
    use edr_state_persistent_trie::PersistentStateTrie;
    use proptest::{collection, prelude::*};

    use super::*;

    const ADDRESSES: [Address; 3] = [
        address!("0x0000000000000000000000000000000000000001"),
        address!("0x0000000000000000000000000000000000000002"),
        address!("0x0000000000000000000000000000000000000003"),
    ];

    /// Legacy code, an empty code, and an EIP-7702 delegation.
    fn codes() -> [Bytes; 3] {
        [
            bytes!("0x6080604052348015600e575f80fd5b50"),
            Bytes::new(),
            bytes!("0xef01000000000000000000000000000000000000000001"),
        ]
    }

    fn json_round_trip<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
        let json = serde_json::to_string(value).expect("serializing to JSON cannot fail");
        serde_json::from_str(&json).expect("the JSON was just written")
    }

    fn round_trip(diff: &StateDiff) -> StateDiff {
        StateDiff::try_from(json_round_trip(&DumpedStateDiff::from(diff)))
            .expect("the diff was just dumped")
    }

    /// An account whose code, if any, is `codes()[code_index]`.
    ///
    /// Each address gets its own code. `PersistentStateTrie` counts the
    /// accounts that use each code hash, but random diffs can remove code they
    /// never inserted. If two accounts in one diff shared a code hash, the code
    /// that's kept would then depend on the order of the diff's hash map,
    /// which differs between two maps with the same entries.
    fn account_strategy(code_index: usize) -> impl Strategy<Value = Account> {
        (
            any::<u64>(),
            any::<u64>(),
            // Whether the diff carries the code or only its hash.
            any::<bool>(),
            any::<[bool; 4]>(),
            collection::btree_map(0u64..4, (0u64..3, 0u64..3), 0..4),
        )
            .prop_map(move |(balance, nonce, carries_code, flags, storage)| {
                let code_bytes = codes()[code_index].clone();
                let code_hash = keccak256(&code_bytes);
                let code = carries_code.then(|| Bytecode::new_raw(code_bytes));

                let mut account = Account::from(AccountInfo {
                    balance: U256::from(balance),
                    nonce,
                    code_hash,
                    account_id: None,
                    code,
                });
                // Built from revm's flags, not through the conversion under test.
                account.status = [
                    AccountStatus::Created,
                    AccountStatus::SelfDestructed,
                    AccountStatus::Touched,
                    AccountStatus::LoadedAsNotExisting,
                ]
                .into_iter()
                .zip(flags)
                .filter(|(_, is_set)| *is_set)
                .fold(AccountStatus::empty(), |status, (flag, _)| status | flag);
                account.storage = storage
                    .into_iter()
                    .map(|(index, (original, present))| {
                        (
                            U256::from(index),
                            EvmStorageSlot::new_changed(
                                U256::from(original),
                                U256::from(present),
                                TransactionId::ZERO,
                            ),
                        )
                    })
                    .collect();
                account
            })
    }

    fn diff_strategy() -> impl Strategy<Value = StateDiff> {
        let accounts: Vec<_> = (0..ADDRESSES.len())
            .map(|index| proptest::option::of(account_strategy(index)))
            .collect();

        accounts.prop_map(|accounts| {
            StateDiff::from(
                accounts
                    .into_iter()
                    .zip(ADDRESSES)
                    .filter_map(|(account, address)| Some((address, account?)))
                    .collect::<EvmState>(),
            )
        })
    }

    fn commit_all<'diffs>(
        diffs: impl IntoIterator<Item = &'diffs StateDiff>,
    ) -> PersistentStateTrie {
        let mut state = PersistentStateTrie::default();
        for diff in diffs {
            state.commit(diff.as_inner().clone());
        }
        state
    }

    proptest! {
        #[test]
        fn decoded_diffs_commit_to_the_same_state(
            diffs in collection::vec(diff_strategy(), 1..5),
        ) {
            let decoded: Vec<StateDiff> = diffs.iter().map(round_trip).collect();

            let expected = commit_all(&diffs);
            let actual = commit_all(&decoded);

            prop_assert_eq!(actual.state_root()?, expected.state_root()?);
            prop_assert_eq!(actual.serialize(), expected.serialize());
            for code in codes() {
                let code_hash = keccak256(&code);
                prop_assert_eq!(
                    actual.code_by_hash(code_hash).ok(),
                    expected.code_by_hash(code_hash).ok()
                );
            }
        }

        #[test]
        fn dumping_a_decoded_diff_gives_the_same_dump(diff in diff_strategy()) {
            let dumped = DumpedStateDiff::from(&diff);

            prop_assert_eq!(DumpedStateDiff::from(&round_trip(&diff)), dumped);
        }

        #[test]
        fn decoding_keeps_each_account_and_its_status(diff in diff_strategy()) {
            let decoded = round_trip(&diff);

            prop_assert_eq!(decoded.as_inner().len(), diff.as_inner().len());
            for (address, account) in diff.as_inner() {
                let decoded_account = &decoded.as_inner()[address];
                prop_assert_eq!(decoded_account.status, account.status);
                prop_assert_eq!(&decoded_account.info, &account.info);
                prop_assert_eq!(&decoded_account.storage, &account.storage);
            }
        }
    }

    #[test]
    fn decoding_rejects_code_that_does_not_match_its_hash() {
        let address = ADDRESSES[0];
        let dumped = DumpedStateDiff {
            accounts: BTreeMap::from([(
                address,
                DumpedAccountDiff {
                    code_hash: KECCAK_EMPTY,
                    code: Some(codes()[0].clone()),
                    ..DumpedAccountDiff::default()
                },
            )]),
        };

        assert_eq!(
            StateDiff::try_from(dumped).unwrap_err(),
            StateDiffDecodeError::CodeHashMismatch {
                address,
                code_hash: KECCAK_EMPTY,
            }
        );
    }

    #[test]
    fn irregular_state_round_trips() -> anyhow::Result<()> {
        let mut diff = StateDiff::default();
        diff.apply_storage_change(
            ADDRESSES[0],
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(7), TransactionId::ZERO),
            None,
        );

        let mut irregular_state = IrregularState::default();
        for (block_number, state_root) in [(3, B256::repeat_byte(3)), (8, B256::repeat_byte(8))] {
            irregular_state
                .state_override_at_block_number(block_number)
                .or_insert(StateOverride {
                    diff: diff.clone(),
                    state_root,
                });
        }

        let dumped = DumpedIrregularState::from(&irregular_state);
        let decoded = IrregularState::try_from(json_round_trip(&dumped))?;

        assert_eq!(DumpedIrregularState::from(&decoded), dumped);
        assert_eq!(
            decoded
                .state_overrides()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [3, 8]
        );
        Ok(())
    }

    #[test]
    fn encodes_a_readable_json_shape() {
        let mut diff = StateDiff::default();
        diff.apply_storage_change(
            ADDRESSES[0],
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(7), TransactionId::ZERO),
            None,
        );

        let json = serde_json::to_value(DumpedStateDiff::from(&diff)).expect("serializable");

        assert_eq!(
            json,
            serde_json::json!({
                "0x0000000000000000000000000000000000000001": {
                    "balance": "0x0",
                    "nonce": 0,
                    "codeHash": KECCAK_EMPTY,
                    "code": "0x",
                    "status": { "created": true, "touched": true },
                    "storage": {
                        "0x1": { "originalValue": "0x0", "presentValue": "0x7" }
                    }
                }
            })
        );
    }
}
