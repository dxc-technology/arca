//! High Availability clustering (Phase 29).
//!
//! A symmetric, self-configuring cluster: every node runs with a byte-identical
//! TOML config, derives its own stable identity, discovers peers automatically,
//! and fully replicates both the data plane (objects) and the control plane
//! (credentials, users, teams, grants, server config, bucket metadata).
//!
//! Submodules are added milestone by milestone (see the Phase 29 plan):
//! - [`identity`] — self-assigned, persisted node identity.
//! - [`membership`] — peer discovery (mDNS / static / dns) + health pings.
//! - [`status`] — `arca cluster status` CLI output.
//! - [`client`] — signed inter-node transport to peers' `/cluster/v1/*`.
//! - [`cluster_blob`] / [`cluster_meta`] — M3 write-path decorators that
//!   replicate blobs and object rows to peers under the consistency policy.

pub mod client;
pub mod cluster_blob;
pub mod cluster_control;
pub mod cluster_meta;
pub mod identity;
pub mod membership;
pub mod status;
