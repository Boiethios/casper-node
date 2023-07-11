//! Block validator
//!
//! The block validator checks whether all the deploys included in the block payload exist, either
//! locally or on the network.
//!
//! When multiple requests are made to validate the same block payload, they will eagerly return
//! true if valid, but only fail if all sources have been exhausted. This is only relevant when
//! calling for validation of the same proposed block multiple times at the same time.

mod keyed_counter;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Debug,
    sync::Arc,
};

use datasize::DataSize;
use derive_more::{Display, From};
use itertools::Itertools;
use smallvec::{smallvec, SmallVec};
use tracing::{info, warn};

use casper_types::{EraId, Timestamp};

use crate::{
    components::{
        consensus::{ClContext, ProposedBlock},
        fetcher::{EmptyValidationMetadata, FetchedData},
        Component,
    },
    effect::{
        requests::{BlockValidationRequest, FetcherRequest, StorageRequest},
        EffectBuilder, EffectExt, Effects, Responder,
    },
    types::{
        appendable_block::AppendableBlock, Approval, BlockWithMetadata, Chainspec, Deploy,
        DeployFootprint, DeployHash, DeployHashWithApprovals, DeployOrTransferHash, LegacyDeploy,
        NodeId, ValidatorMatrix,
    },
    NodeRng,
};
use keyed_counter::KeyedCounter;

const COMPONENT_NAME: &str = "block_validator";

impl ProposedBlock<ClContext> {
    fn timestamp(&self) -> Timestamp {
        self.context().timestamp()
    }

    fn deploy_hashes(&self) -> impl Iterator<Item = &DeployHash> + '_ {
        self.value().deploy_hashes()
    }

    fn transfer_hashes(&self) -> impl Iterator<Item = &DeployHash> + '_ {
        self.value().transfer_hashes()
    }

    fn deploys_and_transfers_iter(
        &self,
    ) -> impl Iterator<Item = (DeployOrTransferHash, BTreeSet<Approval>)> + '_ {
        let deploys = self.value().deploys().iter().map(|dwa| {
            (
                DeployOrTransferHash::Deploy(*dwa.deploy_hash()),
                dwa.approvals().clone(),
            )
        });
        let transfers = self.value().transfers().iter().map(|dwa| {
            (
                DeployOrTransferHash::Transfer(*dwa.deploy_hash()),
                dwa.approvals().clone(),
            )
        });
        deploys.chain(transfers)
    }
}

/// Block validator component event.
#[derive(Debug, From, Display)]
pub(crate) enum Event {
    /// A request made of the block validator component.
    #[from]
    Request(BlockValidationRequest),

    ///TODO
    #[display(fmt = "{:?} read from storage", past_blocks_with_metadata)]
    GotPastBlockWithMetadata {
        past_blocks_with_metadata: Vec<Option<BlockWithMetadata>>,
        proposed_block_era_id: EraId,
        proposed_block_height: u64,
        proposed_block: ProposedBlock<ClContext>,
    },

    /// A deploy has been successfully found.
    #[display(fmt = "{} found", dt_hash)]
    DeployFound {
        dt_hash: DeployOrTransferHash,
        deploy_footprint: Box<DeployFootprint>,
    },

    /// A request to find a specific deploy, potentially from a peer, failed.
    #[display(fmt = "{} missing", _0)]
    DeployMissing(DeployOrTransferHash),

    /// Deploy was invalid. Unable to convert to a deploy type.
    #[display(fmt = "{} invalid", _0)]
    CannotConvertDeploy(DeployOrTransferHash),
}

/// State of the current process of block validation.
///
/// Tracks whether or not there are deploys still missing and who is interested in the final result.
#[derive(DataSize, Debug)]
pub(crate) struct BlockValidationState {
    /// Appendable block ensuring that the deploys satisfy the validity conditions.
    appendable_block: AppendableBlock,
    /// The set of approvals contains approvals from deploys that would be finalized with the
    /// block.
    missing_deploys: HashMap<DeployOrTransferHash, BTreeSet<Approval>>,
    /// A list of responders that are awaiting an answer.
    responders: SmallVec<[Responder<bool>; 2]>,
    // /// TODO
}

