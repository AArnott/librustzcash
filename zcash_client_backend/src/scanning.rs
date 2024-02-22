//! Tools for scanning a compact representation of the Zcash block chain.

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::fmt::{self, Debug};
use std::hash::Hash;

use incrementalmerkletree::{Position, Retention};
use sapling::{
    note_encryption::{CompactOutputDescription, PreparedIncomingViewingKey, SaplingDomain},
    zip32::DiversifiableFullViewingKey,
    SaplingIvk,
};
use subtle::{ConditionallySelectable, ConstantTimeEq, CtOption};
use zcash_note_encryption::batch;
use zcash_primitives::consensus::{self, BlockHeight, NetworkUpgrade};
use zip32::Scope;

use crate::data_api::{BlockMetadata, ScannedBlock, ScannedBundles};
use crate::{
    proto::compact_formats::CompactBlock,
    scan::{Batch, BatchRunner, CompactDecryptor, Tasks},
    wallet::{WalletSaplingOutput, WalletSaplingSpend, WalletTx},
    ShieldedProtocol,
};

/// A key that can be used to perform trial decryption and nullifier
/// computation for a Sapling [`CompactSaplingOutput`]
///
/// The purpose of this trait is to enable [`scan_block`]
/// and related methods to be used with either incoming viewing keys
/// or full viewing keys, with the data returned from trial decryption
/// being dependent upon the type of key used. In the case that an
/// incoming viewing key is used, only the note and payment address
/// will be returned; in the case of a full viewing key, the
/// nullifier for the note can also be obtained.
///
/// [`CompactSaplingOutput`]: crate::proto::compact_formats::CompactSaplingOutput
/// [`scan_block`]: crate::scanning::scan_block
pub trait ScanningKey {
    /// The type representing the scope of the scanning key.
    type Scope: Clone + Eq + std::hash::Hash + Send + 'static;

    /// The type of key that is used to decrypt outputs belonging to the wallet.
    type IncomingViewingKey: Clone;

    /// The type of key that is used to derive nullifiers.
    type NullifierDerivingKey: Clone;

    /// The type of nullifier extracted when a note is successfully obtained by trial decryption.
    type Nf;

    /// The type of notes obtained by trial decryption.
    type Note;

    /// Obtain the underlying incoming viewing key(s) for this scanning key.
    fn to_ivks(
        &self,
    ) -> Vec<(
        Self::Scope,
        Self::IncomingViewingKey,
        Self::NullifierDerivingKey,
    )>;

    /// Produces the nullifier for the specified note and witness, if possible.
    ///
    /// IVK-based implementations of this trait cannot successfully derive
    /// nullifiers, in which case `Self::Nf` should be set to the unit type
    /// and this function is a no-op.
    fn nf(key: &Self::NullifierDerivingKey, note: &Self::Note, note_position: Position)
        -> Self::Nf;
}

impl<K: ScanningKey> ScanningKey for &K {
    type Scope = K::Scope;
    type IncomingViewingKey = K::IncomingViewingKey;
    type NullifierDerivingKey = K::NullifierDerivingKey;
    type Nf = K::Nf;
    type Note = K::Note;

    fn to_ivks(
        &self,
    ) -> Vec<(
        Self::Scope,
        Self::IncomingViewingKey,
        Self::NullifierDerivingKey,
    )> {
        (*self).to_ivks()
    }

    fn nf(key: &Self::NullifierDerivingKey, note: &Self::Note, position: Position) -> Self::Nf {
        K::nf(key, note, position)
    }
}

impl ScanningKey for DiversifiableFullViewingKey {
    type Scope = Scope;
    type IncomingViewingKey = SaplingIvk;
    type NullifierDerivingKey = sapling::NullifierDerivingKey;
    type Nf = sapling::Nullifier;
    type Note = sapling::Note;

    fn to_ivks(
        &self,
    ) -> Vec<(
        Self::Scope,
        Self::IncomingViewingKey,
        Self::NullifierDerivingKey,
    )> {
        vec![
            (
                Scope::External,
                self.to_ivk(Scope::External),
                self.to_nk(Scope::External),
            ),
            (
                Scope::Internal,
                self.to_ivk(Scope::Internal),
                self.to_nk(Scope::Internal),
            ),
        ]
    }

    fn nf(key: &Self::NullifierDerivingKey, note: &Self::Note, position: Position) -> Self::Nf {
        note.nf(key, position.into())
    }
}

impl ScanningKey for (Scope, SaplingIvk, sapling::NullifierDerivingKey) {
    type Scope = Scope;
    type IncomingViewingKey = SaplingIvk;
    type NullifierDerivingKey = sapling::NullifierDerivingKey;
    type Nf = sapling::Nullifier;
    type Note = sapling::Note;

