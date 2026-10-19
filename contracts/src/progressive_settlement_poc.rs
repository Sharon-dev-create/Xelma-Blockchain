// SPDX-License-Identifier: MIT
//! Feature-gated Phase 1 scaffolding for progressive settlement.
//!
//! This module freezes the existing dispute result and snapshots participant
//! inputs in bounded batches. It deliberately does not calculate or apply
//! payouts; production settlement remains synchronous. The feature exists only
//! on the isolated proof-of-concept branch and is disabled by default.

use soroban_sdk::{contracttype, Address, Env, Vec};

use crate::common::{
    _extend_persistent_ttl, _extend_written_persistent_ttl, sort_addresses,
    DEFAULT_ARCHIVE_RETENTION,
};
use crate::errors::ContractError;
use crate::leaderboard;
use crate::types::{
    DataKeyCore, DataKeyScoped, FeeModel, PrecisionCommitment, PrecisionPrediction, Round,
    RoundMode, UserPosition,
};

pub const POC_MAX_PARTICIPANTS: u32 = 100;
pub const POC_PAGE_SIZE: u32 = 16;
const POC_ALGORITHM_VERSION: u32 = 1;

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
enum PocKey {
    ActiveRound,
    State(u64),
    AddressPage(u64, u32),
    ParticipantInput(u64, u32),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum PocPhase {
    Snapshotting = 0,
    SnapshotReady = 1,
    Aborted = 2,
}

/// Frozen inputs and bounded progress for the Phase 1 prototype.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PocSettlementState {
    pub algorithm_version: u32,
    pub phase: PocPhase,
    pub round: Round,
    pub final_price: u128,
    pub confidence: Option<u32>,
    pub season_id: u32,
    pub fee_bps: Option<u32>,
    pub fee_model: FeeModel,
    pub precision_payout_policy: u32,
    pub max_pending_winnings: Option<i128>,
    pub archive_retention: u32,
    pub participant_count: u32,
    pub snapshot_cursor: u32,
    pub started_at_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum PocParticipantInput {
    UpDown(Address, Option<UserPosition>),
    Precision(
        Address,
        Option<PrecisionPrediction>,
        Option<PrecisionCommitment>,
    ),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PocProgress {
    pub round_id: u64,
    pub phase: PocPhase,
    pub participant_count: u32,
    pub snapshot_cursor: u32,
    pub started_at_ledger: u32,
}

fn page_count(item_count: u32) -> u32 {
    if item_count == 0 {
        0
    } else {
        (item_count + POC_PAGE_SIZE - 1) / POC_PAGE_SIZE
    }
}

fn state_key(round_id: u64) -> PocKey {
    PocKey::State(round_id)
}

fn read_state(env: &Env, round_id: u64) -> Option<PocSettlementState> {
    let key = state_key(round_id);
    _extend_persistent_ttl(env, &key);
    env.storage().persistent().get(&key)
}

/// Used by existing mutating paths when the POC has acquired its global lock.
/// With the feature disabled, none of those paths call this helper.
pub(crate) fn ensure_not_locked(env: &Env) -> Result<(), ContractError> {
    let key = PocKey::ActiveRound;
    _extend_persistent_ttl(env, &key);
    if env.storage().persistent().has(&key) {
        return Err(ContractError::IllegalPhaseTransition);
    }
    Ok(())
}

fn frozen_config(env: &Env) -> (Option<u32>, FeeModel, u32, Option<i128>, u32) {
    let fee_bps = crate::config::_read_protocol_fee_bps(env);
    let fee_model = crate::config::_read_fee_model(env);
    let precision_policy = env
        .storage()
        .persistent()
        .get(&DataKeyCore::PrecisionPayoutPolicy)
        .unwrap_or(0);
    let max_pending = env
        .storage()
        .persistent()
        .get(&DataKeyCore::MaxPendingWinnings);
    let archive_retention = env
        .storage()
        .persistent()
        .get(&DataKeyCore::ArchiveRetention)
        .unwrap_or(DEFAULT_ARCHIVE_RETENTION);
    (
        fee_bps,
        fee_model,
        precision_policy,
        max_pending,
        archive_retention,
    )
}

/// Starts an isolated snapshot from an expired, oracle-resolved dispute.
/// Admin authorization is intentional for the POC: the snapshot-only phase
/// has not yet implemented permissionless keeper incentives or finalization.
pub fn begin_snapshot(env: Env, round_id: u64) -> Result<PocProgress, ContractError> {
    let admin: Address = env
        .storage()
        .persistent()
        .get(&DataKeyCore::Admin)
        .ok_or(ContractError::AdminNotSet)?;
    admin.require_auth();

    let active_key = PocKey::ActiveRound;
    if env.storage().persistent().has(&active_key) {
        return Err(ContractError::IllegalPhaseTransition);
    }
    if let Some(existing) = read_state(&env, round_id) {
        if existing.phase != PocPhase::Aborted {
            return Err(ContractError::IllegalPhaseTransition);
        }
    }

    let pending = crate::settlement::_read_pending_dispute(&env, round_id)
        .ok_or(ContractError::RoundNotCancellable)?;
    if env.ledger().sequence() < pending.deadline_ledger {
        return Err(ContractError::RoundNotEnded);
    }

    let participants: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKeyScoped::RoundParticipants(round_id))
        .unwrap_or(Vec::new(&env));
    if participants.len() > POC_MAX_PARTICIPANTS {
        return Err(ContractError::PageSizeExceeded);
    }
    let sorted_participants = sort_addresses(participants);

    // Keep the pending dispute and source inputs alive while snapshot pages
    // are copied. The participant cap makes this start-time touch bounded.
    crate::settlement::_poc_extend_pending_disputes_ttl(&env);
    for user in sorted_participants.iter() {
        match &pending.round.mode {
            RoundMode::UpDown => {
                _extend_persistent_ttl(&env, &DataKeyScoped::Position(round_id, user.clone()));
            }
            RoundMode::Precision => {
                _extend_persistent_ttl(
                    &env,
                    &DataKeyScoped::PrecisionPosition(round_id, user.clone()),
                );
                _extend_persistent_ttl(
                    &env,
                    &DataKeyScoped::PrecisionCommitment(round_id, user.clone()),
                );
            }
        }
    }

    for page_id in 0..page_count(sorted_participants.len()) {
        let start = page_id * POC_PAGE_SIZE;
        let end = core::cmp::min(start + POC_PAGE_SIZE, sorted_participants.len());
        let mut page = Vec::new(&env);
        for index in start..end {
            if let Some(user) = sorted_participants.get(index) {
                page.push_back(user);
            }
        }
        let page_key = PocKey::AddressPage(round_id, page_id);
        env.storage().persistent().set(&page_key, &page);
        _extend_written_persistent_ttl(&env, &page_key);
    }

    let (fee_bps, fee_model, precision_policy, max_pending, archive_retention) =
        frozen_config(&env);
    let state = PocSettlementState {
        algorithm_version: POC_ALGORITHM_VERSION,
        phase: PocPhase::Snapshotting,
        round: pending.round,
        final_price: pending.final_price,
        confidence: pending.confidence,
        season_id: leaderboard::_current_season_id(&env),
        fee_bps,
        fee_model,
        precision_payout_policy: precision_policy,
        max_pending_winnings: max_pending,
        archive_retention,
        participant_count: sorted_participants.len(),
        snapshot_cursor: 0,
        started_at_ledger: env.ledger().sequence(),
    };
    let key = state_key(round_id);
    env.storage().persistent().set(&key, &state);
    _extend_written_persistent_ttl(&env, &key);
    env.storage().persistent().set(&active_key, &round_id);
    _extend_written_persistent_ttl(&env, &active_key);

    Ok(progress_from_state(round_id, &state))
}

/// Copies at most `POC_PAGE_SIZE` immutable participant inputs per call.
pub fn snapshot_batch(env: Env, round_id: u64, limit: u32) -> Result<PocProgress, ContractError> {
    if limit == 0 || limit > POC_PAGE_SIZE {
        return Err(ContractError::PageSizeExceeded);
    }
    crate::settlement::_poc_extend_pending_disputes_ttl(&env);
    _extend_persistent_ttl(&env, &PocKey::ActiveRound);
    let active: Option<u64> = env.storage().persistent().get(&PocKey::ActiveRound);
    if active != Some(round_id) {
        return Err(ContractError::IllegalPhaseTransition);
    }
    let mut state = read_state(&env, round_id).ok_or(ContractError::RoundNotCancellable)?;
    if state.phase != PocPhase::Snapshotting {
        return Err(ContractError::IllegalPhaseTransition);
    }

    let end = core::cmp::min(
        state.snapshot_cursor.saturating_add(limit),
        state.participant_count,
    );
    while state.snapshot_cursor < end {
        let index = state.snapshot_cursor;
        let address_page = index / POC_PAGE_SIZE;
        let address_offset = index % POC_PAGE_SIZE;
        let page_key = PocKey::AddressPage(round_id, address_page);
        _extend_persistent_ttl(&env, &page_key);
        let page: Vec<Address> = env
            .storage()
            .persistent()
            .get(&page_key)
            .ok_or(ContractError::IllegalPhaseTransition)?;
        let user = page
            .get(address_offset)
            .ok_or(ContractError::IllegalPhaseTransition)?;

        let input = match &state.round.mode {
            RoundMode::UpDown => PocParticipantInput::UpDown(
                user.clone(),
                env.storage()
                    .persistent()
                    .get(&DataKeyScoped::Position(round_id, user.clone())),
            ),
            RoundMode::Precision => PocParticipantInput::Precision(
                user.clone(),
                env.storage()
                    .persistent()
                    .get(&DataKeyScoped::PrecisionPosition(round_id, user.clone())),
                env.storage()
                    .persistent()
                    .get(&DataKeyScoped::PrecisionCommitment(round_id, user.clone())),
            ),
        };
        let input_key = PocKey::ParticipantInput(round_id, index);
        env.storage().persistent().set(&input_key, &input);
        _extend_written_persistent_ttl(&env, &input_key);
        state.snapshot_cursor += 1;
    }

    if state.snapshot_cursor == state.participant_count {
        state.phase = PocPhase::SnapshotReady;
    }
    let key = state_key(round_id);
    env.storage().persistent().set(&key, &state);
    _extend_written_persistent_ttl(&env, &key);
    Ok(progress_from_state(round_id, &state))
}

/// Returns the persisted snapshot progress without exposing settlement results.
pub fn get_progress(env: Env, round_id: u64) -> Option<PocProgress> {
    read_state(&env, round_id).map(|state| progress_from_state(round_id, &state))
}

/// Releases a snapshot-only lock. This is safe only because Phase 1 performs
/// no payouts, statistics, outcomes, events, or other irreversible effects.
pub fn abort_snapshot(env: Env, round_id: u64) -> Result<(), ContractError> {
    let admin: Address = env
        .storage()
        .persistent()
        .get(&DataKeyCore::Admin)
        .ok_or(ContractError::AdminNotSet)?;
    admin.require_auth();
    let mut state = read_state(&env, round_id).ok_or(ContractError::RoundNotCancellable)?;
    if !matches!(
        state.phase,
        PocPhase::Snapshotting | PocPhase::SnapshotReady
    ) {
        return Err(ContractError::IllegalPhaseTransition);
    }
    state.phase = PocPhase::Aborted;
    let state_storage_key = state_key(round_id);
    env.storage().persistent().set(&state_storage_key, &state);
    _extend_written_persistent_ttl(&env, &state_storage_key);

    let active_key = PocKey::ActiveRound;
    let active: Option<u64> = env.storage().persistent().get(&active_key);
    if active == Some(round_id) {
        env.storage().persistent().remove(&active_key);
    }
    Ok(())
}

fn progress_from_state(round_id: u64, state: &PocSettlementState) -> PocProgress {
    PocProgress {
        round_id,
        phase: state.phase.clone(),
        participant_count: state.participant_count,
        snapshot_cursor: state.snapshot_cursor,
        started_at_ledger: state.started_at_ledger,
    }
}
