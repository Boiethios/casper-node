#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, ops::Range, sync::Arc};

use casper_execution_engine::engine_state::{self, GetEraValidatorsError};
use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use num_rational::Ratio;

use crate::{
    contract_runtime::EraValidatorsRequest,
    effect::{
        requests::{ContractRuntimeRequest, StorageRequest},
        EffectBuilder,
    },
    types::ExecutableBlock,
};
use casper_types::{
    Block, Chainspec, CoreConfig, Digest, EraId, ProtocolVersion, PublicKey, RewardedSignatures,
    U512,
};

pub(crate) trait ReactorEventT:
    Send + From<StorageRequest> + From<ContractRuntimeRequest>
{
}

impl<T> ReactorEventT for T where T: Send + From<StorageRequest> + From<ContractRuntimeRequest> {}

#[derive(Debug)]
pub(crate) struct CitedBlock {
    height: u64,
    era_id: EraId,
    proposer: PublicKey,
    rewarded_signatures: RewardedSignatures,
    state_root_hash: Digest,
    is_switch_block: bool,
    is_genesis: bool,
}

#[derive(Debug)]
pub(crate) struct RewardsInfo {
    eras_info: BTreeMap<EraId, EraInfo>,
    cited_blocks: Vec<CitedBlock>,
}

/// The era information needed in the rewards computation:
#[derive(Debug, Clone)]
pub(crate) struct EraInfo {
    weights: BTreeMap<PublicKey, U512>,
    total_weights: U512,
    reward_per_round: Ratio<U512>,
}

#[derive(Debug)]
pub enum RewardsError {
    /// We got a block height which is not in the era range it should be in (should not happen).
    HeightNotInEraRange(u64),
    /// The era is not in the range we have (should not happen).
    EraIdNotInEraRange(EraId),
    /// The validator public key is not in the era it should be in (should not happen).
    ValidatorKeyNotInEra(Box<PublicKey>),
    /// We didn't have a required switch block.
    MissingSwitchBlock(EraId),
    /// We got an overflow while computing something.
    ArithmeticError(ArithmeticError),

    FailedToFetchBlockWithHeight(u64),
    FailedToFetchEra(GetEraValidatorsError),
    /// Fetching the era validators succedeed, but no info is present (should not happen).
    /// The `Digest` is the one that was queried.
    FailedToFetchEraValidators(Digest),
    FailedToFetchTotalSupply(engine_state::Error),
    FailedToFetchSeigniorageRate(engine_state::Error),
}

impl RewardsInfo {
    pub async fn new_from_storage<REv: ReactorEventT>(
        effect_builder: EffectBuilder<REv>,
        protocol_version: ProtocolVersion,
        signature_rewards_max_delay: u64,
        executable_block: ExecutableBlock,
    ) -> Result<Self, RewardsError> {
        let current_era_id = executable_block.era_id;
        // All the blocks that may appear as a signed block. They are collected upfront, so that we
        // don't have to worry about doing it one by one later.
        //
        // They are sorted from the oldest to the newest:

        let cited_block_height_start = {
            let previous_era_id = current_era_id.saturating_sub(1);
            let previous_era_switch_block_header = effect_builder
                .get_switch_block_header_by_era_id_from_storage(previous_era_id)
                .await
                .ok_or(RewardsError::MissingSwitchBlock(previous_era_id))?;

            // Here we do not substract 1, because we want one block more:
            previous_era_switch_block_header
                .height()
                .saturating_sub(signature_rewards_max_delay)
        };
        let range_to_fetch = cited_block_height_start..executable_block.height;

        let mut cited_blocks =
            collect_past_blocks_batched(effect_builder, range_to_fetch.clone()).await?;

        tracing::info!(
            current_era_id = %current_era_id.value(),
            range_requested = ?range_to_fetch,
            num_fetched_blocks = %cited_blocks.len(),
            "blocks fetched",
        );

        let eras_info = Self::create_eras_info(
            effect_builder,
            current_era_id,
            protocol_version,
            cited_blocks.iter(),
        )
        .await?;

        cited_blocks.push(executable_block.into());

        Ok(RewardsInfo {
            eras_info,
            cited_blocks,
        })
    }

    #[cfg(test)]
    pub fn new_testing(eras_info: BTreeMap<EraId, EraInfo>, cited_blocks: Vec<CitedBlock>) -> Self {
        Self {
            eras_info,
            cited_blocks,
        }
    }