    fn to_ivks(
        &self,
    ) -> Vec<(
        Self::Scope,
        Self::IncomingViewingKey,
        Self::NullifierDerivingKey,
    )> {
        vec![self.clone()]
    }

    fn nf(key: &Self::NullifierDerivingKey, note: &Self::Note, position: Position) -> Self::Nf {
        note.nf(key, position.into())
    }
}

/// The [`ScanningKey`] implementation for [`SaplingIvk`]s.
/// Nullifiers cannot be derived when scanning with these keys.
///
/// [`SaplingIvk`]: sapling::SaplingIvk
impl ScanningKey for SaplingIvk {
    type Scope = ();
    type IncomingViewingKey = SaplingIvk;
    type NullifierDerivingKey = ();
    type Nf = ();
    type Note = sapling::Note;

    fn to_ivks(
        &self,
    ) -> Vec<(
        Self::Scope,
        Self::IncomingViewingKey,
        Self::NullifierDerivingKey,
    )> {
        vec![((), self.clone(), ())]
    }

    fn nf(_key: &Self::NullifierDerivingKey, _note: &Self::Note, _position: Position) -> Self::Nf {}
}

/// Errors that may occur in chain scanning
#[derive(Clone, Debug)]
pub enum ScanError {
    /// The hash of the parent block given by a proposed new chain tip does not match the hash of
    /// the current chain tip.
    PrevHashMismatch { at_height: BlockHeight },

    /// The block height field of the proposed new block is not equal to the height of the previous
    /// block + 1.
    BlockHeightDiscontinuity {
        prev_height: BlockHeight,
        new_height: BlockHeight,
    },

    /// The note commitment tree size for the given protocol at the proposed new block is not equal
    /// to the size at the previous block plus the count of this block's outputs.
    TreeSizeMismatch {
        protocol: ShieldedProtocol,
        at_height: BlockHeight,
        given: u32,
        computed: u32,
    },

    /// The size of the note commitment tree for the given protocol was not provided as part of a
    /// [`CompactBlock`] being scanned, making it impossible to construct the nullifier for a
    /// detected note.
    TreeSizeUnknown {
        protocol: ShieldedProtocol,
        at_height: BlockHeight,
    },

    /// We were provided chain metadata for a block containing note commitment tree metadata
    /// that is invalidated by the data in the block itself. This may be caused by the presence
    /// of default values in the chain metadata.
    TreeSizeInvalid {
        protocol: ShieldedProtocol,
        at_height: BlockHeight,
    },
}

impl ScanError {
    /// Returns whether this error is the result of a failed continuity check
    pub fn is_continuity_error(&self) -> bool {
        use ScanError::*;
        match self {
            PrevHashMismatch { .. } => true,
            BlockHeightDiscontinuity { .. } => true,
            TreeSizeMismatch { .. } => true,
            TreeSizeUnknown { .. } => false,
            TreeSizeInvalid { .. } => false,
        }
    }

    /// Returns the block height at which the scan error occurred
    pub fn at_height(&self) -> BlockHeight {
        use ScanError::*;
        match self {
            PrevHashMismatch { at_height } => *at_height,
            BlockHeightDiscontinuity { new_height, .. } => *new_height,
            TreeSizeMismatch { at_height, .. } => *at_height,
            TreeSizeUnknown { at_height, .. } => *at_height,
            TreeSizeInvalid { at_height, .. } => *at_height,
        }
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use ScanError::*;
        match &self {
            PrevHashMismatch { at_height } => write!(
                f,
                "The parent hash of proposed block does not correspond to the block hash at height {}.",
                at_height
            ),
            BlockHeightDiscontinuity { prev_height, new_height } => {
                write!(f, "Block height discontinuity at height {}; previous height was: {}", new_height, prev_height)
            }
            TreeSizeMismatch { protocol, at_height, given, computed } => {
                write!(f, "The {:?} note commitment tree size provided by a compact block did not match the expected size at height {}; given {}, expected {}", protocol, at_height, given, computed)
            }
            TreeSizeUnknown { protocol, at_height } => {
                write!(f, "Unable to determine {:?} note commitment tree size at height {}", protocol, at_height)
            }
            TreeSizeInvalid { protocol, at_height } => {
                write!(f, "Received invalid (potentially default) {:?} note commitment tree size metadata at height {}", protocol, at_height)
            }
        }
    }
}