impl BlockValidationState {
    fn respond<REv>(&mut self, value: bool) -> Effects<REv> {
        self.responders
            .drain(..)
            .flat_map(|responder| responder.respond(value).ignore())
            .collect()
    }
}

#[derive(DataSize, Debug)]
pub(crate) struct BlockValidator {
    /// Chainspec loaded for deploy validation.
    #[data_size(skip)]
    chainspec: Arc<Chainspec>,
    #[data_size(skip)]
    validator_matrix: ValidatorMatrix,
    /// State of validation of a specific block.
    validation_states: HashMap<ProposedBlock<ClContext>, BlockValidationState>,
    /// Number of requests for a specific deploy hash still in flight.
    in_flight: KeyedCounter<DeployHash>,
}

impl BlockValidator {
    /// Creates a new block validator instance.
    pub(crate) fn new(chainspec: Arc<Chainspec>, validator_matrix: ValidatorMatrix) -> Self {
        BlockValidator {
            chainspec,
            validator_matrix,
            validation_states: HashMap::new(),
            in_flight: KeyedCounter::default(),
        }
    }

    /// Prints a log message about an invalid block with duplicated deploys.
    fn log_block_with_replay(&self, sender: NodeId, block: &ProposedBlock<ClContext>) {
        let mut deploy_counts = BTreeMap::new();
        for (dt_hash, _) in block.deploys_and_transfers_iter() {
            *deploy_counts.entry(dt_hash).or_default() += 1;
        }
        let duplicates = deploy_counts
            .into_iter()
            .filter_map(|(dt_hash, count): (DeployOrTransferHash, usize)| {
                (count > 1).then(|| format!("{} * {}", count, dt_hash))
            })
            .join(", ");
        info!(
            peer_id=?sender, %duplicates,
            "received invalid block containing duplicated deploys"
        );
    }

    fn handle_got_past_blocks_with_metadata<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        past_blocks_with_metadata: Vec<Option<BlockWithMetadata>>,
        proposed_block_era_id: EraId,
        proposed_block_height: u64,
        proposed_block: ProposedBlock<ClContext>,
    ) -> Effects<Event> {
        let num_ancestor_values = proposed_block.context().ancestor_values().len();

        if past_blocks_with_metadata
            .iter()
            .rev()
            .skip(num_ancestor_values)
            .any(|maybe_block| maybe_block.is_none())
        {
            // TODO: we _need_ those blocks to validate the new one - fetch them, or something?
            return Effects::new();
        }

        // This will create a map of relative_height → era_id - relative_height being the number of
        // blocks in the past relative to the current block, minus 1 (ie., 0 is the previous block,
        // 1 is the one before that, etc.) - these indices will correspond directly to the indices
        // in RewardedSignatures
        let era_ids_vec: Vec<_> = std::iter::repeat(proposed_block_era_id)
            .take(num_ancestor_values)
            .chain(
                past_blocks_with_metadata
                    .into_iter()
                    .rev()
                    .skip(num_ancestor_values)
                    .flatten()
                    .map(|metadata| metadata.block.header().era_id()),
            )
            .collect();

        let era_ids: BTreeSet<_> = era_ids_vec.iter().copied().collect();

        let validators: BTreeMap<_, BTreeSet<_>> = era_ids
            .into_iter()
            .filter_map(|era_id| {
                self.validator_matrix
                    .validator_weights(era_id)
                    .map(|weights| (era_id, weights.into_validator_public_keys().collect()))
            })
            .collect();

        // This will be a map from block height to the set of public keys of the validators who are
        // supposed to have signed that block.
        let included_sigs: BTreeMap<_, _> = proposed_block
            .value()
            .rewarded_signatures()
            .iter()
            .zip(era_ids_vec)
            .enumerate()
            .map(|(i, (single_block_rewarded_sigs, era_id))| {
                let all_validators = validators.get(&era_id).unwrap(); // TODO: don't unwrap
                (
                    proposed_block_height
                        .saturating_sub(i as u64)
                        .saturating_sub(1),
                    single_block_rewarded_sigs
                        .clone()
                        .into_validator_set(all_validators.into_iter().cloned()),
                )
            })
            .collect();

        todo!()
        //let validator_keys: Option<BTreeSet<_>> = self
        //    .validator_matrix
        //    .validator_weights(todo!())
        //    .map(|weights| weights.into_validator_public_keys().collect());
    }
}

