//! Shared cluster state (Phase 29 High Availability).
//!
//! A dependency-light, observable view of the cluster shared between:
//! - the membership manager (`arca-server`) which discovers peers via mDNS /
//!   seeds and refreshes this state from health checks;
//! - request handlers (`arca-proto`) which expose it via `/cluster/v1/health`,
//!   `/admin/cluster`, and the console dashboard;
//! - the cluster store decorators (`arca-server`) which read it for the write
//!   quorum gate.
//!
//! It deliberately has no networking/mDNS dependency: it is a plain shared
//! snapshot that the membership manager updates. This is why it lives in
//! `arca-core` (which `arca-proto` and `arca-server` both depend on) rather
//! than in `arca-server` (which `arca-proto` cannot see).

use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{BucketInfo, Credential, User};

/// Fixed access-key id of the shared cluster credential. The matching secret is
/// `[cluster].secret`; every inter-node `/cluster/v1/*` request is signed and
/// verified with this credential, so no per-node credential is ever stored.
pub const CLUSTER_ACCESS_KEY: &str = "arca-cluster";

/// SigV4 credential-scope region used for inter-node requests. Arbitrary but
/// fixed: signer and verifier agree because the verifier reads the region back
/// from the request's own credential scope.
pub const CLUSTER_REGION: &str = "arca";

/// Header carrying the blob sidecar (base64-encoded JSON) on
/// `PUT /cluster/v1/blob/{id}` requests and `GET` responses. It is part of the
/// signed header set, so the wrapped DEK an encrypted sidecar may contain
/// cannot be tampered with in transit.
pub const CLUSTER_SIDECAR_HEADER: &str = "x-arca-sidecar";

/// Body of `POST /cluster/v1/object/delete`: a replicated hard-delete of a
/// single object version. `version_id == "null"` targets the null-version row.
/// Shared contract between the cluster client (sender) and the receive handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterVersionDelete {
    pub bucket: String,
    pub key: String,
    pub version_id: String,
}

/// Body of `POST /cluster/v1/op`: a replicated control-plane mutation applied
/// idempotently by the receiving node. Shared contract between the cluster
/// client (sender) and the receive handler.
///
/// This chunk covers the bucket family (so replicated objects become servable
/// on peers). Identity entities (credentials, users, teams, grants,
/// server_config) reuse this same `/op` channel in a follow-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlOp {
    /// Create or replace a bucket row verbatim.
    BucketUpsert { info: BucketInfo },
    /// Delete a bucket (cascades its tags/config locally).
    BucketDelete { name: String },
    /// Set a single bucket-config key (e.g. versioning, encryption).
    BucketConfigSet {
        bucket: String,
        key: String,
        value: String,
    },
    /// Delete a single bucket-config key.
    BucketConfigDelete { bucket: String, key: String },
    /// Replace all tags on a bucket (an empty list clears them).
    BucketTags {
        bucket: String,
        tags: Vec<(String, String)>,
    },
    /// Create or replace a credential verbatim (so failover auth recognizes the
    /// access key).
    CredentialUpsert { credential: Credential },
    /// Delete a credential by access key id.
    CredentialDelete { access_key_id: String },
    /// Create or replace a user verbatim.
    UserUpsert { user: User },
    /// Delete a user by id.
    UserDelete { user_id: String },
}

/// A peer node as currently seen by this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerNode {
    /// Stable, self-assigned identity of the peer.
    pub node_id: String,
    /// Base URL used to reach the peer (e.g. "https://10.0.0.2:9000").
    pub endpoint: String,
    /// Whether the most recent health check succeeded.
    pub alive: bool,
    /// Timestamp of the last successful health contact, if any.
    pub last_seen: Option<DateTime<Utc>>,
}

/// Serializable point-in-time view of the cluster, for the admin API / console.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterSnapshot {
    pub node_id: String,
    /// Durable copies (including self) required to ACK a write. `null` =
    /// "available" mode (W=1).
    pub write_quorum: Option<u32>,
    /// Whether writes can currently be acknowledged.
    pub has_write_quorum: bool,
    /// Live nodes reachable for a write, including this node.
    pub live_node_count: usize,
    /// All known peers (alive or not).
    pub peers: Vec<PeerNode>,
}

/// Shared, mutable view of the cluster from this node's perspective.
///
/// Constructed once at startup, wrapped in `Arc`, and shared. The membership
/// manager replaces the peer list as discovery/health evolve; readers take a
/// cheap snapshot.
pub struct ClusterState {
    node_id: String,
    /// Durable copies (including self) required to ACK a write. `None` means
    /// "available" mode: any single node may ACK (W=1).
    write_quorum: Option<u32>,
    /// Currently known peers (excluding self).
    peers: RwLock<Vec<PeerNode>>,
}

impl ClusterState {
    /// Creates cluster state for this node. `write_quorum` is the majority
    /// threshold in quorum mode, or `None` in available mode.
    pub fn new(node_id: impl Into<String>, write_quorum: Option<u32>) -> Self {
        Self {
            node_id: node_id.into(),
            write_quorum,
            peers: RwLock::new(Vec::new()),
        }
    }

    /// This node's stable identity.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The configured write quorum (majority), or `None` in available mode.
    pub fn write_quorum(&self) -> Option<u32> {
        self.write_quorum
    }