/// Scans a [`CompactBlock`] with a set of [`ScanningKey`]s.
///
/// Returns a vector of [`WalletTx`]s belonging to any of the given
/// [`ScanningKey`]s. If scanning with a full viewing key, the nullifiers
/// of the resulting [`WalletSaplingOutput`]s will also be computed.
///
/// The given [`CommitmentTree`] and existing [`IncrementalWitness`]es are
/// incremented appropriately.
///
/// The implementation of [`ScanningKey`] may either support or omit the computation of
/// the nullifiers for received notes; the implementation for [`ExtendedFullViewingKey`]
/// will derive the nullifiers for received notes and return them as part of the resulting
/// [`WalletSaplingOutput`]s, whereas the implementation for [`SaplingIvk`] cannot
/// do so and will return the unit value in those outputs instead.
///
/// [`ExtendedFullViewingKey`]: sapling::zip32::ExtendedFullViewingKey
/// [`SaplingIvk`]: sapling::SaplingIvk
/// [`CompactBlock`]: crate::proto::compact_formats::CompactBlock
/// [`ScanningKey`]: crate::scanning::ScanningKey
/// [`CommitmentTree`]: sapling::CommitmentTree
/// [`IncrementalWitness`]: sapling::IncrementalWitness
/// [`WalletSaplingOutput`]: crate::wallet::WalletSaplingOutput
/// [`WalletTx`]: crate::wallet::WalletTx
pub fn scan_block<P, A, SK>(
    params: &P,
    block: CompactBlock,
    sapling_keys: &[(&A, &SK)],
    sapling_nullifiers: &[(A, sapling::Nullifier)],
    prior_block_metadata: Option<&BlockMetadata>,
) -> Result<ScannedBlock<SK::Nf, SK::Scope, A>, ScanError>
where
    P: consensus::Parameters + Send + 'static,
    A: Default + Eq + Hash + Send + ConditionallySelectable + 'static,
    SK: ScanningKey<IncomingViewingKey = SaplingIvk, Note = sapling::Note>,
{
    scan_block_with_runner::<_, A, _, ()>(
        params,
        block,
        sapling_keys,
        sapling_nullifiers,
        prior_block_metadata,
        None,
    )
}

type TaggedBatch<A, S> = Batch<(A, S), SaplingDomain, CompactOutputDescription, CompactDecryptor>;
type TaggedBatchRunner<A, S, T> =
    BatchRunner<(A, S), SaplingDomain, CompactOutputDescription, CompactDecryptor, T>;

#[tracing::instrument(skip_all, fields(height = block.height))]
pub(crate) fn add_block_to_runner<P, S, T, A>(
    params: &P,
    block: CompactBlock,
    batch_runner: &mut TaggedBatchRunner<A, S, T>,
) where
    P: consensus::Parameters + Send + 'static,
    S: Clone + Send + 'static,
    T: Tasks<TaggedBatch<A, S>>,
    A: Copy + Default + Eq + Send + 'static,
{
    let block_hash = block.hash();
    let block_height = block.height();
    let zip212_enforcement = consensus::sapling_zip212_enforcement(params, block_height);

    for tx in block.vtx.into_iter() {
        let txid = tx.txid();
        let outputs = tx
            .outputs
            .into_iter()
            .map(|output| {
                CompactOutputDescription::try_from(output)
                    .expect("Invalid output found in compact block decoding.")
            })
            .collect::<Vec<_>>();

        batch_runner.add_outputs(
            block_hash,
            txid,
            || SaplingDomain::new(zip212_enforcement),
            &outputs,
        )
    }
}

fn check_hash_continuity(
    block: &CompactBlock,
    prior_block_metadata: Option<&BlockMetadata>,
) -> Option<ScanError> {
    if let Some(prev) = prior_block_metadata {
        if block.height() != prev.block_height() + 1 {
            return Some(ScanError::BlockHeightDiscontinuity {
                prev_height: prev.block_height(),
                new_height: block.height(),
            });
        }

        if block.prev_hash() != prev.block_hash() {
            return Some(ScanError::PrevHashMismatch {
                at_height: block.height(),
            });
        }
    }

    None
}

