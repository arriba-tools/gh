//! Communication with GitHub: the authenticated REST and GraphQL client and the helpers that
//! shape its requests and read its responses.

pub mod actions;
pub mod attachments;
mod client;
pub mod dependabot;
pub mod gists;
pub mod notifications;
pub mod pulls;
pub mod releases;
mod repo;

pub use client::{Client, encode_path, encode_segment, is_failed};
pub use repo::{PrRef, Repo, default_branch, parse_pr_ref};