    /// Replaces the known peer set (called by the membership manager).
    pub fn set_peers(&self, peers: Vec<PeerNode>) {
        *self.peers.write().expect("cluster peers lock poisoned") = peers;
    }

    /// A clone of all known peers (alive or not).
    pub fn peers(&self) -> Vec<PeerNode> {
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .clone()
    }

    /// Number of peers currently considered alive (excluding this node).
    pub fn live_peer_count(&self) -> usize {
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .iter()
            .filter(|p| p.alive)
            .count()
    }

    /// Total nodes currently reachable for a write, including this node.
    pub fn live_node_count(&self) -> usize {
        self.live_peer_count() + 1
    }

    /// Whether a write can currently be acknowledged under the configured
    /// consistency policy.
    ///
    /// - Available mode (`write_quorum == None`): always `true` (W=1, self).
    /// - Quorum mode: `true` when live nodes (peers + self) meet the majority.
    pub fn has_write_quorum(&self) -> bool {
        match self.write_quorum {
            None => true,
            Some(q) => self.live_node_count() >= q as usize,
        }
    }

    /// A serializable snapshot for the admin API / console dashboard.
    pub fn snapshot(&self) -> ClusterSnapshot {
        let peers = self.peers();
        let live_node_count = peers.iter().filter(|p| p.alive).count() + 1;
        let has_write_quorum = match self.write_quorum {
            None => true,
            Some(q) => live_node_count >= q as usize,
        };
        ClusterSnapshot {
            node_id: self.node_id.clone(),
            write_quorum: self.write_quorum,
            has_write_quorum,
            live_node_count,
            peers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(node_id: &str, alive: bool) -> PeerNode {
        PeerNode {
            node_id: node_id.to_string(),
            endpoint: format!("https://{node_id}:9000"),
            alive,
            last_seen: None,
        }
    }

    #[test]
    fn available_mode_always_has_quorum() {
        let state = ClusterState::new("self", None);
        // No peers, alone: still writable in available mode.
        assert!(state.has_write_quorum());
        assert_eq!(state.live_node_count(), 1);
    }

    #[test]
    fn quorum_mode_needs_majority() {
        // cluster_size = 3 → write_quorum = 2.
        let state = ClusterState::new("self", Some(2));
        // Alone (self only) → 1 < 2 → no quorum (read-only).
        assert!(!state.has_write_quorum());
        // One live peer → self + 1 = 2 ≥ 2 → quorum.
        state.set_peers(vec![peer("n2", true)]);
        assert!(state.has_write_quorum());
        // The peer dies → back to 1 → no quorum.
        state.set_peers(vec![peer("n2", false)]);
        assert!(!state.has_write_quorum());
        assert_eq!(state.live_peer_count(), 0);
    }

    #[test]
    fn snapshot_reports_state() {
        let state = ClusterState::new("self", Some(2));
        state.set_peers(vec![peer("n2", true), peer("n3", false)]);
        let snap = state.snapshot();
        assert_eq!(snap.node_id, "self");
        assert_eq!(snap.write_quorum, Some(2));
        assert_eq!(snap.peers.len(), 2);
        assert_eq!(snap.live_node_count, 2); // self + n2
        assert!(snap.has_write_quorum);
    }

    #[test]
    fn peers_roundtrip() {
        let state = ClusterState::new("self", None);
        assert!(state.peers().is_empty());
        state.set_peers(vec![peer("n2", true)]);
        assert_eq!(state.peers().len(), 1);
        assert_eq!(state.peers()[0].node_id, "n2");
    }

    #[test]
    fn control_op_serde_roundtrip() {
        let ops = vec![
            ControlOp::BucketUpsert {
                info: BucketInfo {
                    name: "b".to_string(),
                    created_at: Utc::now(),
                    owner: "root".to_string(),
                },
            },
            ControlOp::BucketDelete {
                name: "b".to_string(),
            },
            ControlOp::BucketConfigSet {
                bucket: "b".to_string(),
                key: "versioning".to_string(),
                value: "Enabled".to_string(),
            },
            ControlOp::BucketConfigDelete {
                bucket: "b".to_string(),
                key: "versioning".to_string(),
            },
            ControlOp::BucketTags {
                bucket: "b".to_string(),
                tags: vec![("k".to_string(), "v".to_string())],
            },
            ControlOp::CredentialUpsert {
                credential: Credential {
                    access_key_id: "AK".to_string(),
                    secret_access_key: "sk".to_string(),
                    description: "d".to_string(),
                    created_at: Utc::now(),
                    active: true,
                    admin: false,
                    user_id: "u1".to_string(),
                },
            },
            ControlOp::CredentialDelete {
                access_key_id: "AK".to_string(),
            },
            ControlOp::UserUpsert {
                user: User {
                    user_id: "u1".to_string(),
                    username: "alice".to_string(),
                    description: String::new(),
                    is_root: false,
                    created_at: Utc::now(),
                },
            },
            ControlOp::UserDelete {
                user_id: "u1".to_string(),
            },
        ];
        for op in &ops {
            let json = serde_json::to_string(op).unwrap();
            // The tagged enum carries a "kind" discriminant.
            assert!(json.contains("\"kind\""), "missing kind tag in {json}");
            let back: ControlOp = serde_json::from_str(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                json,
                "round-trip mismatch for {json}"
            );
        }
    }
}