#[tracing::instrument(skip_all, fields(height = block.height))]
pub(crate) fn scan_block_with_runner<P, A, SK, T>(
    params: &P,
    block: CompactBlock,
    sapling_keys: &[(&A, SK)],
    sapling_nullifiers: &[(A, sapling::Nullifier)],
    prior_block_metadata: Option<&BlockMetadata>,
    mut sapling_batch_runner: Option<&mut TaggedBatchRunner<A, SK::Scope, T>>,
) -> Result<ScannedBlock<SK::Nf, SK::Scope, A>, ScanError>
where
    P: consensus::Parameters + Send + 'static,
    SK: ScanningKey<IncomingViewingKey = SaplingIvk, Note = sapling::Note>,
    T: Tasks<TaggedBatch<A, SK::Scope>> + Sync,
    A: Default + Eq + Hash + ConditionallySelectable + Send + 'static,
{
    if let Some(scan_error) = check_hash_continuity(&block, prior_block_metadata) {
        return Err(scan_error);
    }

    let cur_height = block.height();
    let cur_hash = block.hash();
    let zip212_enforcement = consensus::sapling_zip212_enforcement(params, cur_height);

    let mut sapling_commitment_tree_size = prior_block_metadata
        .and_then(|m| m.sapling_tree_size())
        .map_or_else(
            || {
                block.chain_metadata.as_ref().map_or_else(
                    || {
                        // If we're below Sapling activation, or Sapling activation is not set, the tree size is zero
                        params
                            .activation_height(NetworkUpgrade::Sapling)
                            .map_or_else(
                                || Ok(0),
                                |sapling_activation| {
                                    if cur_height < sapling_activation {
                                        Ok(0)
                                    } else {
                                        Err(ScanError::TreeSizeUnknown {
                                            protocol: ShieldedProtocol::Sapling,
                                            at_height: cur_height,
                                        })
                                    }
                                },
                            )
                    },
                    |m| {
                        let sapling_output_count: u32 = block
                            .vtx
                            .iter()
                            .map(|tx| tx.outputs.len())
                            .sum::<usize>()
                            .try_into()
                            .expect("Sapling output count cannot exceed a u32");

                        // The default for m.sapling_commitment_tree_size is zero, so we need to check
                        // that the subtraction will not underflow; if it would do so, we were given
                        // invalid chain metadata for a block with Sapling outputs.
                        m.sapling_commitment_tree_size
                            .checked_sub(sapling_output_count)
                            .ok_or(ScanError::TreeSizeInvalid {
                                protocol: ShieldedProtocol::Sapling,
                                at_height: cur_height,
                            })
                    },
                )
            },
            Ok,
        )?;

    #[cfg(feature = "orchard")]
    let mut orchard_commitment_tree_size = prior_block_metadata
        .and_then(|m| m.orchard_tree_size())
        .map_or_else(
            || {
                block.chain_metadata.as_ref().map_or_else(
                    || {
                        // If we're below Orchard activation, or Orchard activation is not set, the tree size is zero
                        params.activation_height(NetworkUpgrade::Nu5).map_or_else(
                            || Ok(0),
                            |orchard_activation| {
                                if cur_height < orchard_activation {
                                    Ok(0)
                                } else {
                                    Err(ScanError::TreeSizeUnknown {
                                        protocol: ShieldedProtocol::Orchard,
                                        at_height: cur_height,
                                    })
                                }
                            },
                        )
                    },
                    |m| {
                        let orchard_action_count: u32 = block
                            .vtx
                            .iter()
                            .map(|tx| tx.actions.len())
                            .sum::<usize>()
                            .try_into()
                            .expect("Orchard action count cannot exceed a u32");

                        // The default for m.orchard_commitment_tree_size is zero, so we need to check
                        // that the subtraction will not underflow; if it would do so, we were given
                        // invalid chain metadata for a block with Orchard actions.
                        m.orchard_commitment_tree_size
                            .checked_sub(orchard_action_count)
                            .ok_or(ScanError::TreeSizeInvalid {
                                protocol: ShieldedProtocol::Orchard,
                                at_height: cur_height,
                            })
                    },
                )
            },
            Ok,
        )?;

    let compact_block_tx_count = block.vtx.len();
    let mut wtxs: Vec<WalletTx<SK::Nf, SK::Scope, A>> = vec![];
    let mut sapling_nullifier_map = Vec::with_capacity(block.vtx.len());
    let mut sapling_note_commitments: Vec<(sapling::Node, Retention<BlockHeight>)> = vec![];
    for (tx_idx, tx) in block.vtx.into_iter().enumerate() {
        let txid = tx.txid();
        let tx_index =
            u16::try_from(tx.index).expect("Cannot fit more than 2^16 transactions in a block");

        let (sapling_spends, sapling_unlinked_nullifiers) = check_nullifiers(
            &tx.spends,
            sapling_nullifiers,
            |spend| {
                spend.nf().expect(
                    "Could not deserialize nullifier for spend from protobuf representation.",
                )
            },
            WalletSaplingSpend::from_parts,
        );

        sapling_nullifier_map.push((txid, tx_index, sapling_unlinked_nullifiers));

        // Collect the set of accounts that were spent from in this transaction
        let spent_from_accounts: HashSet<_> =
            sapling_spends.iter().map(|spend| spend.account()).collect();

        // We keep track of the number of outputs and actions here because tx.outputs
        // and tx.actions end up being moved.
        let tx_outputs_len =
            u32::try_from(tx.outputs.len()).expect("Sapling output count cannot exceed a u32");
        #[cfg(feature = "orchard")]
        let tx_actions_len =
            u32::try_from(tx.actions.len()).expect("Orchard action count cannot exceed a u32");

        // Check for incoming notes while incrementing tree and witnesses
        let mut shielded_outputs: Vec<WalletSaplingOutput<SK::Nf, SK::Scope, A>> = vec![];
        {
            let decoded = &tx
                .outputs
                .into_iter()
                .map(|output| {
                    (
                        SaplingDomain::new(zip212_enforcement),
                        CompactOutputDescription::try_from(output)
                            .expect("Invalid output found in compact block decoding."),
                    )
                })
                .collect::<Vec<_>>();

            let decrypted: Vec<_> = if let Some(runner) = sapling_batch_runner.as_mut() {
                let sapling_keys = sapling_keys
                    .iter()
                    .flat_map(|(a, k)| {
                        k.to_ivks()
                            .into_iter()
                            .map(move |(scope, _, nk)| ((**a, scope), nk))
                    })
                    .collect::<HashMap<_, _>>();

                let mut decrypted = runner.collect_results(cur_hash, txid);
                (0..decoded.len())
                    .map(|i| {
                        decrypted.remove(&(txid, i)).map(|d_out| {
                            let a = d_out.ivk_tag.0;
                            let nk = sapling_keys.get(&d_out.ivk_tag).expect(
                                "The batch runner and scan_block must use the same set of IVKs.",
                            );

                            (d_out.note, a, d_out.ivk_tag.1, (*nk).clone())
                        })
                    })
                    .collect()
            } else {
                let sapling_keys = sapling_keys
                    .iter()
                    .flat_map(|(a, k)| {
                        k.to_ivks()
                            .into_iter()
                            .map(move |(scope, ivk, nk)| (**a, scope, ivk, nk))
                    })
                    .collect::<Vec<_>>();

                let ivks = sapling_keys
                    .iter()
                    .map(|(_, _, ivk, _)| PreparedIncomingViewingKey::new(ivk))
                    .collect::<Vec<_>>();

                batch::try_compact_note_decryption(&ivks, &decoded[..])
                    .into_iter()
                    .map(|v| {
                        v.map(|((note, _), ivk_idx)| {
                            let (account, scope, _, nk) = &sapling_keys[ivk_idx];
                            (note, *account, scope.clone(), (*nk).clone())
                        })
                    })
                    .collect()
            };

            for (output_idx, ((_, output), dec_output)) in decoded.iter().zip(decrypted).enumerate()
            {
                // Collect block note commitments
                let node = sapling::Node::from_cmu(&output.cmu);
                let is_checkpoint =
                    output_idx + 1 == decoded.len() && tx_idx + 1 == compact_block_tx_count;
                let retention = match (dec_output.is_some(), is_checkpoint) {
                    (is_marked, true) => Retention::Checkpoint {
                        id: cur_height,
                        is_marked,
                    },
                    (true, false) => Retention::Marked,
                    (false, false) => Retention::Ephemeral,
                };

                if let Some((note, account, scope, nk)) = dec_output {
                    // A note is marked as "change" if the account that received it
                    // also spent notes in the same transaction. This will catch,
                    // for instance:
                    // - Change created by spending fractions of notes.
                    // - Notes created by consolidation transactions.
                    // - Notes sent from one account to itself.
                    let is_change = spent_from_accounts.contains(&account);
                    let note_commitment_tree_position = Position::from(u64::from(
                        sapling_commitment_tree_size + u32::try_from(output_idx).unwrap(),
                    ));
                    let nf = SK::nf(&nk, &note, note_commitment_tree_position);

                    shielded_outputs.push(WalletSaplingOutput::from_parts(
                        output_idx,
                        output.cmu,
                        output.ephemeral_key.clone(),
                        account,
                        note,
                        is_change,
                        note_commitment_tree_position,
                        nf,
                        scope,
                    ));
                }

                sapling_note_commitments.push((node, retention));
            }
        }

        if !(sapling_spends.is_empty() && shielded_outputs.is_empty()) {
            wtxs.push(WalletTx {
                txid,
                index: tx_index as usize,
                sapling_spends,
                sapling_outputs: shielded_outputs,
            });
        }

        sapling_commitment_tree_size += tx_outputs_len;
        #[cfg(feature = "orchard")]
        {
            orchard_commitment_tree_size += tx_actions_len;
        }
    }

    if let Some(chain_meta) = block.chain_metadata {
        if chain_meta.sapling_commitment_tree_size != sapling_commitment_tree_size {
            return Err(ScanError::TreeSizeMismatch {
                protocol: ShieldedProtocol::Sapling,
                at_height: cur_height,
                given: chain_meta.sapling_commitment_tree_size,
                computed: sapling_commitment_tree_size,
            });
        }

        #[cfg(feature = "orchard")]
        if chain_meta.orchard_commitment_tree_size != orchard_commitment_tree_size {
            return Err(ScanError::TreeSizeMismatch {
                protocol: ShieldedProtocol::Orchard,
                at_height: cur_height,
                given: chain_meta.orchard_commitment_tree_size,
                computed: orchard_commitment_tree_size,
            });
        }
    }

    Ok(ScannedBlock::from_parts(
        cur_height,
        cur_hash,
        block.time,
        wtxs,
        ScannedBundles::new(
            sapling_commitment_tree_size,
            sapling_note_commitments,
            sapling_nullifier_map,
        ),
        #[cfg(feature = "orchard")]
        ScannedBundles::new(
            orchard_commitment_tree_size,
            vec![], // FIXME: collect the Orchard nullifiers
            vec![], // FIXME: collect the Orchard note commitments
        ),
    ))
}

