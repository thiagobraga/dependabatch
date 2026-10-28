//! dependabatch core: detect, parse and analyze open dependabot pull requests.
//!
//! The library is surface agnostic: the CLI, the GitHub Action, the web API and
//! the Tauri desktop app all reuse these pieces.

pub mod client;
pub mod conflict;
pub mod pr;
pub mod report;

pub use pr::{DepBump, Hunk, PrFile, PullRequest, Update, parse_update};
