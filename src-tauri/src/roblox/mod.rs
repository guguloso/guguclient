//! Roblox web API access (replaces `rozod` + `RobloxApiClient` from Electron).

mod client;
pub mod endpoints;

pub use client::{Challenge, ChallengeOutcome, ReplayRequest, RobloxClient};