// Check for spent notes. The comparison against known-unspent nullifiers is done
// in constant time.
fn check_nullifiers<A: ConditionallySelectable + Default, Spend, Nf: ConstantTimeEq + Copy, WS>(
    spends: &[Spend],
    nullifiers: &[(A, Nf)],
    extract_nf: impl Fn(&Spend) -> Nf,
    construct_wallet_spend: impl Fn(usize, Nf, A) -> WS,
) -> (Vec<WS>, Vec<Nf>) {
    // TODO: this is O(|nullifiers| * |notes|); does using constant-time operations here really
    // make sense?
    let mut found_spent = vec![];
    let mut unlinked_nullifiers = Vec::with_capacity(spends.len());
    for (index, spend) in spends.iter().enumerate() {
        let spend_nf = extract_nf(spend);

        // Find the first tracked nullifier that matches this spend, and produce
        // a WalletShieldedSpend if there is a match, in constant time.
        let spend = nullifiers
            .iter()
            .map(|&(account, nf)| CtOption::new(account, nf.ct_eq(&spend_nf)))
            .fold(CtOption::new(A::default(), 0.into()), |first, next| {
                CtOption::conditional_select(&next, &first, first.is_some())
            })
            .map(|account| construct_wallet_spend(index, spend_nf, account));

        if let Some(spend) = spend.into() {
            found_spent.push(spend);
        } else {
            // This nullifier didn't match any we are currently tracking; save it in
            // case it matches an earlier block range we haven't scanned yet.
            unlinked_nullifiers.push(spend_nf);
        }
    }
    (found_spent, unlinked_nullifiers)
}

