// SPDX-License-Identifier: MIT
//! # XLM Price Prediction Market
//!
//! Secure Soroban-based prediction market for XLM price movements.
//! Users bet on price direction (UP/DOWN) using virtual XLM tokens
//!
//! ## Key Features
//! - Role-based access control (Admin, Oracle, Users)
//! - Checked arithmetic prevents overflow
//! - Proportional payout distribution
//! - Comprehensive error handling

#![no_std]
extern crate alloc;

#[cfg(test)]
extern crate std;

mod access_control;
mod admin;
mod betting;
pub mod collateral;
pub mod common;
mod config;
mod contract;
mod errors;
mod governance;
mod insurance;
mod leaderboard;
mod math_common;
pub mod oracle_committee;
mod queries;
mod settlement;
mod settlement_math;
mod storage;
mod types;

// Isolated Phase 1 scaffolding. This feature is disabled by default and does
// not alter the synchronous settlement path or default contract interface.
// Its public contract wrappers stay out of `#[contractimpl]`: the resolved
// Soroban macro emits references to individually `cfg`-removed methods in the
// default build. POC functions are therefore internal scaffolding for now.
#[cfg(feature = "progressive-settlement-poc")]
pub mod progressive_settlement_poc;

#[cfg(test)]
mod tests;

pub use contract::VirtualTokenContract;
pub use errors::ContractError;
pub use types::{
    ArchivedRoundSummary, BetSide, ConfigChangeKind, ConfigChangePayload, DataKeyCore,
    DataKeyScoped, InsuranceEvent, LeaderboardEntry, OracleRotationProposal, PendingConfigChange,
    PrecisionCommitment, PrecisionPrediction, ProtocolHealthStatus, Round, RoundArchiveStatus,
    RoundTemplate, SeasonArchive, SeasonLeaderboardEntry, UserPosition, UserStats,
    CANCEL_REASON_FALLBACK_REFUND, CANCEL_REASON_GENERIC, CANCEL_REASON_ORACLE_DEVIATION,
    CANCEL_REASON_ORACLE_OUTAGE,
};
