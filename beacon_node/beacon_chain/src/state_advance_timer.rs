//! Provides a timer which runs in the tail-end of each slot and maybe advances the state of the
//! head block forward a single slot.
//!
//! This provides an optimization with the following benefits:
//!
//! 1. Removes the burden of a single, mandatory `per_slot_processing` call from the leading-edge of
//!    block processing. This helps import blocks faster.
//! 2. Allows the node to learn of the shuffling for the next epoch, before the first block from
//!    that epoch has arrived. This helps reduce gossip block propagation times.
//!
//! The downsides to this optimization are:
//!
//! 1. We are required to store an additional `BeaconState` for the head block. This consumes
//!    memory.
//! 2. There's a possibility that the head block is never built upon, causing wasted CPU cycles.
use crate::validator_monitor::HISTORIC_EPOCHS as VALIDATOR_MONITOR_HISTORIC_EPOCHS;
use crate::{
    beacon_chain::ATTESTATION_CACHE_LOCK_TIMEOUT, BeaconChain, BeaconChainError, BeaconChainTypes,
};
use slog::{debug, error, warn, Logger};
use slot_clock::SlotClock;
use state_processing::per_slot_processing;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use task_executor::TaskExecutor;
use tokio::time::sleep;
use types::{AttestationShufflingId, BeaconStateError, EthSpec, Hash256, RelativeEpoch, Slot};

/// If the head slot is more than `MAX_ADVANCE_DISTANCE` from the current slot, then don't perform
/// the state advancement.
///
/// This avoids doing unnecessary work whilst the node is syncing or has perhaps been put to sleep
/// for some period of time.
const MAX_ADVANCE_DISTANCE: u64 = 4;

#[derive(Debug)]
enum Error {
    BeaconChain(BeaconChainError),
    BeaconState(BeaconStateError),
    Store(store::Error),
    HeadMissingFromSnapshotCache(Hash256),
    MaxDistanceExceeded {
        current_slot: Slot,
        head_slot: Slot,
    },
    StateAlreadyAdvanced {
        block_root: Hash256,
    },
    BadStateSlot {
        _state_slot: Slot,
        _current_slot: Slot,
    },
}

impl From<BeaconChainError> for Error {
    fn from(e: BeaconChainError) -> Self {
        Self::BeaconChain(e)
    }
}

impl From<BeaconStateError> for Error {
    fn from(e: BeaconStateError) -> Self {
        Self::BeaconState(e)
    }
}

impl From<store::Error> for Error {
    fn from(e: store::Error) -> Self {
        Self::Store(e)
    }
}

/// Provides a simple thread-safe lock to be used for task co-ordination. Practically equivalent to
/// `Mutex<()>`.
#[derive(Clone)]
struct Lock(Arc<AtomicBool>);

impl Lock {
    /// Instantiate an unlocked self.
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Lock self, returning `true` if the lock was already set.
    pub fn lock(&self) -> bool {
        self.0.fetch_or(true, Ordering::SeqCst)
    }