#[cfg(test)]
mod tests {
    use group::{
        ff::{Field, PrimeField},
        GroupEncoding,
    };
    use incrementalmerkletree::{Position, Retention};
    use rand_core::{OsRng, RngCore};
    use sapling::{
        constants::SPENDING_KEY_GENERATOR,
        note_encryption::{sapling_note_encryption, PreparedIncomingViewingKey, SaplingDomain},
        util::generate_random_rseed,
        value::NoteValue,
        zip32::{DiversifiableFullViewingKey, ExtendedSpendingKey},
        Nullifier, SaplingIvk,
    };
    use zcash_note_encryption::Domain;
    use zcash_primitives::{
        block::BlockHash,
        consensus::{sapling_zip212_enforcement, BlockHeight, Network},
        memo::MemoBytes,
        transaction::components::amount::NonNegativeAmount,
        zip32::AccountId,
    };

    use crate::{
        data_api::BlockMetadata,
        proto::compact_formats::{
            self as compact, CompactBlock, CompactSaplingOutput, CompactSaplingSpend, CompactTx,
        },
        scan::BatchRunner,
    };

    use super::{add_block_to_runner, scan_block, scan_block_with_runner, ScanningKey};

    fn random_compact_tx(mut rng: impl RngCore) -> CompactTx {
        let fake_nf = {
            let mut nf = vec![0; 32];
            rng.fill_bytes(&mut nf);
            nf
        };
        let fake_cmu = {
            let fake_cmu = bls12_381::Scalar::random(&mut rng);
            fake_cmu.to_repr().as_ref().to_owned()
        };
        let fake_epk = {
            let mut buffer = [0; 64];
            rng.fill_bytes(&mut buffer);
            let fake_esk = jubjub::Fr::from_bytes_wide(&buffer);
            let fake_epk = SPENDING_KEY_GENERATOR * fake_esk;
            fake_epk.to_bytes().to_vec()
        };
        let cspend = CompactSaplingSpend { nf: fake_nf };
        let cout = CompactSaplingOutput {
            cmu: fake_cmu,
            ephemeral_key: fake_epk,
            ciphertext: vec![0; 52],
        };
        let mut ctx = CompactTx::default();
        let mut txid = vec![0; 32];
        rng.fill_bytes(&mut txid);
        ctx.hash = txid;
        ctx.spends.push(cspend);
        ctx.outputs.push(cout);
        ctx
    }