    /// `block_hashs` is an iterator over the era ID to get the information about + the block
    /// hash to query to have such information (which may not be from the same era).
    async fn create_eras_info<REv: ReactorEventT>(
        effect_builder: EffectBuilder<REv>,
        current_era_id: EraId,
        protocol_version: ProtocolVersion,
        mut cited_blocks: impl Iterator<Item = &CitedBlock>,
    ) -> Result<BTreeMap<EraId, EraInfo>, RewardsError> {
        let oldest_block = cited_blocks.next();

        // If the oldest block is genesis, we add the validator information for genesis (era 0) from
        // era 1, because it's the same:
        let oldest_block_is_genesis = oldest_block.map_or(false, |block| block.is_genesis);

        // Here, we gather a list of all of the era ID we need to fetch to calculate the rewards,
        // as well as the state root hash allowing to query this information.
        //
        // To get all of the needed era IDs, we take the very first block, then every switch block
        // We take the first block, because we need it for the first cited era, then every switch
        // block for every subsequent eras.
        // If the first block is itself a switch block, that's fine, because we fetch one block more
        // in the first place to handle this case.
        let eras_and_state_root_hashes: Vec<_> = oldest_block
            .into_iter()
            .chain(cited_blocks.filter(|&block| block.is_switch_block))
            .map(|block| {
                let state_root_hash = block.state_root_hash;
                let era = if block.is_switch_block {
                    block.era_id.successor()
                } else {
                    block.era_id
                };

                (era, state_root_hash)
            })
            .collect();

        let num_eras_to_fetch =
            eras_and_state_root_hashes.len() + usize::from(oldest_block_is_genesis);

        let mut eras_info: BTreeMap<_, _> = stream::iter(eras_and_state_root_hashes)
            .then(|(era_id, state_root_hash)| async move {
                let weights = effect_builder
                    .get_era_validators_from_contract_runtime(EraValidatorsRequest::new(
                        state_root_hash,
                        protocol_version,
                    ))
                    .await
                    .map_err(RewardsError::FailedToFetchEra)?
                    // We consume the map to not clone the value:
                    .into_iter()
                    .find(|(key, _)| key == &era_id)
                    .ok_or_else(|| RewardsError::FailedToFetchEraValidators(state_root_hash))?
                    .1;
                let total_supply = effect_builder
                    .get_total_supply(state_root_hash)
                    .await
                    .map_err(RewardsError::FailedToFetchTotalSupply)?;
                let seignorate_rate = effect_builder
                    .get_round_seigniorage_rate(state_root_hash)
                    .await
                    .map_err(RewardsError::FailedToFetchSeigniorageRate)?;
                let reward_per_round = seignorate_rate * total_supply;
                let total_weights = weights.values().copied().sum();

                Ok::<_, RewardsError>((
                    era_id,
                    EraInfo {
                        weights,
                        total_weights,
                        reward_per_round,
                    },
                ))
            })
            .try_collect()
            .await?;

        // We cannot get the genesis info from a root hash, so we copy it from era 1 when needed.
        if oldest_block_is_genesis {
            let era_1 = EraId::from(1);
            let era_1_info = eras_info
                .get(&era_1)
                .ok_or(RewardsError::EraIdNotInEraRange(era_1))?;
            eras_info.insert(EraId::from(0), era_1_info.clone());
        }

        {
            let era_ids: Vec<_> = eras_info.keys().map(|id| id.value()).collect();
            tracing::info!(
                current_era_id = %current_era_id.value(),
                %num_eras_to_fetch,
                eras_fetched = ?era_ids,
            );
        }

        Ok(eras_info)
    }

    /// Returns the validators from a given era.
    pub fn validator_keys(
        &self,
        era_id: EraId,
    ) -> Result<impl Iterator<Item = PublicKey> + '_, RewardsError> {
        let keys = self
            .eras_info
            .get(&era_id)
            .ok_or(RewardsError::EraIdNotInEraRange(era_id))?
            .weights
            .keys()
            .cloned();

        Ok(keys)
    }

    /// Returns the total potential reward per block.
    /// Since it is per block, we do not care about the expected number of blocks per era.
    pub fn reward(&self, era_id: EraId) -> Result<Ratio<U512>, RewardsError> {
        Ok(self
            .eras_info
            .get(&era_id)
            .ok_or(RewardsError::EraIdNotInEraRange(era_id))?
            .reward_per_round)
    }

    /// Returns the weight ratio for a given validator for a given era.
    pub fn weight_ratio(
        &self,
        era_id: EraId,
        validator: &PublicKey,
    ) -> Result<Ratio<U512>, RewardsError> {
        let era = self
            .eras_info
            .get(&era_id)
            .ok_or(RewardsError::EraIdNotInEraRange(era_id))?;
        let weight = era
            .weights
            .get(validator)
            .ok_or_else(|| RewardsError::ValidatorKeyNotInEra(Box::new(validator.clone())))?;

        Ok(Ratio::new(*weight, era.total_weights))
    }

    /// Returns the era in which is the given block height.
    pub fn era_for_block_height(&self, height: u64) -> Result<EraId, RewardsError> {
        self.cited_blocks
            .iter()
            .find_map(|block| (block.height == height).then_some(block.era_id))
            .ok_or_else(|| RewardsError::HeightNotInEraRange(height))
    }

    /// Returns all the blocks belonging to an era.
    pub fn blocks_from_era(&self, era_id: EraId) -> impl Iterator<Item = &CitedBlock> {
        self.cited_blocks
            .iter()
            .filter(move |block| block.era_id == era_id)
    }
}