    /// Unlock self.
    pub fn unlock(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Spawns the timer described in the module-level documentation.
pub fn spawn_state_advance_timer<T: BeaconChainTypes>(
    executor: TaskExecutor,
    beacon_chain: Arc<BeaconChain<T>>,
    log: Logger,
) {
    executor.spawn(
        state_advance_timer(executor.clone(), beacon_chain, log),
        "state_advance_timer",
    );
}

/// Provides the timer described in the module-level documentation.
async fn state_advance_timer<T: BeaconChainTypes>(
    executor: TaskExecutor,
    beacon_chain: Arc<BeaconChain<T>>,
    log: Logger,
) {
    let is_running = Lock::new();
    let slot_clock = &beacon_chain.slot_clock;
    let slot_duration = slot_clock.slot_duration();

    loop {
        match beacon_chain.slot_clock.duration_to_next_slot() {
            Some(duration) => sleep(duration + (slot_duration / 4) * 3).await,
            None => {
                error!(log, "Failed to read slot clock");
                // If we can't read the slot clock, just wait another slot.
                sleep(slot_duration).await;
                continue;
            }
        };

        // Only start spawn the state advance task if the lock was previously free.
        if !is_running.lock() {
            let log = log.clone();
            let beacon_chain = beacon_chain.clone();
            let is_running = is_running.clone();

            executor.spawn_blocking(
                move || {
                    match advance_head(&beacon_chain, &log) {
                        Ok(()) => (),
                        Err(Error::BeaconChain(e)) => error!(
                            log,
                            "Failed to advance head state";
                            "error" => ?e
                        ),
                        Err(Error::StateAlreadyAdvanced { block_root }) => debug!(
                            log,
                            "State already advanced on slot";
                            "block_root" => ?block_root
                        ),
                        Err(Error::MaxDistanceExceeded {
                            current_slot,
                            head_slot,
                        }) => debug!(
                            log,
                            "Refused to advance head state";
                            "head_slot" => head_slot,
                            "current_slot" => current_slot,
                        ),
                        other => warn!(
                            log,
                            "Did not advance head state";
                            "reason" => ?other
                        ),
                    };

                    // Permit this blocking task to spawn again, next time the timer fires.
                    is_running.unlock();
                },
                "state_advance_blocking",
            );
        } else {
            warn!(
                log,
                "State advance routine overloaded";
                "msg" => "system resources may be overloaded"
            )
        }
    }
}

fn advance_head<T: BeaconChainTypes>(
    beacon_chain: &BeaconChain<T>,
    log: &Logger,
) -> Result<(), Error> {
    let current_slot = beacon_chain.slot()?;

    // These brackets ensure that the `head_slot` value is dropped before we run fork choice and
    // potentially invalidate it.
    //
    // Fork-choice is not run *before* this function to avoid unnecessary calls whilst syncing.
    {
        let head_slot = beacon_chain.head_info()?.slot;

        // Don't run this when syncing or if lagging too far behind.
        if head_slot + MAX_ADVANCE_DISTANCE < current_slot {
            return Err(Error::MaxDistanceExceeded {
                current_slot,
                head_slot,
            });
        }
    }

    // Run fork choice so we get the latest view of the head.
    //
    // This is useful since it's quite likely that the last time we ran fork choice was shortly
    // after receiving the latest gossip block, but not necessarily after we've received the
    // majority of attestations.
    beacon_chain.fork_choice()?;

    let head_info = beacon_chain.head_info()?;
    let head_block_root = head_info.block_root;

    let (head_state_root, mut state) = beacon_chain
        .store
        .get_advanced_state(head_block_root, current_slot, head_info.state_root)?
        .ok_or(Error::HeadMissingFromSnapshotCache(head_block_root))?;

    if state.slot() == current_slot + 1 {
        return Err(Error::StateAlreadyAdvanced {
            block_root: head_block_root,
        });
    } else if state.slot() != current_slot {
        // Protect against advancing a state more than a single slot.
        //
        // Advancing more than one slot without storing the intermediate state would corrupt the
        // database. Future works might store temporary, intermediate states inside this function.
        return Err(Error::BadStateSlot {
            _state_slot: state.slot(),
            _current_slot: current_slot,
        });
    }

    let initial_slot = state.slot();
    let initial_epoch = state.current_epoch();

    // Advance the state a single slot.
    if let Some(summary) =
        per_slot_processing(&mut state, Some(head_state_root), &beacon_chain.spec)
            .map_err(BeaconChainError::from)?
    {
        // Expose Prometheus metrics.
        if let Err(e) = summary.observe_metrics() {
            error!(
                log,
                "Failed to observe epoch summary metrics";
                "src" => "state_advance_timer",
                "error" => ?e
            );
        }

        // Only notify the validator monitor for recent blocks.
        if state.current_epoch() + VALIDATOR_MONITOR_HISTORIC_EPOCHS as u64
            >= current_slot.epoch(T::EthSpec::slots_per_epoch())
        {
            // Potentially create logs/metrics for locally monitored validators.
            if let Err(e) = beacon_chain
                .validator_monitor
                .read()
                .process_validator_statuses(state.current_epoch(), &summary, &beacon_chain.spec)
            {
                error!(
                    log,
                    "Unable to process validator statuses";
                    "error" => ?e
                );
            }
        }
    }

    debug!(
        log,
        "Advanced head state one slot";
        "head_block_root" => ?head_block_root,
        "state_slot" => state.slot(),
        "current_slot" => current_slot,
    );

    // Build the current epoch cache, to prepare to compute proposer duties.
    state
        .build_committee_cache(RelativeEpoch::Current, &beacon_chain.spec)
        .map_err(BeaconChainError::from)?;
    // Build the next epoch cache, to prepare to compute attester duties.
    state
        .build_committee_cache(RelativeEpoch::Next, &beacon_chain.spec)
        .map_err(BeaconChainError::from)?;

    // If the `pre_state` is in a later epoch than `state`, pre-emptively add the proposer shuffling
    // for the state's current epoch and the committee cache for the state's next epoch.
    if initial_epoch < state.current_epoch() {
        // Update the proposer cache.
        //
        // We supply the `head_block_root` as the decision block since the prior `if` statement guarantees
        // the head root is the latest block from the prior epoch.
        beacon_chain
            .beacon_proposer_cache
            .lock()
            .insert(
                state.current_epoch(),
                head_block_root,
                state
                    .get_beacon_proposer_indices(&beacon_chain.spec)
                    .map_err(BeaconChainError::from)?,
                state.fork(),
            )
            .map_err(BeaconChainError::from)?;

        // Update the attester cache.
        let shuffling_id =
            AttestationShufflingId::new(head_block_root, &state, RelativeEpoch::Next)
                .map_err(BeaconChainError::from)?;
        let committee_cache = state
            .committee_cache(RelativeEpoch::Next)
            .map_err(BeaconChainError::from)?;
        beacon_chain
            .shuffling_cache
            .try_write_for(ATTESTATION_CACHE_LOCK_TIMEOUT)
            .ok_or(BeaconChainError::AttestationCacheLockTimeout)?
            .insert(shuffling_id.clone(), committee_cache);

        debug!(
            log,
            "Primed proposer and attester caches";
            "head_block_root" => ?head_block_root,
            "next_epoch_shuffling_root" => ?shuffling_id.shuffling_decision_block,
            "state_epoch" => state.current_epoch(),
            "current_epoch" => current_slot.epoch(T::EthSpec::slots_per_epoch()),
        );
    }

    // Apply the state to the attester cache, if the cache deems it interesting.
    beacon_chain
        .attester_cache
        .maybe_cache_state(&state, head_block_root, &beacon_chain.spec)
        .map_err(BeaconChainError::from)?;

    let final_slot = state.slot();

    // Write the advanced state to the database.
    let advanced_state_root = state.update_tree_hash_cache()?;
    beacon_chain.store.put_state(&advanced_state_root, &state)?;

    debug!(
        log,
        "Completed state advance";
        "head_block_root" => ?head_block_root,
        "advanced_slot" => final_slot,
        "initial_slot" => initial_slot,
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock() {
        let lock = Lock::new();
        assert!(!lock.lock());
        assert!(lock.lock());
        assert!(lock.lock());
        lock.unlock();
        assert!(!lock.lock());
        assert!(lock.lock());
    }
}