    /// Create a fake CompactBlock at the given height, with a transaction containing a
    /// single spend of the given nullifier and a single output paying the given address.
    /// Returns the CompactBlock.
    ///
    /// Set `initial_tree_sizes` to `None` to simulate a `CompactBlock` retrieved
    /// from a `lightwalletd` that is not currently tracking note commitment tree sizes.
    fn fake_compact_block(
        height: BlockHeight,
        prev_hash: BlockHash,
        nf: Nullifier,
        dfvk: &DiversifiableFullViewingKey,
        value: NonNegativeAmount,
        tx_after: bool,
        initial_tree_sizes: Option<(u32, u32)>,
    ) -> CompactBlock {
        let zip212_enforcement = sapling_zip212_enforcement(&Network::TestNetwork, height);
        let to = dfvk.default_address().1;

        // Create a fake Note for the account
        let mut rng = OsRng;
        let rseed = generate_random_rseed(zip212_enforcement, &mut rng);
        let note = sapling::Note::from_parts(to, NoteValue::from(value), rseed);
        let encryptor = sapling_note_encryption(
            Some(dfvk.fvk().ovk),
            note.clone(),
            *MemoBytes::empty().as_array(),
            &mut rng,
        );
        let cmu = note.cmu().to_bytes().to_vec();
        let ephemeral_key = SaplingDomain::epk_bytes(encryptor.epk()).0.to_vec();
        let enc_ciphertext = encryptor.encrypt_note_plaintext();

        // Create a fake CompactBlock containing the note
        let mut cb = CompactBlock {
            hash: {
                let mut hash = vec![0; 32];
                rng.fill_bytes(&mut hash);
                hash
            },
            prev_hash: prev_hash.0.to_vec(),
            height: height.into(),
            ..Default::default()
        };

        // Add a random Sapling tx before ours
        {
            let mut tx = random_compact_tx(&mut rng);
            tx.index = cb.vtx.len() as u64;
            cb.vtx.push(tx);
        }

        let cspend = CompactSaplingSpend { nf: nf.0.to_vec() };
        let cout = CompactSaplingOutput {
            cmu,
            ephemeral_key,
            ciphertext: enc_ciphertext.as_ref()[..52].to_vec(),
        };
        let mut ctx = CompactTx::default();
        let mut txid = vec![0; 32];
        rng.fill_bytes(&mut txid);
        ctx.hash = txid;
        ctx.spends.push(cspend);
        ctx.outputs.push(cout);
        ctx.index = cb.vtx.len() as u64;
        cb.vtx.push(ctx);

        // Optionally add another random Sapling tx after ours
        if tx_after {
            let mut tx = random_compact_tx(&mut rng);
            tx.index = cb.vtx.len() as u64;
            cb.vtx.push(tx);
        }

        cb.chain_metadata =
            initial_tree_sizes.map(|(initial_sapling_tree_size, initial_orchard_tree_size)| {
                compact::ChainMetadata {
                    sapling_commitment_tree_size: initial_sapling_tree_size
                        + cb.vtx.iter().map(|tx| tx.outputs.len() as u32).sum::<u32>(),
                    orchard_commitment_tree_size: initial_orchard_tree_size
                        + cb.vtx.iter().map(|tx| tx.actions.len() as u32).sum::<u32>(),
                }
            });

        cb
    }

    #[test]
    fn scan_block_with_my_tx() {
        fn go(scan_multithreaded: bool) {
            let account = AccountId::ZERO;
            let extsk = ExtendedSpendingKey::master(&[]);
            let dfvk = extsk.to_diversifiable_full_viewing_key();

            let cb = fake_compact_block(
                1u32.into(),
                BlockHash([0; 32]),
                Nullifier([0; 32]),
                &dfvk,
                NonNegativeAmount::const_from_u64(5),
                false,
                None,
            );
            assert_eq!(cb.vtx.len(), 2);

            let mut batch_runner = if scan_multithreaded {
                let mut runner = BatchRunner::<_, _, _, _, ()>::new(
                    10,
                    dfvk.to_ivks()
                        .iter()
                        .map(|(scope, ivk, _)| ((account, *scope), ivk))
                        .map(|(tag, ivk)| (tag, PreparedIncomingViewingKey::new(ivk))),
                );

                add_block_to_runner(&Network::TestNetwork, cb.clone(), &mut runner);
                runner.flush();

                Some(runner)
            } else {
                None
            };

            let scanned_block = scan_block_with_runner(
                &Network::TestNetwork,
                cb,
                &[(&account, &dfvk)],
                &[],
                Some(&BlockMetadata::from_parts(
                    BlockHeight::from(0),
                    BlockHash([0u8; 32]),
                    Some(0),
                    #[cfg(feature = "orchard")]
                    Some(0),
                )),
                batch_runner.as_mut(),
            )
            .unwrap();
            let txs = scanned_block.transactions();
            assert_eq!(txs.len(), 1);

            let tx = &txs[0];
            assert_eq!(tx.index, 1);
            assert_eq!(tx.sapling_spends.len(), 0);
            assert_eq!(tx.sapling_outputs.len(), 1);
            assert_eq!(tx.sapling_outputs[0].index(), 0);
            assert_eq!(*tx.sapling_outputs[0].account(), account);
            assert_eq!(tx.sapling_outputs[0].note().value().inner(), 5);
            assert_eq!(
                tx.sapling_outputs[0].note_commitment_tree_position(),
                Position::from(1)
            );

            assert_eq!(scanned_block.sapling().final_tree_size(), 2);
            assert_eq!(
                scanned_block
                    .sapling()
                    .commitments()
                    .iter()
                    .map(|(_, retention)| *retention)
                    .collect::<Vec<_>>(),
                vec![
                    Retention::Ephemeral,
                    Retention::Checkpoint {
                        id: scanned_block.height(),
                        is_marked: true
                    }
                ]
            );
        }

        go(false);
        go(true);
    }