impl EraInfo {
    #[cfg(test)]
    pub fn new_testing(weights: BTreeMap<PublicKey, U512>, reward_per_round: Ratio<U512>) -> Self {
        let total_weights = weights.values().copied().sum();
        Self {
            weights,
            total_weights,
            reward_per_round,
        }
    }
}

/// First create the `RewardsInfo` structure, then compute the rewards.
/// It is done in 2 steps so that it is easier to unit test the rewards calculation.
pub(crate) async fn fetch_data_and_calculate_rewards_for_era<REv: ReactorEventT>(
    effect_builder: EffectBuilder<REv>,
    chainspec: Arc<Chainspec>,
    executable_block: ExecutableBlock,
) -> Result<BTreeMap<PublicKey, U512>, RewardsError> {
    let current_era_id = executable_block.era_id;
    tracing::info!(
        current_era_id = %current_era_id.value(),
        "starting the rewards calculation"
    );

    if current_era_id.is_genesis() {
        // Special case: genesis block does not yield any reward, because there is no block
        // producer, and no previous blocks whose signatures are to be rewarded:
        Ok(chainspec
            .network_config
            .accounts_config
            .validators()
            .map(|account| (account.public_key.clone(), U512::zero()))
            .collect())
    } else {
        let rewards_info = RewardsInfo::new_from_storage(
            effect_builder,
            chainspec.protocol_version(),
            chainspec.core_config.signature_rewards_max_delay,
            executable_block,
        )
        .await?;

        rewards_for_era(rewards_info, current_era_id, &chainspec.core_config)
    }
}

pub(crate) fn rewards_for_era(
    rewards_info: RewardsInfo,
    current_era_id: EraId,
    core_config: &CoreConfig,
) -> Result<BTreeMap<PublicKey, U512>, RewardsError> {
    let mut full_reward_for_validators: BTreeMap<_, _> = rewards_info
        .validator_keys(current_era_id)?
        .map(|key| (key, Ratio::new(U512::zero(), U512::one())))
        .collect();

    let mut increase_value_for_key =
        |key: PublicKey, value: MaybeNum<Ratio<U512>>| -> Result<(), RewardsError> {
            match full_reward_for_validators.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(value.get()?);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let new_value = value + *entry.get();
                    *entry.get_mut() = new_value.get()?;
                }
            }

            Ok(())
        };

    // Rules out a special case: genesis block does not yield any reward,
    // because there is no block producer, and no previous blocks whose
    // signatures are to be rewarded:
    if current_era_id.is_genesis() == false {
        let collection_proportion = MaybeNum::from(core_config.collection_rewards_proportion());
        let contribution_proportion = MaybeNum::from(core_config.contribution_rewards_proportion());

        // Reward for producing a block from this era:
        let production_reward = MaybeNum::from(core_config.production_rewards_proportion())
            * rewards_info.reward(current_era_id)?;

        // Collect all rewards as a ratio:
        for block in rewards_info.blocks_from_era(current_era_id) {
            // Transfer the block production reward for this block proposer:
            increase_value_for_key(block.proposer.clone(), production_reward)?;

            // Now, let's compute the reward attached to each signed block reported by the block
            // we examine:
            for (signature_rewards, signed_block_height) in block
                .rewarded_signatures
                .iter()
                .zip((0..block.height).rev())
            {
                let signed_block_era = rewards_info.era_for_block_height(signed_block_height)?;
                let validators_providing_signature = signature_rewards
                    .to_validator_set(rewards_info.validator_keys(signed_block_era)?);

                for signing_validator in validators_providing_signature {
                    // Reward for contributing to the finality signature, ie signing this block:
                    let contribution_reward = contribution_proportion
                        * rewards_info.weight_ratio(signed_block_era, &signing_validator)?
                        * rewards_info.reward(signed_block_era)?;
                    // Reward for gathering this signature. It is both weighted by the block
                    // producing/signature collecting validator, and the signing validator:
                    let collection_reward = collection_proportion
                        * rewards_info.weight_ratio(signed_block_era, &signing_validator)?
                        * rewards_info.reward(signed_block_era)?;

                    increase_value_for_key(signing_validator, contribution_reward)?;
                    increase_value_for_key(block.proposer.clone(), collection_reward)?;
                }
            }
        }
    }

    // Return the rewards as plain U512:
    Ok(full_reward_for_validators
        .into_iter()
        .map(|(key, amount)| (key, amount.to_integer()))
        .collect())
}