impl<REv> Component<REv> for BlockValidator
where
    REv: From<Event>
        + From<BlockValidationRequest>
        + From<FetcherRequest<LegacyDeploy>>
        + From<StorageRequest>
        + Send,
{
    type Event = Event;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut NodeRng,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        let mut effects = Effects::new();
        match event {
            Event::Request(BlockValidationRequest {
                proposed_block_era_id,
                proposed_block_height,
                block,
                sender,
                responder,
            }) => {
                if block.deploy_hashes().count()
                    > self.chainspec.deploy_config.block_max_deploy_count as usize
                {
                    return responder.respond(false).ignore();
                }
                if block.transfer_hashes().count()
                    > self.chainspec.deploy_config.block_max_transfer_count as usize
                {
                    return responder.respond(false).ignore();
                }

                let deploy_count = block.deploy_hashes().count() + block.transfer_hashes().count();
                if deploy_count == 0 {
                    // If there are no deploys, return early.
                    return responder.respond(true).ignore();
                }

                // Collect the deploys in a map. If they are fewer now, then there was a duplicate!
                let block_deploys: HashMap<_, _> = block.deploys_and_transfers_iter().collect();
                if block_deploys.len() != deploy_count {
                    self.log_block_with_replay(sender, &block);
                    return responder.respond(false).ignore();
                }

                let block_timestamp = block.timestamp();
                let state =
                    self.validation_states
                        .entry(block.clone())
                        .or_insert(BlockValidationState {
                            appendable_block: AppendableBlock::new(
                                self.chainspec.deploy_config,
                                block_timestamp,
                            ),
                            missing_deploys: block_deploys.clone(),
                            responders: smallvec![],
                        });

                if state.missing_deploys.is_empty() {
                    // Block has already been validated successfully, early return to caller.
                    return responder.respond(true).ignore();
                }

                // We register ourselves as someone interested in the ultimate validation result.
                state.responders.push(responder);

                effects.extend(block_deploys.into_iter().flat_map(|(dt_hash, _)| {
                    // For every request, increase the number of in-flight...
                    self.in_flight.inc(&dt_hash.into());
                    // ...then request it.
                    fetch_deploy(effect_builder, dt_hash, sender)
                }));

                let signature_rewards_max_delay =
                    self.chainspec.core_config.signature_rewards_max_delay;
                let minimum_block_height =
                    proposed_block_height.saturating_sub(signature_rewards_max_delay);

                effects.extend(
                    effect_builder
                        .collect_past_blocks_with_metadata(
                            minimum_block_height..proposed_block_height,
                            false,
                        )
                        .event(
                            move |past_blocks_with_metadata| Event::GotPastBlockWithMetadata {
                                past_blocks_with_metadata,
                                proposed_block_era_id,
                                proposed_block_height,
                                proposed_block: block,
                            },
                        ),
                );
            }
            Event::GotPastBlockWithMetadata {
                past_blocks_with_metadata,
                proposed_block_era_id,
                proposed_block_height,
                proposed_block,
            } => {
                effects.extend(self.handle_got_past_blocks_with_metadata(
                    effect_builder,
                    past_blocks_with_metadata,
                    proposed_block_era_id,
                    proposed_block_height,
                    proposed_block,
                ));
            }
            Event::DeployFound {
                dt_hash,
                deploy_footprint,
            } => {
                // We successfully found a hash. Decrease the number of outstanding requests.
                self.in_flight.dec(&dt_hash.into());

                // If a deploy is received for a given block that makes that block invalid somehow,
                // mark it for removal.
                let mut invalid = Vec::new();

                // Our first pass updates all validation states, crossing off the found deploy.
                for (key, state) in self.validation_states.iter_mut() {
                    if let Some(approvals) = state.missing_deploys.remove(&dt_hash) {
                        // If the deploy is of the wrong type or would be invalid for this block,
                        // notify everyone still waiting on it that all is lost.
                        let add_result = match dt_hash {
                            DeployOrTransferHash::Deploy(hash) => {
                                state.appendable_block.add_deploy(
                                    DeployHashWithApprovals::new(hash, approvals.clone()),
                                    &deploy_footprint,
                                )
                            }
                            DeployOrTransferHash::Transfer(hash) => {
                                state.appendable_block.add_transfer(
                                    DeployHashWithApprovals::new(hash, approvals.clone()),
                                    &deploy_footprint,
                                )
                            }
                        };
                        if let Err(err) = add_result {
                            info!(block = ?key, %dt_hash, ?deploy_footprint, ?err, "block invalid");
                            invalid.push(key.clone());
                        }
                    }
                }

                // Now we remove all states that have finished and notify the requesters.
                self.validation_states.retain(|key, state| {
                    if invalid.contains(key) {
                        effects.extend(state.respond(false));
                        return false;
                    }
                    if state.missing_deploys.is_empty() {
                        // This one is done and valid.
                        effects.extend(state.respond(true));
                        return false;
                    }
                    true
                });
            }
            Event::DeployMissing(dt_hash) => {
                info!(%dt_hash, "request to download deploy timed out");
                // A deploy failed to fetch. If there is still hope (i.e. other outstanding
                // requests), we just ignore this little accident.
                if self.in_flight.dec(&dt_hash.into()) != 0 {
                    return Effects::new();
                }

                self.validation_states.retain(|key, state| {
                    if !state.missing_deploys.contains_key(&dt_hash) {
                        return true;
                    }

                    // Notify everyone still waiting on it that all is lost.
                    info!(block = ?key, %dt_hash, "could not validate the deploy. block is invalid");
                    // This validation state contains a deploy hash we failed to fetch from all
                    // sources, it can never succeed.
                    effects.extend(state.respond(false));
                    false
                });
            }
            Event::CannotConvertDeploy(dt_hash) => {
                // Deploy is invalid. There's no point waiting for other in-flight requests to
                // finish.
                self.in_flight.dec(&dt_hash.into());

                self.validation_states.retain(|key, state| {
                    if state.missing_deploys.contains_key(&dt_hash) {
                        // Notify everyone still waiting on it that all is lost.
                        info!(
                            block = ?key, %dt_hash,
                            "could not convert deploy to deploy type. block is invalid"
                        );
                        // This validation state contains a failed deploy hash, it can never
                        // succeed.
                        effects.extend(state.respond(false));
                        false
                    } else {
                        true
                    }
                });
            }
        }
        effects
    }

    fn name(&self) -> &str {
        COMPONENT_NAME
    }
}