    #[test]
    fn scan_block_with_txs_after_my_tx() {
        fn go(scan_multithreaded: bool) {
            let account = AccountId::ZERO;
            let extsk = ExtendedSpendingKey::master(&[]);
            let dfvk = extsk.to_diversifiable_full_viewing_key();

            let cb = fake_compact_block(
                1u32.into(),
                BlockHash([0; 32]),
                Nullifier([0; 32]),
                &dfvk,
                NonNegativeAmount::const_from_u64(5),
                true,
                Some((0, 0)),
            );
            assert_eq!(cb.vtx.len(), 3);

            let mut batch_runner = if scan_multithreaded {
                let mut runner = BatchRunner::<_, _, _, _, ()>::new(
                    10,
                    dfvk.to_ivks()
                        .iter()
                        .map(|(scope, ivk, _)| ((account, *scope), ivk))
                        .map(|(tag, ivk)| (tag, PreparedIncomingViewingKey::new(ivk))),
                );

                add_block_to_runner(&Network::TestNetwork, cb.clone(), &mut runner);
                runner.flush();

                Some(runner)
            } else {
                None
            };

            let scanned_block = scan_block_with_runner(
                &Network::TestNetwork,
                cb,
                &[(&AccountId::ZERO, &dfvk)],
                &[],
                None,
                batch_runner.as_mut(),
            )
            .unwrap();
            let txs = scanned_block.transactions();
            assert_eq!(txs.len(), 1);

            let tx = &txs[0];
            assert_eq!(tx.index, 1);
            assert_eq!(tx.sapling_spends.len(), 0);
            assert_eq!(tx.sapling_outputs.len(), 1);
            assert_eq!(tx.sapling_outputs[0].index(), 0);
            assert_eq!(*tx.sapling_outputs[0].account(), AccountId::ZERO);
            assert_eq!(tx.sapling_outputs[0].note().value().inner(), 5);

            assert_eq!(
                scanned_block
                    .sapling()
                    .commitments()
                    .iter()
                    .map(|(_, retention)| *retention)
                    .collect::<Vec<_>>(),
                vec![
                    Retention::Ephemeral,
                    Retention::Marked,
                    Retention::Checkpoint {
                        id: scanned_block.height(),
                        is_marked: false
                    }
                ]
            );
        }

        go(false);
        go(true);
    }

    #[test]
    fn scan_block_with_my_spend() {
        let extsk = ExtendedSpendingKey::master(&[]);
        let dfvk = extsk.to_diversifiable_full_viewing_key();
        let nf = Nullifier([7; 32]);
        let account = AccountId::try_from(12).unwrap();

        let cb = fake_compact_block(
            1u32.into(),
            BlockHash([0; 32]),
            nf,
            &dfvk,
            NonNegativeAmount::const_from_u64(5),
            false,
            Some((0, 0)),
        );
        assert_eq!(cb.vtx.len(), 2);
        let sapling_keys: Vec<(&AccountId, &SaplingIvk)> = vec![];

        let scanned_block = scan_block(
            &Network::TestNetwork,
            cb,
            &sapling_keys[..],
            &[(account, nf)],
            None,
        )
        .unwrap();
        let txs = scanned_block.transactions();
        assert_eq!(txs.len(), 1);

        let tx = &txs[0];
        assert_eq!(tx.index, 1);
        assert_eq!(tx.sapling_spends.len(), 1);
        assert_eq!(tx.sapling_outputs.len(), 0);
        assert_eq!(tx.sapling_spends[0].index(), 0);
        assert_eq!(tx.sapling_spends[0].nf(), &nf);
        assert_eq!(tx.sapling_spends[0].account().to_owned(), account);

        assert_eq!(
            scanned_block
                .sapling()
                .commitments()
                .iter()
                .map(|(_, retention)| *retention)
                .collect::<Vec<_>>(),
            vec![
                Retention::Ephemeral,
                Retention::Checkpoint {
                    id: scanned_block.height(),
                    is_marked: false
                }
            ]
        );
    }
}