/// Query all the blocks from the given range with a batch mechanism.
async fn collect_past_blocks_batched<REv: From<StorageRequest>>(
    effect_builder: EffectBuilder<REv>,
    era_height_span: Range<u64>,
) -> Result<Vec<CitedBlock>, RewardsError> {
    const STEP: usize = 100;
    let only_from_available_block_range = false;

    let batches = {
        let range_end = era_height_span.end;

        era_height_span
            .step_by(STEP)
            .map(move |internal_start| internal_start..range_end.min(internal_start + STEP as u64))
    };

    stream::iter(batches)
        .then(|range| async move {
            stream::iter(
                effect_builder
                    .collect_past_blocks_with_metadata(
                        range.clone(),
                        only_from_available_block_range,
                    )
                    .await
                    .into_iter()
                    .zip(range)
                    .map(|(maybe_block_with_metadata, height)| {
                        maybe_block_with_metadata
                            .ok_or(RewardsError::FailedToFetchBlockWithHeight(height))
                            .map(|b| CitedBlock::from(b.block))
                    }),
            )
        })
        .flatten()
        .try_collect()
        .await
}

impl From<ArithmeticError> for RewardsError {
    fn from(value: ArithmeticError) -> Self {
        Self::ArithmeticError(value)
    }
}

impl From<Block> for CitedBlock {
    fn from(block: Block) -> Self {
        Self {
            era_id: block.era_id(),
            height: block.height(),
            proposer: block.proposer().clone(),
            rewarded_signatures: block.rewarded_signatures().clone(),
            state_root_hash: *block.state_root_hash(),
            is_switch_block: block.is_switch_block(),
            is_genesis: block.is_genesis(),
        }
    }
}

impl From<ExecutableBlock> for CitedBlock {
    fn from(block: ExecutableBlock) -> Self {
        Self {
            era_id: block.era_id,
            height: block.height,
            proposer: *block.proposer,
            rewarded_signatures: block.rewarded_signatures,
            state_root_hash: Digest::default(),
            is_switch_block: block.era_report.is_some(),
            // When the executable block is genesis, it's a different path:
            is_genesis: false,
        }
    }
}

use fallible_num::{ArithmeticError, MaybeNum};
mod fallible_num {
    use casper_types::U512;
    use num_rational::Ratio;
    use num_traits::{CheckedAdd, CheckedMul};
    use std::ops;

    #[derive(Debug, Copy, Clone)]
    pub enum ArithmeticError {
        Overflow,
    }

    #[derive(Debug, Copy, Clone)]
    pub enum MaybeNum<N> {
        Num(N),
        Error(ArithmeticError),
    }

    impl<N> MaybeNum<N> {
        pub fn get(self) -> Result<N, ArithmeticError> {
            match self {
                MaybeNum::Num(n) => Ok(n),
                MaybeNum::Error(e) => Err(e),
            }
        }
    }

    impl<N> From<N> for MaybeNum<N> {
        fn from(n: N) -> MaybeNum<N> {
            MaybeNum::Num(n)
        }
    }

    impl From<Ratio<u64>> for MaybeNum<Ratio<U512>> {
        fn from(ratio: Ratio<u64>) -> Self {
            MaybeNum::Num(Ratio::new(
                U512::from(*ratio.numer()),
                U512::from(*ratio.denom()),
            ))
        }
    }

    impl<N> ops::Mul<N> for MaybeNum<Ratio<U512>>
    where
        N: Into<MaybeNum<Ratio<U512>>>,
    {
        type Output = Self;

        fn mul(self, rhs: N) -> Self::Output {
            match nums(self, rhs.into()) {
                Ok((a, b)) => match a.checked_mul(&b) {
                    Some(ans) => MaybeNum::Num(ans),
                    None => MaybeNum::Error(ArithmeticError::Overflow),
                },
                Err(e) => MaybeNum::Error(e),
            }
        }
    }

    impl<N> ops::Add<N> for MaybeNum<Ratio<U512>>
    where
        N: Into<MaybeNum<Ratio<U512>>>,
    {
        type Output = Self;

        fn add(self, rhs: N) -> Self::Output {
            match nums(self, rhs.into()) {
                Ok((a, b)) => match a.checked_add(&b) {
                    Some(ans) => MaybeNum::Num(ans),
                    None => MaybeNum::Error(ArithmeticError::Overflow),
                },
                Err(e) => MaybeNum::Error(e),
            }
        }
    }

    fn nums<A, B>(a: MaybeNum<A>, b: MaybeNum<B>) -> Result<(A, B), ArithmeticError> {
        match (a, b) {
            (MaybeNum::Num(a), MaybeNum::Num(b)) => Ok((a, b)),
            (MaybeNum::Error(e), _) | (_, MaybeNum::Error(e)) => Err(e),
        }
    }
}
