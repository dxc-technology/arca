//! Phase 28 — Replication.
//!
//! The replicator forwards objects from a local bucket to a remote
//! S3-compatible destination asynchronously, driven by a DB-backed change
//! journal so retries survive restarts and destination outages.
//!
//! Wiring summary (see plan `Phase 28 — Replication`):
//! - Handlers emit journal entries via [`emit`] after successful metadata
//!   writes (see `crates/arca-proto/src/handlers/object.rs`).
//! - The worker (spawned from `main.rs`) periodically claims a batch, delivers
//!   via the outbound S3 client, and updates each entry's status.
//! - Loop prevention: every outbound request carries
//!   [`REPLICATION_SOURCE_HEADER`]. The inbound handler detects the header,
//!   flags the object `REPLICA`, and skips journal emission.

pub mod client;
pub mod worker;

pub use worker::spawn_replication_worker;
