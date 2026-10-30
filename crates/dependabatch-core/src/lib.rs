//! dependabatch core: detect, parse and analyze open dependabot pull requests.
//!
//! The library is surface agnostic: the CLI, the GitHub Action, the web API and
//! the Tauri desktop app all reuse these pieces.

pub mod alert_fix;
pub mod alerts;
pub mod batch;
pub mod client;
pub mod conflict;
mod container_fix;
pub mod manifest;
pub mod plan;
pub mod pr;
pub mod regen;
pub mod report;

pub use pr::{DepBump, Hunk, PrFile, PullRequest, Update, parse_update};
