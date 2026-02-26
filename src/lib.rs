#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::struct_excessive_bools)]

pub mod cli;
pub mod config;
pub mod deploy_tracker;
pub mod orphan;
pub mod reconcile;
pub mod remotes;
pub mod store;
pub mod suggest;
pub mod sync_tracker;
pub mod targets;
#[cfg(test)]
pub mod test_support;
pub mod ui;
pub mod validate;
#[cfg(feature = "mcp")]
pub mod mcp;