/// Returns effects that fetch the deploy and validate it.
fn fetch_deploy<REv>(
    effect_builder: EffectBuilder<REv>,
    dt_hash: DeployOrTransferHash,
    sender: NodeId,
) -> Effects<Event>
where
    REv: From<Event> + From<FetcherRequest<LegacyDeploy>> + Send,
{
    async move {
        let deploy_hash: DeployHash = dt_hash.into();
        let deploy = match effect_builder
            .fetch::<LegacyDeploy>(deploy_hash, sender, Box::new(EmptyValidationMetadata))
            .await
        {
            Ok(FetchedData::FromStorage { item }) | Ok(FetchedData::FromPeer { item, .. }) => {
                Deploy::from(*item)
            }
            Err(fetcher_error) => {
                warn!(
                    "Could not fetch deploy with deploy hash {}: {}",
                    deploy_hash, fetcher_error
                );
                return Event::DeployMissing(dt_hash);
            }
        };
        if deploy.deploy_or_transfer_hash() != dt_hash {
            warn!(
                deploy = ?deploy,
                expected_deploy_or_transfer_hash = ?dt_hash,
                actual_deploy_or_transfer_hash = ?deploy.deploy_or_transfer_hash(),
                "Deploy has incorrect transfer hash"
            );
            return Event::CannotConvertDeploy(dt_hash);
        }
        match deploy.footprint() {
            Ok(deploy_footprint) => Event::DeployFound {
                dt_hash,
                deploy_footprint: Box::new(deploy_footprint),
            },
            Err(error) => {
                warn!(
                    deploy = ?deploy,
                    deploy_or_transfer_hash = ?dt_hash,
                    ?error,
                    "Could not convert deploy",
                );
                Event::CannotConvertDeploy(dt_hash)
            }
        }
    }
    .event(std::convert::identity)
}
