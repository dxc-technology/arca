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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::control_tombstone::{
    ControlTombstone, TOMBSTONE_BUCKET, TOMBSTONE_BUCKET_CONFIG, TOMBSTONE_BUCKET_TAGS,
    TOMBSTONE_CREDENTIAL, TOMBSTONE_GRANT, TOMBSTONE_MULTIPART, TOMBSTONE_SERVER_CONFIG,
    TOMBSTONE_TEAM, TOMBSTONE_TEAM_GRANT, TOMBSTONE_TEAM_MEMBER, TOMBSTONE_USER,
    TOMBSTONE_USER_GRANT,
};
use crate::types::{
    BlobId, BucketInfo, Credential, Grant, MultipartUploadRecord, ObjectRecord, PartRecord, Team,
    User,
};

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

/// Header carrying the prober's fresh challenge nonce on
/// `GET /cluster/v1/ping` requests (decision H12). Part of the signed header
/// set; the peer answers with [`ping_nonce_mac`] over it, proving possession of
/// the cluster secret to the prober.
pub const CLUSTER_PING_NONCE_HEADER: &str = "x-arca-cluster-nonce";

/// `server_config` key holding this node's persistent cluster identity
/// (loop-prevention source id). Written once at first start, never replicated.
pub const NODE_ID_KEY: &str = "node_id";

/// Returns true for `server_config` keys that are NODE-LOCAL and must never be
/// replicated to peers — currently only [`NODE_ID_KEY`]: replicating it would
/// overwrite a peer's own identity. Every other setting (region, retention
/// windows, log level, preview limits, lifecycle interval) is cluster-wide.
/// Applied on BOTH sides — the sender's fan-out skips these keys, and the
/// receive handler drops them (D12.1) — so one buggy or older peer cannot
/// rewrite another node's identity.
pub fn is_node_local_server_config_key(key: &str) -> bool {
    key == NODE_ID_KEY
}

/// Composite tombstone/merge key for the two-part control families
/// (`user_grant` = user:grant, `team_grant` = team:grant, `team_member` =
/// team:user, `bucket_config` = bucket:config_key, `parts` =
/// upload_id:part_number). The `:` separator is unambiguous here: ids are
/// server-generated (UUID-shaped), bucket names follow S3 naming (no colon)
/// and config keys are fixed identifiers. Shared by the tombstone writers and
/// [`plan_control_merge`] so both sides always encode the same way.
pub fn pair_key(a: &str, b: &str) -> String {
    format!("{a}:{b}")
}

/// Domain-separation prefix for the ping challenge MAC, so this HMAC use of the
/// cluster secret can never collide with another (e.g. SigV4 key derivation).
const PING_MAC_CONTEXT: &[u8] = b"arca-cluster-ping-v1";

/// Computes the challenge-response MAC a pinged node returns: hex-encoded
/// HMAC-SHA256 of the prober's nonce, keyed by the shared cluster secret
/// (decision H12). A correct MAC over a FRESH nonce proves the responder holds
/// the secret — merely answering 200 proves nothing (a rogue controls its own
/// server), and a recorded MAC cannot be replayed against a new nonce.
pub fn ping_nonce_mac(secret: &str, nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(PING_MAC_CONTEXT);
    mac.update(b"\x1f");
    mac.update(nonce.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verifies a peer's challenge-response MAC against the nonce we sent it.
/// Constant-time on the MAC comparison (via `Mac::verify_slice`); any decode
/// failure or mismatch is simply "not authenticated".
pub fn verify_ping_nonce_mac(secret: &str, nonce: &str, mac_hex: &str) -> bool {
    let Ok(received) = hex::decode(mac_hex) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(PING_MAC_CONTEXT);
    mac.update(b"\x1f");
    mac.update(nonce.as_bytes());
    mac.verify_slice(&received).is_ok()
}

/// Response of the authenticated `GET /cluster/v1/ping` (decision H12, review
/// §3.5): the peer-facing identity/health detail that used to live on the
/// public `/cluster/v1/health`. Only an authenticated peer (signed request)
/// can read it, and `nonce_mac` proves the responder's own possession of the
/// secret to the prober. `max_seq` reports the node's object write cursor for
/// restore/rewind detection (D3c). Shared contract between the ping handler
/// (producer) and the membership prober (consumer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPingResponse {
    pub status: String,
    pub node_id: String,
    /// Fingerprint of the cluster-alignment-critical config (drift detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_fingerprint: Option<String>,
    /// This node's total disk capacity (bytes); peers track the cluster minimum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total: Option<u64>,
    /// This node's available disk space (bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_available: Option<u64>,
    /// Highest node-local object `seq` assigned so far (0 = none yet).
    #[serde(default)]
    pub max_seq: u64,
    /// [`ping_nonce_mac`] over the request's [`CLUSTER_PING_NONCE_HEADER`];
    /// absent when the request carried no nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_mac: Option<String>,
}

/// Computes a fingerprint of the cluster-alignment-critical configuration —
/// the fields that MUST be identical on every node for the cluster to work:
/// `cluster_id`, the consistency contract (`mode` + effective write quorum), the
/// shared `secret`, and the encryption master-key id. Nodes exchange this hash
/// (via `/cluster/v1/health`) and flag any peer whose value differs, catching
/// the silent misconfigurations: a wrong `secret` (replication 403s while the
/// node still looks alive) or a different master key (encrypted blobs unreadable
/// on the peer). Sensitive inputs (the secret) only feed the one-way hash; the
/// output reveals nothing.
pub fn config_fingerprint(
    cluster_id: &str,
    mode: &str,
    write_quorum: Option<u32>,
    secret: &str,
    master_key_id: Option<&str>,
) -> String {
    let mut h = Sha256::new();
    h.update(b"arca-cluster-cfg-v1");
    for part in [
        cluster_id,
        mode,
        &write_quorum.map(|q| q.to_string()).unwrap_or_default(),
        secret,
        master_key_id.unwrap_or("none"),
    ] {
        h.update(b"\x1f");
        h.update(part.as_bytes());
    }
    let digest = h.finalize();
    // 16 hex chars (8 bytes) is ample to detect drift between a handful of nodes.
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Body of `POST /cluster/v1/object/delete`: a replicated hard-delete of a
/// single object version. `version_id == "null"` targets the null-version row.
/// Shared contract between the cluster client (sender) and the receive handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterVersionDelete {
    pub bucket: String,
    pub key: String,
    pub version_id: String,
}

/// Response of `POST /cluster/v1/object` and `POST /cluster/v1/object/delete`:
/// the receiving peer self-certifies what it durably holds, so the origin can
/// count true replication ACKs for the write quorum (review §2.1, decision H2).
/// Shared contract between the receive handler (producer) and the cluster
/// client (consumer).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClusterObjectAck {
    /// The row was applied (idempotent LWW upsert / delete succeeded).
    pub applied: bool,
    /// The blob the row references is durably present on this peer (its
    /// sidecar exists — for composites, the composite sidecar). Vacuously
    /// `true` when the row references no blob (delete markers, tombstones,
    /// version deletes), so a full ACK is always `applied && has_blob`.
    pub has_blob: bool,
}

/// Whether a replicated write reached its durability quorum (review §2.1,
/// decision H1): `acks` counts the nodes that durably hold the write — the
/// local copy plus every peer that returned a full [`ClusterObjectAck`].
///
/// - Available mode (`write_quorum == None`): always satisfied (W = 1, self).
/// - Quorum mode: satisfied when `acks >= write_quorum`.
///
/// Pure so the ACK-counting semantics are unit-testable in isolation.
pub fn quorum_satisfied(acks: usize, write_quorum: Option<u32>) -> bool {
    match write_quorum {
        None => true,
        Some(q) => acks >= q as usize,
    }
}

/// Error of a [`ClusterAdminProxy`] query (review D6, decision H9).
#[derive(Debug)]
pub enum ClusterProxyError {
    /// The peer answered with a non-2xx HTTP status — its own admin-layer
    /// error (e.g. "audit logging is not enabled" on that node), or a 404
    /// from a pre-R8 peer that has no `/cluster/v1/admin/*` routes yet.
    Http { status: u16, body: String },
    /// The peer could not be reached at all (network error / timeout).
    Unreachable(String),
}

impl std::fmt::Display for ClusterProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http { status, body } => write!(f, "peer returned HTTP {status}: {body}"),
            Self::Unreachable(e) => write!(f, "peer unreachable: {e}"),
        }
    }
}

/// Signed transport for proxying node-local admin queries to a peer (review
/// D6, decision H9): the console's per-node views (audit log, metrics history,
/// notification events, replication journal) reach a specific node THROUGH
/// whichever node the LB picked, via `POST /cluster/v1/admin/*` on the peer —
/// browsers cannot reach cluster nodes directly in the typical deployment
/// (only the LB is exposed), and the cluster credential never leaves the
/// server side.
///
/// Implemented in `arca-server` on top of the cluster transport client (which
/// `arca-proto` cannot see — same dependency rationale as `RawBlobOps`).
#[async_trait::async_trait]
pub trait ClusterAdminProxy: Send + Sync {
    /// POSTs `body` (a JSON-encoded query filter) to `path` (a fixed
    /// `/cluster/v1/admin/*` route) on the peer at `endpoint`, returning the
    /// raw response body bytes on success.
    async fn admin_query(
        &self,
        endpoint: &str,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, ClusterProxyError>;
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
    /// Create or replace a grant (policy) verbatim.
    GrantUpsert { grant: Grant },
    /// Delete a grant by id.
    GrantDelete { grant_id: String },
    /// Attach a grant to a user (idempotent).
    UserGrantAttach { user_id: String, grant_id: String },
    /// Detach a grant from a user.
    UserGrantDetach { user_id: String, grant_id: String },
    /// Attach a grant to a team (idempotent).
    TeamGrantAttach { team_id: String, grant_id: String },
    /// Detach a grant from a team.
    TeamGrantDetach { team_id: String, grant_id: String },
    /// Create or replace a team verbatim.
    TeamUpsert { team: Team },
    /// Delete a team by id.
    TeamDelete { team_id: String },
    /// Add a user to a team (idempotent).
    TeamMemberAdd { team_id: String, user_id: String },
    /// Remove a user from a team.
    TeamMemberRemove { team_id: String, user_id: String },
    /// Set a single server-config key (cluster-wide instance settings). The
    /// sender decorator denylists node-local keys (e.g. `node_id`), so this op
    /// only ever carries cluster-wide settings.
    ServerConfigSet { key: String, value: String },
    /// Delete a single server-config key.
    ServerConfigDelete { key: String },
    /// Replace all tags on a specific object version (an empty list clears
    /// them). `version_id` is the verbatim key the origin used (empty string for
    /// the null version).
    ObjectTags {
        bucket: String,
        key: String,
        version_id: String,
        tags: Vec<(String, String)>,
    },
    /// Create an in-progress multipart upload row (immutable; idempotent upsert).
    MultipartCreate {
        record: MultipartUploadRecord,
    },
    /// Create or replace a part row (idempotent replace by upload_id+part_number).
    PartUpsert { part: PartRecord },
    /// Delete a multipart upload and all its part rows (idempotent).
    MultipartDelete { upload_id: String },
}

/// Request body of `POST /cluster/v1/manifest`: a peer asks for every object
/// row this node has written with a node-local `seq` strictly greater than
/// `since`, up to `limit` rows. Modeled as a POST (not a `GET` with query
/// params) so the request body stays the `UNSIGNED-PAYLOAD` the cluster signing
/// path already uses, sidestepping canonical-query-string signing. Shared
/// contract between the cluster client (sender) and the receive handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterManifestRequest {
    /// Exclusive lower bound: return rows whose `seq` is strictly greater.
    pub since: u64,
    /// Maximum rows to return (the handler additionally clamps it).
    pub limit: u32,
}

/// One entry in a [`ClusterManifest`]: an object row paired with the producing
/// node's local `seq`. The requester applies `record` via
/// [`crate::store::MetadataStore::apply_remote_object`] and advances its cursor
/// to the batch's highest `seq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub seq: u64,
    pub record: ObjectRecord,
}

/// Response body of `POST /cluster/v1/manifest`: the changed-since object rows
/// (ascending `seq`) plus the cursor the requester advances to. When `entries`
/// is empty the requester is caught up and `cursor` echoes the requested
/// `since`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterManifest {
    pub entries: Vec<ManifestEntry>,
    pub cursor: u64,
}

/// A control-plane entity paired with its last-write timestamp, carried in a
/// [`ControlSnapshot`]. The `updated_at` is the LWW key the reconcile pass
/// compares (it is a DB column maintained on write, NOT a field of the inner
/// struct — see migrations sqlite v20 / pg 0007), so it must travel here
/// explicitly and be preserved verbatim on apply (`apply_*_at`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedCredential {
    pub credential: Credential,
    pub updated_at: DateTime<Utc>,
}

/// A user paired with its last-write timestamp. See [`TimestampedCredential`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedUser {
    pub user: User,
    pub updated_at: DateTime<Utc>,
}

/// A team paired with its last-write timestamp. See [`TimestampedCredential`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedTeam {
    pub team: Team,
    pub updated_at: DateTime<Utc>,
}

/// A user↔grant attachment with its last-write timestamp (HA hardening R5,
/// TD-016). The timestamp is a DB column (migration sqlite v22 / pg 0010)
/// refreshed on every attach — including idempotent re-attaches, so a
/// re-attach made while a peer concurrently detached still wins LWW.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedUserGrant {
    pub user_id: String,
    pub grant_id: String,
    pub updated_at: DateTime<Utc>,
}

/// A team↔grant attachment with its last-write timestamp. See
/// [`TimestampedUserGrant`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedTeamGrant {
    pub team_id: String,
    pub grant_id: String,
    pub updated_at: DateTime<Utc>,
}

/// A team membership with its last-write timestamp. See
/// [`TimestampedUserGrant`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedTeamMember {
    pub team_id: String,
    pub user_id: String,
    pub updated_at: DateTime<Utc>,
}

/// A single bucket-config key/value with its last-write timestamp (the
/// `bucket_config.updated_at` column, maintained since the table exists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedBucketConfig {
    pub bucket: String,
    pub key: String,
    pub value: String,
    pub updated_at: DateTime<Utc>,
}

/// A bucket's WHOLE tag set as one LWW entity, matching the replace-all
/// semantics of `PutBucketTagging` / `ControlOp::BucketTags` (per-tag-key LWW
/// could not represent "key removed by a replace"). `updated_at` is the
/// newest row timestamp of the set; an empty set never appears here — clearing
/// the tags records a `bucket_tags` tombstone instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedBucketTags {
    pub bucket: String,
    pub tags: Vec<(String, String)>,
    pub updated_at: DateTime<Utc>,
}

/// A cluster-wide server-config key/value with its last-write timestamp.
/// Node-local keys ([`is_node_local_server_config_key`]) never appear in a
/// snapshot — excluded at build AND ignored on apply (same double defense as
/// the real-time D12.1 filter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedServerConfig {
    pub key: String,
    pub value: String,
    pub updated_at: DateTime<Utc>,
}

/// Full control-plane state of a node, exchanged via `GET
/// /cluster/v1/control-snapshot` and merged last-writer-wins by the reconcile
/// pass (decision 12). Small and bounded (the control plane rarely changes), so
/// shipping the whole thing each cycle is cheap and lets a long-absent node
/// bootstrap past tombstone GC.
///
/// SCOPE: every control-plane family (HA hardening R5 closed TD-016).
/// Identity parents — credentials, users, teams, grants — plus buckets, the
/// attachment/membership joins, bucket config/tags, cluster-wide server config
/// and in-progress multipart uploads with their parts (D4). Grants travel with
/// their struct-level `updated_at`; buckets are create/delete-only and
/// reconcile on `created_at`; multipart uploads on `initiated_at` (immutable
/// rows) and parts on `last_modified`; the remaining families pair the entity
/// with its `updated_at` DB column. Deletions are represented by tombstones
/// for every family except parts (a part disappears only with its upload —
/// parent-dead filtering — or by being replaced under the same key).
///
/// The R5 fields are `#[serde(default)]`: a snapshot from a pre-R5 peer
/// (rolling upgrade, H10) deserializes with the families empty, which the
/// merge treats as "no information" — nothing is deleted on either side.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub credentials: Vec<TimestampedCredential>,
    pub users: Vec<TimestampedUser>,
    pub teams: Vec<TimestampedTeam>,
    pub grants: Vec<Grant>,
    pub buckets: Vec<BucketInfo>,
    pub tombstones: Vec<ControlTombstone>,
    #[serde(default)]
    pub user_grants: Vec<TimestampedUserGrant>,
    #[serde(default)]
    pub team_grants: Vec<TimestampedTeamGrant>,
    #[serde(default)]
    pub team_members: Vec<TimestampedTeamMember>,
    #[serde(default)]
    pub bucket_configs: Vec<TimestampedBucketConfig>,
    #[serde(default)]
    pub bucket_tags: Vec<TimestampedBucketTags>,
    #[serde(default)]
    pub server_configs: Vec<TimestampedServerConfig>,
    #[serde(default)]
    pub multipart_uploads: Vec<MultipartUploadRecord>,
    #[serde(default)]
    pub parts: Vec<PartRecord>,
}

/// The local writes a node must perform to converge with a peer's
/// [`ControlSnapshot`], computed by [`plan_control_merge`]. Pure data: the
/// store applies it (`apply_control_merge`). Upserts carry the winning payload
/// AND its timestamp so the receiver preserves it (no `now()` re-stamp → no
/// flapping).
#[derive(Debug, Default, Clone)]
pub struct ControlMergePlan {
    pub upsert_credentials: Vec<TimestampedCredential>,
    pub upsert_users: Vec<TimestampedUser>,
    pub upsert_teams: Vec<TimestampedTeam>,
    pub upsert_grants: Vec<Grant>,
    pub upsert_buckets: Vec<BucketInfo>,
    pub delete_credentials: Vec<String>,
    pub delete_users: Vec<String>,
    pub delete_teams: Vec<String>,
    pub delete_grants: Vec<String>,
    pub delete_buckets: Vec<String>,
    /// R5 families. Child upserts (joins, bucket config/tags, parts) are
    /// already parent-filtered by [`plan_control_merge`]: an entry whose parent
    /// resolved dead never appears here.
    pub upsert_user_grants: Vec<TimestampedUserGrant>,
    pub upsert_team_grants: Vec<TimestampedTeamGrant>,
    pub upsert_team_members: Vec<TimestampedTeamMember>,
    pub upsert_bucket_configs: Vec<TimestampedBucketConfig>,
    pub upsert_bucket_tags: Vec<TimestampedBucketTags>,
    pub upsert_server_configs: Vec<TimestampedServerConfig>,
    pub upsert_multipart_uploads: Vec<MultipartUploadRecord>,
    pub upsert_parts: Vec<PartRecord>,
    /// Pairs are (user_id, grant_id) / (team_id, grant_id) / (team_id, user_id)
    /// / (bucket, config_key) respectively.
    pub delete_user_grants: Vec<(String, String)>,
    pub delete_team_grants: Vec<(String, String)>,
    pub delete_team_members: Vec<(String, String)>,
    pub delete_bucket_configs: Vec<(String, String)>,
    /// Bucket names whose whole tag set must be cleared.
    pub delete_bucket_tags: Vec<String>,
    pub delete_server_configs: Vec<String>,
    /// Upload ids to delete (cascades the part rows).
    pub delete_multipart_uploads: Vec<String>,
    /// Peer deletions to adopt locally, recorded with the given `deleted_at`.
    pub adopt_tombstones: Vec<ControlTombstone>,
    /// Local tombstones to clear (the entity is alive again, newer somewhere).
    pub clear_tombstones: Vec<ControlTombstone>,
}

impl ControlMergePlan {
    /// True when the merge requires no local writes (the common steady state).
    pub fn is_empty(&self) -> bool {
        self.upsert_credentials.is_empty()
            && self.upsert_users.is_empty()
            && self.upsert_teams.is_empty()
            && self.upsert_grants.is_empty()
            && self.upsert_buckets.is_empty()
            && self.upsert_user_grants.is_empty()
            && self.upsert_team_grants.is_empty()
            && self.upsert_team_members.is_empty()
            && self.upsert_bucket_configs.is_empty()
            && self.upsert_bucket_tags.is_empty()
            && self.upsert_server_configs.is_empty()
            && self.upsert_multipart_uploads.is_empty()
            && self.upsert_parts.is_empty()
            && self.delete_credentials.is_empty()
            && self.delete_users.is_empty()
            && self.delete_teams.is_empty()
            && self.delete_grants.is_empty()
            && self.delete_buckets.is_empty()
            && self.delete_user_grants.is_empty()
            && self.delete_team_grants.is_empty()
            && self.delete_team_members.is_empty()
            && self.delete_bucket_configs.is_empty()
            && self.delete_bucket_tags.is_empty()
            && self.delete_server_configs.is_empty()
            && self.delete_multipart_uploads.is_empty()
            && self.adopt_tombstones.is_empty()
            && self.clear_tombstones.is_empty()
    }
}

/// The local action for a single entity key after last-writer-wins resolution
/// of its alive/dead timestamps across the two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyResolution {
    /// The entity is alive after the merge (kept locally or upserted). Parent
    /// families record this so child upserts (attachments, memberships, bucket
    /// config/tags, parts) can be filtered: adopting a child whose parent
    /// resolved dead would resurrect cascade-deleted rows (and violate the FK
    /// constraints on the PostgreSQL backend).
    alive: bool,
    /// Local should adopt the remote payload (remote's alive ts is strictly newer).
    upsert_from_remote: bool,
    /// Local should delete the live entity (it lost to a newer deletion).
    delete_local: bool,
    /// Record/refresh a local tombstone at this `deleted_at` (the delete won).
    adopt_tombstone_at: Option<DateTime<Utc>>,
    /// Remove the local tombstone (the entity is alive again).
    clear_tombstone: bool,
}

/// Resolves one key from the four event timestamps a node may hold for it: the
/// local/remote "alive" (entity present, `updated_at`) and "dead" (tombstone,
/// `deleted_at`) times. The newest event wins; on an alive/dead tie the entity
/// stays alive (a re-create at the same instant as its delete keeps the entity).
fn resolve_key(
    local_alive: Option<DateTime<Utc>>,
    local_dead: Option<DateTime<Utc>>,
    remote_alive: Option<DateTime<Utc>>,
    remote_dead: Option<DateTime<Utc>>,
) -> KeyResolution {
    let global_alive = [local_alive, remote_alive].into_iter().flatten().max();
    let global_dead = [local_dead, remote_dead].into_iter().flatten().max();

    let dead = match (global_alive, global_dead) {
        (Some(a), Some(d)) => d > a,
        (None, Some(_)) => true,
        _ => false,
    };

    if dead {
        let dead_at = global_dead.expect("dead implies a deletion timestamp");
        KeyResolution {
            alive: false,
            upsert_from_remote: false,
            delete_local: local_alive.is_some(),
            // Record locally if missing or stale.
            adopt_tombstone_at: (local_dead < Some(dead_at)).then_some(dead_at),
            clear_tombstone: false,
        }
    } else {
        // Alive wins. Adopt the remote payload only if its alive ts is strictly
        // newer than ours (ties keep local, avoiding needless churn).
        let upsert_from_remote = match (remote_alive, local_alive) {
            (Some(r), Some(l)) => r > l,
            (Some(_), None) => true,
            _ => false,
        };
        KeyResolution {
            // "Alive" requires an actual live copy somewhere: a key known only
            // through tombstones (alive=None on both sides) is not alive even
            // though the dead-vs-alive comparison did not pick "dead".
            alive: local_alive.is_some() || remote_alive.is_some(),
            upsert_from_remote,
            delete_local: false,
            adopt_tombstone_at: None,
            // A live entity must not keep a stale local tombstone.
            clear_tombstone: local_dead.is_some(),
        }
    }
}

/// Builds a `(entity_key -> deleted_at)` map of a snapshot's tombstones of one
/// type.
fn tombstone_map<'a>(
    tombstones: &'a [ControlTombstone],
    entity_type: &str,
) -> std::collections::HashMap<&'a str, DateTime<Utc>> {
    tombstones
        .iter()
        .filter(|t| t.entity_type == entity_type)
        .map(|t| (t.entity_key.as_str(), t.deleted_at))
        .collect()
}

/// Computes the local writes needed to converge with `remote` (decision 12):
/// last-writer-wins per entity, with deletions represented by tombstones so a
/// peer that still holds a deleted entity cannot resurrect it. Pure: no I/O, so
/// it is exhaustively unit-tested.
///
/// Covers every control-plane family (R5/TD-016): the parent families
/// (credentials, users, teams, grants, buckets, multipart uploads) resolve
/// first and record which keys stay alive; the child families (grant
/// attachments, memberships, bucket config/tags, parts) then skip any upsert
/// whose parent resolved dead. This replaces per-child cascade tombstones: a
/// parent delete cascades its children on every node locally, and the
/// parent-dead filter stops a stale peer's child rows from resurrecting
/// (adopting them would also violate the join-table FK constraints on the
/// PostgreSQL backend).
pub fn plan_control_merge(local: &ControlSnapshot, remote: &ControlSnapshot) -> ControlMergePlan {
    use std::collections::{HashMap, HashSet};
    let mut plan = ControlMergePlan::default();

    // Parent keys alive after the merge, consulted by the child families below.
    let mut alive_users: HashSet<String> = HashSet::new();
    let mut alive_teams: HashSet<String> = HashSet::new();
    let mut alive_grants: HashSet<String> = HashSet::new();
    let mut alive_buckets: HashSet<String> = HashSet::new();
    let mut alive_uploads: HashSet<String> = HashSet::new();

    // -- Credentials --
    {
        let l_alive: HashMap<&str, DateTime<Utc>> = local
            .credentials
            .iter()
            .map(|c| (c.credential.access_key_id.as_str(), c.updated_at))
            .collect();
        let r_alive: HashMap<&str, &TimestampedCredential> = remote
            .credentials
            .iter()
            .map(|c| (c.credential.access_key_id.as_str(), c))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_CREDENTIAL);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_CREDENTIAL);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).copied(),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|c| c.updated_at),
                r_dead.get(key).copied(),
            );
            if res.upsert_from_remote {
                plan.upsert_credentials
                    .push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_credentials.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_CREDENTIAL, key, &res);
        }
    }

    // -- Users --
    {
        let l_alive: HashMap<&str, DateTime<Utc>> = local
            .users
            .iter()
            .map(|u| (u.user.user_id.as_str(), u.updated_at))
            .collect();
        let r_alive: HashMap<&str, &TimestampedUser> = remote
            .users
            .iter()
            .map(|u| (u.user.user_id.as_str(), u))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_USER);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_USER);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).copied(),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|u| u.updated_at),
                r_dead.get(key).copied(),
            );
            if res.alive {
                alive_users.insert(key.to_string());
            }
            if res.upsert_from_remote {
                plan.upsert_users.push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_users.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_USER, key, &res);
        }
    }

    // -- Teams --
    {
        let l_alive: HashMap<&str, DateTime<Utc>> = local
            .teams
            .iter()
            .map(|t| (t.team.team_id.as_str(), t.updated_at))
            .collect();
        let r_alive: HashMap<&str, &TimestampedTeam> = remote
            .teams
            .iter()
            .map(|t| (t.team.team_id.as_str(), t))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_TEAM);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_TEAM);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).copied(),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|t| t.updated_at),
                r_dead.get(key).copied(),
            );
            if res.alive {
                alive_teams.insert(key.to_string());
            }
            if res.upsert_from_remote {
                plan.upsert_teams.push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_teams.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_TEAM, key, &res);
        }
    }

    // -- Grants (updated_at is a struct field) --
    {
        let l_alive: HashMap<&str, DateTime<Utc>> = local
            .grants
            .iter()
            .map(|g| (g.grant_id.as_str(), g.updated_at))
            .collect();
        let r_alive: HashMap<&str, &Grant> =
            remote.grants.iter().map(|g| (g.grant_id.as_str(), g)).collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_GRANT);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_GRANT);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).copied(),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|g| g.updated_at),
                r_dead.get(key).copied(),
            );
            if res.alive {
                alive_grants.insert(key.to_string());
            }
            if res.upsert_from_remote {
                plan.upsert_grants.push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_grants.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_GRANT, key, &res);
        }
    }

    // -- Buckets (create/delete only; created_at is the alive timestamp) --
    {
        let l_alive: HashMap<&str, DateTime<Utc>> = local
            .buckets
            .iter()
            .map(|b| (b.name.as_str(), b.created_at))
            .collect();
        let r_alive: HashMap<&str, &BucketInfo> =
            remote.buckets.iter().map(|b| (b.name.as_str(), b)).collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_BUCKET);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_BUCKET);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).copied(),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|b| b.created_at),
                r_dead.get(key).copied(),
            );
            if res.alive {
                alive_buckets.insert(key.to_string());
            }
            if res.upsert_from_remote {
                plan.upsert_buckets.push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_buckets.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_BUCKET, key, &res);
        }
    }

    // -- User↔grant attachments (R5; child of users AND grants) --
    {
        let l_alive: HashMap<String, &TimestampedUserGrant> = local
            .user_grants
            .iter()
            .map(|x| (pair_key(&x.user_id, &x.grant_id), x))
            .collect();
        let r_alive: HashMap<String, &TimestampedUserGrant> = remote
            .user_grants
            .iter()
            .map(|x| (pair_key(&x.user_id, &x.grant_id), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_USER_GRANT);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_USER_GRANT);
        for key in union_keys_owned(&l_alive, &r_alive, &l_dead, &r_dead) {
            let res = resolve_key(
                l_alive.get(&key).map(|x| x.updated_at),
                l_dead.get(key.as_str()).copied(),
                r_alive.get(&key).map(|x| x.updated_at),
                r_dead.get(key.as_str()).copied(),
            );
            if res.upsert_from_remote {
                let x = *r_alive.get(&key).unwrap();
                if alive_users.contains(&x.user_id) && alive_grants.contains(&x.grant_id) {
                    plan.upsert_user_grants.push(x.clone());
                }
            }
            if res.delete_local {
                let x = *l_alive.get(&key).unwrap();
                plan.delete_user_grants
                    .push((x.user_id.clone(), x.grant_id.clone()));
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_USER_GRANT, &key, &res);
        }
    }

    // -- Team↔grant attachments (R5; child of teams AND grants) --
    {
        let l_alive: HashMap<String, &TimestampedTeamGrant> = local
            .team_grants
            .iter()
            .map(|x| (pair_key(&x.team_id, &x.grant_id), x))
            .collect();
        let r_alive: HashMap<String, &TimestampedTeamGrant> = remote
            .team_grants
            .iter()
            .map(|x| (pair_key(&x.team_id, &x.grant_id), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_TEAM_GRANT);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_TEAM_GRANT);
        for key in union_keys_owned(&l_alive, &r_alive, &l_dead, &r_dead) {
            let res = resolve_key(
                l_alive.get(&key).map(|x| x.updated_at),
                l_dead.get(key.as_str()).copied(),
                r_alive.get(&key).map(|x| x.updated_at),
                r_dead.get(key.as_str()).copied(),
            );
            if res.upsert_from_remote {
                let x = *r_alive.get(&key).unwrap();
                if alive_teams.contains(&x.team_id) && alive_grants.contains(&x.grant_id) {
                    plan.upsert_team_grants.push(x.clone());
                }
            }
            if res.delete_local {
                let x = *l_alive.get(&key).unwrap();
                plan.delete_team_grants
                    .push((x.team_id.clone(), x.grant_id.clone()));
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_TEAM_GRANT, &key, &res);
        }
    }

    // -- Team memberships (R5; child of teams AND users) --
    {
        let l_alive: HashMap<String, &TimestampedTeamMember> = local
            .team_members
            .iter()
            .map(|x| (pair_key(&x.team_id, &x.user_id), x))
            .collect();
        let r_alive: HashMap<String, &TimestampedTeamMember> = remote
            .team_members
            .iter()
            .map(|x| (pair_key(&x.team_id, &x.user_id), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_TEAM_MEMBER);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_TEAM_MEMBER);
        for key in union_keys_owned(&l_alive, &r_alive, &l_dead, &r_dead) {
            let res = resolve_key(
                l_alive.get(&key).map(|x| x.updated_at),
                l_dead.get(key.as_str()).copied(),
                r_alive.get(&key).map(|x| x.updated_at),
                r_dead.get(key.as_str()).copied(),
            );
            if res.upsert_from_remote {
                let x = *r_alive.get(&key).unwrap();
                if alive_teams.contains(&x.team_id) && alive_users.contains(&x.user_id) {
                    plan.upsert_team_members.push(x.clone());
                }
            }
            if res.delete_local {
                let x = *l_alive.get(&key).unwrap();
                plan.delete_team_members
                    .push((x.team_id.clone(), x.user_id.clone()));
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_TEAM_MEMBER, &key, &res);
        }
    }

    // -- Bucket config keys (R5; child of buckets) --
    {
        let l_alive: HashMap<String, &TimestampedBucketConfig> = local
            .bucket_configs
            .iter()
            .map(|x| (pair_key(&x.bucket, &x.key), x))
            .collect();
        let r_alive: HashMap<String, &TimestampedBucketConfig> = remote
            .bucket_configs
            .iter()
            .map(|x| (pair_key(&x.bucket, &x.key), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_BUCKET_CONFIG);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_BUCKET_CONFIG);
        for key in union_keys_owned(&l_alive, &r_alive, &l_dead, &r_dead) {
            let res = resolve_key(
                l_alive.get(&key).map(|x| x.updated_at),
                l_dead.get(key.as_str()).copied(),
                r_alive.get(&key).map(|x| x.updated_at),
                r_dead.get(key.as_str()).copied(),
            );
            if res.upsert_from_remote {
                let x = *r_alive.get(&key).unwrap();
                if alive_buckets.contains(&x.bucket) {
                    plan.upsert_bucket_configs.push(x.clone());
                }
            }
            if res.delete_local {
                let x = *l_alive.get(&key).unwrap();
                plan.delete_bucket_configs
                    .push((x.bucket.clone(), x.key.clone()));
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_BUCKET_CONFIG, &key, &res);
        }
    }

    // -- Bucket tag sets (R5; child of buckets; the whole set is one entity) --
    {
        let l_alive: HashMap<&str, &TimestampedBucketTags> = local
            .bucket_tags
            .iter()
            .map(|x| (x.bucket.as_str(), x))
            .collect();
        let r_alive: HashMap<&str, &TimestampedBucketTags> = remote
            .bucket_tags
            .iter()
            .map(|x| (x.bucket.as_str(), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_BUCKET_TAGS);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_BUCKET_TAGS);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).map(|x| x.updated_at),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|x| x.updated_at),
                r_dead.get(key).copied(),
            );
            if res.upsert_from_remote {
                let x = *r_alive.get(key).unwrap();
                if alive_buckets.contains(&x.bucket) {
                    plan.upsert_bucket_tags.push(x.clone());
                }
            }
            if res.delete_local {
                plan.delete_bucket_tags.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_BUCKET_TAGS, key, &res);
        }
    }

    // -- Cluster-wide server config (R5; no parent). Node-local keys are
    // excluded at snapshot build; filtered again here so even a buggy or
    // malicious snapshot cannot rewrite another node's identity (the same
    // double defense as the real-time D12.1 filter). --
    {
        let l_alive: HashMap<&str, &TimestampedServerConfig> = local
            .server_configs
            .iter()
            .filter(|x| !is_node_local_server_config_key(&x.key))
            .map(|x| (x.key.as_str(), x))
            .collect();
        let r_alive: HashMap<&str, &TimestampedServerConfig> = remote
            .server_configs
            .iter()
            .filter(|x| !is_node_local_server_config_key(&x.key))
            .map(|x| (x.key.as_str(), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_SERVER_CONFIG);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_SERVER_CONFIG);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            if is_node_local_server_config_key(key) {
                continue;
            }
            let res = resolve_key(
                l_alive.get(key).map(|x| x.updated_at),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|x| x.updated_at),
                r_dead.get(key).copied(),
            );
            if res.upsert_from_remote {
                plan.upsert_server_configs
                    .push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_server_configs.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_SERVER_CONFIG, key, &res);
        }
    }

    // -- Multipart uploads (D4; immutable rows keyed by upload_id, alive ts =
    // initiated_at; a Complete/Abort records a `multipart` tombstone so a
    // closed upload cannot resurrect from a peer that missed the close) --
    {
        let l_alive: HashMap<&str, &MultipartUploadRecord> = local
            .multipart_uploads
            .iter()
            .map(|x| (x.upload_id.as_str(), x))
            .collect();
        let r_alive: HashMap<&str, &MultipartUploadRecord> = remote
            .multipart_uploads
            .iter()
            .map(|x| (x.upload_id.as_str(), x))
            .collect();
        let l_dead = tombstone_map(&local.tombstones, TOMBSTONE_MULTIPART);
        let r_dead = tombstone_map(&remote.tombstones, TOMBSTONE_MULTIPART);
        for key in union_keys(&l_alive, &r_alive, &l_dead, &r_dead) {
            let key: &str = &key;
            let res = resolve_key(
                l_alive.get(key).map(|x| x.initiated_at),
                l_dead.get(key).copied(),
                r_alive.get(key).map(|x| x.initiated_at),
                r_dead.get(key).copied(),
            );
            if res.alive {
                alive_uploads.insert(key.to_string());
            }
            if res.upsert_from_remote {
                plan.upsert_multipart_uploads
                    .push((*r_alive.get(key).unwrap()).clone());
            }
            if res.delete_local {
                plan.delete_multipart_uploads.push(key.to_string());
            }
            apply_tombstone_actions(&mut plan, TOMBSTONE_MULTIPART, key, &res);
        }
    }

    // -- Multipart parts (D4; replace-only children of an upload). No
    // tombstones: a part disappears only with its upload (parent-dead filter,
    // delete_multipart_upload cascades the rows) or by being replaced under
    // the same (upload_id, part_number) key. --
    {
        let part_ts =
            |p: &PartRecord| p.last_modified.unwrap_or(DateTime::<Utc>::MIN_UTC);
        let l_alive: HashMap<String, &PartRecord> = local
            .parts
            .iter()
            .map(|x| (pair_key(&x.upload_id, &x.part_number.to_string()), x))
            .collect();
        for x in &remote.parts {
            let key = pair_key(&x.upload_id, &x.part_number.to_string());
            let newer = match l_alive.get(&key) {
                None => true,
                Some(l) => part_ts(x) > part_ts(l),
            };
            if newer && alive_uploads.contains(&x.upload_id) {
                plan.upsert_parts.push(x.clone());
            }
        }
    }

    plan
}

/// Selects on-disk blob files safe to reclaim: those NOT referenced AND older
/// than `grace`. The grace protects freshly-written blobs whose object row has
/// not yet reconciled to this node (it must exceed the max reconcile lag /
/// downtime, like the tombstone grace).
///
/// DATA-LOSS GUARD: the caller MUST build `referenced` as the union of (a) the
/// metadata-referenced blob_ids ([`crate::store::MetadataStore::list_referenced_blob_ids`])
/// and (b) the part blob_ids of every composite sidecar whose composite blob is
/// still metadata-referenced. Composite-completed multipart parts are kept alive
/// only by their composite sidecar, so omitting (b) would delete live parts.
pub fn plan_blob_gc(
    on_disk: &[(BlobId, std::time::SystemTime)],
    referenced: &std::collections::HashSet<BlobId>,
    now: std::time::SystemTime,
    grace: std::time::Duration,
) -> Vec<BlobId> {
    on_disk
        .iter()
        .filter(|(id, mtime)| {
            !referenced.contains(id)
                && now
                    .duration_since(*mtime)
                    .map(|age| age >= grace)
                    .unwrap_or(false)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Pushes the tombstone adopt/clear actions of a resolved key into the plan.
fn apply_tombstone_actions(
    plan: &mut ControlMergePlan,
    entity_type: &str,
    key: &str,
    res: &KeyResolution,
) {
    if let Some(deleted_at) = res.adopt_tombstone_at {
        plan.adopt_tombstones.push(ControlTombstone {
            entity_type: entity_type.to_string(),
            entity_key: key.to_string(),
            deleted_at,
        });
    }
    if res.clear_tombstone {
        plan.clear_tombstones.push(ControlTombstone {
            entity_type: entity_type.to_string(),
            entity_key: key.to_string(),
            // deleted_at is irrelevant for a clear (keyed by type+key).
            deleted_at: DateTime::<Utc>::MIN_UTC,
        });
    }
}

/// The set of entity keys (owned) appearing in any of a type's four alive/dead
/// maps. Four independent value generics because the local/remote "alive" maps
/// hold different value types (timestamp vs entity reference).
fn union_keys<A, B, C, D>(
    local_alive: &std::collections::HashMap<&str, A>,
    remote_alive: &std::collections::HashMap<&str, B>,
    local_dead: &std::collections::HashMap<&str, C>,
    remote_dead: &std::collections::HashMap<&str, D>,
) -> Vec<String> {
    let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    keys.extend(local_alive.keys().map(|k| k.to_string()));
    keys.extend(remote_alive.keys().map(|k| k.to_string()));
    keys.extend(local_dead.keys().map(|k| k.to_string()));
    keys.extend(remote_dead.keys().map(|k| k.to_string()));
    keys.into_iter().collect()
}

/// [`union_keys`] for the composite-keyed R5 families, whose alive maps are
/// keyed by an owned [`pair_key`] (the dead maps stay borrowed: tombstones
/// store the composite key verbatim).
fn union_keys_owned<A, B, C, D>(
    local_alive: &std::collections::HashMap<String, A>,
    remote_alive: &std::collections::HashMap<String, B>,
    local_dead: &std::collections::HashMap<&str, C>,
    remote_dead: &std::collections::HashMap<&str, D>,
) -> Vec<String> {
    let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    keys.extend(local_alive.keys().cloned());
    keys.extend(remote_alive.keys().cloned());
    keys.extend(local_dead.keys().map(|k| k.to_string()));
    keys.extend(remote_dead.keys().map(|k| k.to_string()));
    keys.into_iter().collect()
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
    /// Timestamp of the last successful contact, if any. Kept on a DEAD peer
    /// (the last time it WAS reachable): the tombstone-GC liveness guard
    /// (review §3.2) and membership pruning (M3) both reason about how long a
    /// peer has been unseen.
    pub last_seen: Option<DateTime<Utc>>,
    /// Whether the peer proved possession of the cluster secret on its most
    /// recent probe — a valid [`ping_nonce_mac`] over our fresh nonce (decision
    /// H12). A peer that merely answers HTTP (a rogue mDNS registrant, a legacy
    /// pre-ping node) is alive but NOT authenticated, and is excluded from
    /// replication fan-out and quorum accounting (see [`PeerNode::eligible`]).
    #[serde(default)]
    pub authenticated: bool,
    /// Whether this peer's cluster-critical config matches ours (see
    /// [`config_fingerprint`]). `true` until a live peer reports a differing
    /// fingerprint; a dead peer (no fresh fingerprint) stays `true` (we don't
    /// know — don't cry wolf).
    #[serde(default = "default_true")]
    pub config_ok: bool,
    /// Peer's total disk capacity (bytes), as it reported via health. `None`
    /// until known. With full replication the smallest node bounds the cluster.
    #[serde(default)]
    pub disk_total: Option<u64>,
    /// Peer's available disk space (bytes), as it reported via health.
    #[serde(default)]
    pub disk_available: Option<u64>,
    /// Highest object `seq` the peer reported on its most recent successful
    /// probe ([`ClusterPingResponse::max_seq`]). `None` for a legacy (pre-ping)
    /// peer or a dead one. Feeds the D3c restore/rewind detection
    /// ([`sync_rewound`]): a peer restored from backup reports a counter lower
    /// than the high-water mark this node already consumed.
    #[serde(default)]
    pub max_seq: Option<u64>,
}

impl PeerNode {
    /// Whether this peer may participate in replication: reachable, proved
    /// possession of the cluster secret (peer authentication, decision H12),
    /// and config-aligned (decision H7). This single predicate gates the
    /// fan-out target list, the write-quorum count, anti-entropy pulls, and
    /// the capacity minimum — an unauthenticated or drifted peer can neither
    /// receive replicas nor sustain a quorum (review §3.7(A), D1).
    pub fn eligible(&self) -> bool {
        self.alive && self.authenticated && self.config_ok
    }
}

fn default_true() -> bool {
    true
}

/// §3.2 — tombstone-GC liveness guard: the known peers whose last contact is
/// missing or older than the grace window. While any exist, purging tombstones
/// is unsafe: a tombstone recorded while such a peer was already unreachable
/// would be gone before the peer ever learns of the deletion, and its stale
/// live row would resurrect the object on re-entry. (A peer seen within the
/// grace necessarily saw — or will pull, it is reachable — every tombstone
/// older than the grace, so purging those is safe.) Membership pruning (M3)
/// eventually removes never-returning peers so they cannot block GC forever;
/// a beyond-grace re-entry after pruning is a documented residual risk.
pub fn tombstone_gc_blockers(
    peers: &[PeerNode],
    now: DateTime<Utc>,
    grace: chrono::Duration,
) -> Vec<PeerNode> {
    let cutoff = now - grace;
    peers
        .iter()
        .filter(|p| !p.alive && p.last_seen.is_none_or(|seen| seen < cutoff))
        .cloned()
        .collect()
}

/// This node's anti-entropy pull-synchronization status toward ONE peer
/// (review D2 — syncing readiness; M1 — stuck-entry evidence). Node-local and
/// in-memory, like the high-water mark itself: it describes how far THIS node
/// has consumed a peer's changes since its own startup, so it resets on
/// restart (costing one extra idempotent full pass) and must never be
/// replicated.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerSyncStatus {
    /// High-water mark: the highest peer `seq` the incremental object
    /// reconcile has applied.
    pub hwm: u64,
    /// When the HWM last advanced. Freshness anchor for the D3c rewind check
    /// ([`sync_rewound`]): a peer-reported `max_seq` older than this may
    /// legitimately predate rows already pulled.
    pub hwm_at: Option<DateTime<Utc>>,
    /// Completion time of the most recent FULL reconcile pass toward the peer
    /// (objects caught up + control snapshot merged).
    pub last_reconcile: Option<DateTime<Utc>>,
    /// Whether at least one full pass completed since this node started. The
    /// D2 readiness gate: until true for every eligible peer, this node may
    /// still be missing rows and `/admin/health` reports `syncing`.
    pub first_pass_done: bool,
    /// M1: manifest entries skipped after persistently failing to apply
    /// (operator evidence — a skipped entry means that key may not converge
    /// here until it changes again on the peer).
    pub skipped_entries: u64,
}

/// D3c — restore/rewind detection: whether a peer's reported object-seq
/// counter ([`PeerNode::max_seq`], from the authenticated ping) has rewound
/// below the high-water mark this node already consumed from it. That happens
/// when the peer was restored from a backup: its post-restore writes re-use
/// seq values below our HWM and would stay invisible to the incremental sync
/// until this node restarts. The caller's remedy is to reset the HWM to 0 (one
/// idempotent full re-pull).
///
/// The freshness guard (`peer_last_seen > hwm_at`) avoids the false alarm
/// under sustained writes: probe and reconcile run on independent cadences, so
/// a ping report taken BEFORE our last HWM advance may legitimately be lower
/// than the HWM without any rewind having happened. A genuine restore keeps
/// reporting the rewound counter on every later ping, so the detection is only
/// deferred to the first probe after the last reconcile, never lost.
pub fn sync_rewound(
    hwm: u64,
    hwm_at: Option<DateTime<Utc>>,
    peer_max_seq: Option<u64>,
    peer_last_seen: Option<DateTime<Utc>>,
) -> bool {
    let (Some(max_seq), Some(seen)) = (peer_max_seq, peer_last_seen) else {
        return false; // legacy peer / never contacted: nothing to judge
    };
    if max_seq >= hwm {
        return false;
    }
    match hwm_at {
        Some(at) => seen > at,
        // hwm > 0 with no recorded advance time cannot happen (they are set
        // together); be conservative and trust the report if it ever does.
        None => true,
    }
}

/// Minimum of two optional values, treating `None` as "unknown" (ignored):
/// `min_opt(Some(a), Some(b)) = min`, `min_opt(Some(a), None) = Some(a)`.
fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Why the cluster write gate is currently closed (or open). Computed by
/// [`ClusterState::write_gate`]; the store decorators map each variant to a
/// distinct `503 ServiceUnavailable` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGate {
    /// Writes may proceed (subject to the post-fan-out ACK count).
    Open,
    /// Quorum mode: too few eligible nodes (authenticated + config-aligned,
    /// including self) to possibly reach the write quorum.
    NoQuorum { eligible: usize, quorum: u32 },
    /// Quorum mode, decision H6 (D3a): MORE eligible nodes than the configured
    /// `cluster_size`. The write majority is derived from `cluster_size`, so an
    /// over-sized membership can form two disjoint "majorities" (split-brain).
    /// A misconfiguration this dangerous fails closed — no escape hatch.
    SizeExceeded { eligible: usize, cluster_size: u32 },
}

/// Serializable point-in-time view of the cluster, for the admin API / console.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterSnapshot {
    pub node_id: String,
    /// This node's own advertised endpoint, once the membership manager has
    /// learned it (null until the first self-probe).
    pub local_endpoint: Option<String>,
    /// Durable copies (including self) required to ACK a write. `null` =
    /// "available" mode (W=1).
    pub write_quorum: Option<u32>,
    /// Whether writes can currently be acknowledged.
    pub has_write_quorum: bool,
    /// Live nodes reachable at all, including this node (visibility count —
    /// includes unauthenticated/drifted nodes).
    pub live_node_count: usize,
    /// Nodes that count for replication (alive + authenticated +
    /// config-aligned peers, plus this node). This is the number the write
    /// quorum is measured against.
    pub eligible_node_count: usize,
    /// Decision H6 (D3a): true when eligible nodes exceed the configured
    /// `cluster_size` — the write gate is closed until the operator resizes.
    pub size_exceeded: bool,
    /// Review §3.2: true when the anti-entropy worker is skipping tombstone GC
    /// because a known peer has been unreachable beyond the grace window.
    pub tombstone_gc_blocked: bool,
    /// Decision H5 (review §3.3): true when THIS node currently holds the
    /// worker-leader role (lowest `node_id` among eligible nodes) and so runs
    /// the cluster-singleton background work. In a stable cluster exactly one
    /// node reports `true`.
    pub worker_leader: bool,
    /// Review D2: true while this node has not completed its first anti-entropy
    /// pass toward every eligible peer since startup — it may still be missing
    /// rows and should not receive LB traffic (`/admin/health` answers 503).
    pub syncing: bool,
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
    /// Configured expected cluster size (quorum mode; `None` in available
    /// mode). The H6 (D3a) gate refuses writes when eligible nodes exceed it.
    cluster_size: Option<u32>,
    /// Currently known peers (excluding self).
    peers: RwLock<Vec<PeerNode>>,
    /// This node's own advertised endpoint, learned by the membership manager
    /// when a discovery candidate's health probe returns this node's own id.
    /// `None` until that first self-probe completes.
    local_endpoint: RwLock<Option<String>>,
    /// This node's own [`config_fingerprint`], set once at startup. `None` until
    /// set (e.g. before the master key is resolved). Peers' fingerprints are
    /// compared against this to flag config drift.
    config_fingerprint: RwLock<Option<String>>,
    /// Review §3.2: set by the anti-entropy worker while it is skipping
    /// tombstone GC because a known peer is unseen beyond the grace window.
    tombstone_gc_blocked: AtomicBool,
    /// Review D2/D3c/M1: per-peer pull-sync status (keyed by peer `node_id`),
    /// written by the anti-entropy worker and read by the health/admin
    /// endpoints. In-memory by design, like the HWM it carries.
    sync: RwLock<std::collections::HashMap<String, PeerSyncStatus>>,
}

impl ClusterState {
    /// Creates cluster state for this node. `write_quorum` is the majority
    /// threshold in quorum mode, or `None` in available mode; `cluster_size`
    /// is the configured expected size (quorum mode only — it drives the H6
    /// over-size write gate).
    pub fn new(
        node_id: impl Into<String>,
        write_quorum: Option<u32>,
        cluster_size: Option<u32>,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            write_quorum,
            cluster_size,
            peers: RwLock::new(Vec::new()),
            local_endpoint: RwLock::new(None),
            config_fingerprint: RwLock::new(None),
            tombstone_gc_blocked: AtomicBool::new(false),
            sync: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Records this node's own cluster-config fingerprint (set once at startup).
    pub fn set_config_fingerprint(&self, fingerprint: impl Into<String>) {
        *self
            .config_fingerprint
            .write()
            .expect("cluster config_fingerprint lock poisoned") = Some(fingerprint.into());
    }

    /// This node's own cluster-config fingerprint, if set.
    pub fn config_fingerprint(&self) -> Option<String> {
        self.config_fingerprint
            .read()
            .expect("cluster config_fingerprint lock poisoned")
            .clone()
    }

    /// The cluster's effective disk capacity (bytes), as `(min_total,
    /// min_available)` over this node and all ELIGIBLE peers. With full
    /// replication the smallest node bounds what the cluster can store, so the
    /// minimum free space is what gates writes and is shown in the dashboard.
    /// Only eligible peers count: an unauthenticated node (e.g. a rogue mDNS
    /// registrant) must not be able to close the capacity guard by advertising
    /// a tiny disk, and a drifted node does not receive replicas anyway.
    /// `local_*` are this node's own stats (the caller computes them); peers
    /// whose stats are not known yet (`None`) are skipped.
    pub fn min_disk(
        &self,
        local_total: Option<u64>,
        local_available: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        let mut min_total = local_total;
        let mut min_available = local_available;
        for p in self.peers().iter().filter(|p| p.eligible()) {
            min_total = min_opt(min_total, p.disk_total);
            min_available = min_opt(min_available, p.disk_available);
        }
        (min_total, min_available)
    }

    /// Records this node's own advertised endpoint (called by the membership
    /// manager once it recognises its own id in a health probe).
    pub fn set_local_endpoint(&self, endpoint: impl Into<String>) {
        *self
            .local_endpoint
            .write()
            .expect("cluster local_endpoint lock poisoned") = Some(endpoint.into());
    }

    /// This node's own advertised endpoint, if learned yet.
    pub fn local_endpoint(&self) -> Option<String> {
        self.local_endpoint
            .read()
            .expect("cluster local_endpoint lock poisoned")
            .clone()
    }

    /// This node's stable identity.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The configured write quorum (majority), or `None` in available mode.
    pub fn write_quorum(&self) -> Option<u32> {
        self.write_quorum
    }

    /// Replaces the known peer set (called by the membership manager). Sync
    /// statuses of peers no longer known (pruned by M3) are dropped with them;
    /// a dead-but-remembered peer keeps its entry, so a returning node resumes
    /// from its incremental HWM instead of a full re-pull.
    pub fn set_peers(&self, peers: Vec<PeerNode>) {
        self.sync
            .write()
            .expect("cluster sync lock poisoned")
            .retain(|id, _| peers.iter().any(|p| p.node_id == *id));
        *self.peers.write().expect("cluster peers lock poisoned") = peers;
    }

    /// This node's pull-sync status toward one peer (zeroed default when the
    /// peer was never reconciled from).
    pub fn peer_sync(&self, node_id: &str) -> PeerSyncStatus {
        self.sync
            .read()
            .expect("cluster sync lock poisoned")
            .get(node_id)
            .cloned()
            .unwrap_or_default()
    }

    /// All per-peer pull-sync statuses, keyed by peer `node_id` (for the
    /// admin topology view).
    pub fn sync_status(&self) -> std::collections::HashMap<String, PeerSyncStatus> {
        self.sync
            .read()
            .expect("cluster sync lock poisoned")
            .clone()
    }

    /// Records an object-reconcile HWM advance toward a peer (`at` anchors the
    /// D3c freshness guard — see [`sync_rewound`]).
    pub fn set_sync_hwm(&self, node_id: &str, hwm: u64, at: DateTime<Utc>) {
        let mut sync = self.sync.write().expect("cluster sync lock poisoned");
        let entry = sync.entry(node_id.to_string()).or_default();
        entry.hwm = hwm;
        entry.hwm_at = Some(at);
    }

    /// D3c: resets a peer's HWM to 0 after its seq counter was observed to
    /// rewind (restore from backup) — the next pass re-pulls its full
    /// manifest (idempotent).
    pub fn reset_sync_hwm(&self, node_id: &str) {
        let mut sync = self.sync.write().expect("cluster sync lock poisoned");
        let entry = sync.entry(node_id.to_string()).or_default();
        entry.hwm = 0;
        entry.hwm_at = None;
    }

    /// Marks a completed FULL reconcile pass toward a peer (objects caught up
    /// + control snapshot merged): stamps `last_reconcile` and latches
    /// `first_pass_done` (the D2 readiness signal).
    pub fn record_reconcile_complete(&self, node_id: &str, at: DateTime<Utc>) {
        let mut sync = self.sync.write().expect("cluster sync lock poisoned");
        let entry = sync.entry(node_id.to_string()).or_default();
        entry.last_reconcile = Some(at);
        entry.first_pass_done = true;
    }

    /// M1: counts a manifest entry skipped after persistent apply failures
    /// (operator evidence in `/admin/cluster`).
    pub fn record_skipped_entry(&self, node_id: &str) {
        let mut sync = self.sync.write().expect("cluster sync lock poisoned");
        sync.entry(node_id.to_string()).or_default().skipped_entries += 1;
    }

    /// Review D2 — the readiness gate: true while any ELIGIBLE peer lacks a
    /// completed first reconcile pass since this node started. Until then this
    /// node may answer 404s / partial listings for data it has not pulled yet,
    /// so `/admin/health` reports `syncing` (503) and the LB keeps it out of
    /// rotation. Eligible — not merely alive — peers gate it for the same
    /// reason they gate the quorum: a rogue or drifted peer is never
    /// reconciled from, so requiring a pass toward it would deadlock
    /// readiness. With no eligible peers (cluster of one, full outage) the
    /// node reports ready: degraded-but-serving beats permanently dark.
    pub fn is_syncing(&self) -> bool {
        let sync = self.sync.read().expect("cluster sync lock poisoned");
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .iter()
            .filter(|p| p.eligible())
            .any(|p| !sync.get(&p.node_id).is_some_and(|s| s.first_pass_done))
    }

    /// A clone of all known peers (alive or not).
    pub fn peers(&self) -> Vec<PeerNode> {
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .clone()
    }

    /// Number of peers currently considered alive (excluding this node).
    /// Visibility count: includes unauthenticated and drifted peers.
    pub fn live_peer_count(&self) -> usize {
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .iter()
            .filter(|p| p.alive)
            .count()
    }

    /// Total nodes currently reachable at all, including this node
    /// (visibility count — NOT what the quorum is measured against).
    pub fn live_node_count(&self) -> usize {
        self.live_peer_count() + 1
    }

    /// Peers that count for replication: alive AND authenticated AND
    /// config-aligned (see [`PeerNode::eligible`]).
    pub fn eligible_peer_count(&self) -> usize {
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .iter()
            .filter(|p| p.eligible())
            .count()
    }

    /// Nodes that count for replication, including this node. The write
    /// quorum and the H6 size gate are measured against this.
    pub fn eligible_node_count(&self) -> usize {
        self.eligible_peer_count() + 1
    }

    /// The admission write gate under the configured consistency policy
    /// (decisions H12/H7/H6 — review §3.7(A), D1, D3a).
    ///
    /// - Available mode (`write_quorum == None`): always [`WriteGate::Open`].
    /// - Quorum mode: counts ELIGIBLE nodes (authenticated + config-aligned
    ///   peers, plus self) — an unauthenticated rogue or a drifted node can
    ///   neither sustain a quorum ([`WriteGate::NoQuorum`]) nor hide an
    ///   over-size membership ([`WriteGate::SizeExceeded`], fail-closed).
    pub fn write_gate(&self) -> WriteGate {
        let Some(q) = self.write_quorum else {
            return WriteGate::Open;
        };
        let eligible = self.eligible_node_count();
        if let Some(size) = self.cluster_size {
            if eligible > size as usize {
                return WriteGate::SizeExceeded {
                    eligible,
                    cluster_size: size,
                };
            }
        }
        if eligible < q as usize {
            return WriteGate::NoQuorum {
                eligible,
                quorum: q,
            };
        }
        WriteGate::Open
    }

    /// Whether a write can currently be acknowledged under the configured
    /// consistency policy (shorthand for `write_gate() == Open`).
    pub fn has_write_quorum(&self) -> bool {
        self.write_gate() == WriteGate::Open
    }

    /// Decision H5 (review §3.3) — the symmetric worker-leader gate: this
    /// node runs the cluster-singleton background work (the lifecycle
    /// evaluator) iff it has the lowest `node_id` among the ELIGIBLE nodes
    /// (authenticated + config-aligned peers, plus self). Counting eligible —
    /// not merely alive — nodes is deliberate: an unauthenticated rogue or a
    /// drifted peer with a low `node_id` must not be able to steal the role
    /// and silence the workers cluster-wide (the same rationale that gates
    /// the quorum and `min_disk` on eligibility — review §3.7(A), D1).
    ///
    /// Trivially true single-node (no peers). Failover is automatic: when the
    /// leader dies, the next-lowest eligible node observes it at its next
    /// membership tick and takes the role. During a membership disagreement
    /// two nodes can briefly both claim it — a double-execution window H5
    /// explicitly accepts (the gated work is idempotent and converges).
    pub fn is_worker_leader(&self) -> bool {
        self.peers
            .read()
            .expect("cluster peers lock poisoned")
            .iter()
            .filter(|p| p.eligible())
            .all(|p| p.node_id.as_str() > self.node_id.as_str())
    }

    /// Records whether the anti-entropy worker is currently skipping tombstone
    /// GC because of an unseen-beyond-grace peer (review §3.2).
    pub fn set_tombstone_gc_blocked(&self, blocked: bool) {
        self.tombstone_gc_blocked.store(blocked, Ordering::Relaxed);
    }

    /// Whether tombstone GC is currently blocked by the §3.2 liveness guard.
    pub fn tombstone_gc_blocked(&self) -> bool {
        self.tombstone_gc_blocked.load(Ordering::Relaxed)
    }

    /// A serializable snapshot for the admin API / console dashboard.
    pub fn snapshot(&self) -> ClusterSnapshot {
        let peers = self.peers();
        let live_node_count = peers.iter().filter(|p| p.alive).count() + 1;
        let eligible_node_count = peers.iter().filter(|p| p.eligible()).count() + 1;
        let gate = self.write_gate();
        ClusterSnapshot {
            node_id: self.node_id.clone(),
            local_endpoint: self.local_endpoint(),
            write_quorum: self.write_quorum,
            has_write_quorum: gate == WriteGate::Open,
            live_node_count,
            eligible_node_count,
            size_exceeded: matches!(gate, WriteGate::SizeExceeded { .. }),
            tombstone_gc_blocked: self.tombstone_gc_blocked(),
            worker_leader: self.is_worker_leader(),
            syncing: self.is_syncing(),
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
            // Tests model the normal case: a live peer has proven possession
            // of the secret (eligibility variations get dedicated tests).
            authenticated: alive,
            config_ok: true,
            disk_total: None,
            disk_available: None,
            max_seq: None,
        }
    }

    #[test]
    fn quorum_satisfied_available_mode_always_true() {
        // Available mode (no quorum): a single durable copy is enough.
        assert!(quorum_satisfied(1, None));
        assert!(quorum_satisfied(0, None)); // degenerate, still no gate
    }

    #[test]
    fn quorum_satisfied_counts_acks_against_threshold() {
        // 3-node cluster, write_quorum = 2: local + 1 peer ACK suffices.
        assert!(quorum_satisfied(2, Some(2)));
        assert!(quorum_satisfied(3, Some(2)));
        // Local copy alone is NOT a quorum: the write must error (review §2.1 —
        // an admitted write whose fan-out failed everywhere is a ghost write).
        assert!(!quorum_satisfied(1, Some(2)));
        assert!(!quorum_satisfied(0, Some(2)));
        // 5-node cluster, write_quorum = 3.
        assert!(quorum_satisfied(3, Some(3)));
        assert!(!quorum_satisfied(2, Some(3)));
    }

    #[test]
    fn min_disk_takes_minimum_over_alive_nodes() {
        let state = ClusterState::new("self", None, None);
        let mut p1 = peer("n2", true);
        p1.disk_total = Some(2_000);
        p1.disk_available = Some(100); // the bottleneck for free space
        let mut p2 = peer("n3", true);
        p2.disk_total = Some(1_000); // the bottleneck for total
        p2.disk_available = Some(800);
        // A dead peer must NOT drag the minimum down.
        let mut dead = peer("n4", false);
        dead.disk_total = Some(1);
        dead.disk_available = Some(1);
        state.set_peers(vec![p1, p2, dead]);

        // Local node: 3 TB total, 500 free.
        let (total, avail) = state.min_disk(Some(3_000), Some(500));
        assert_eq!(total, Some(1_000), "min total across alive nodes + self");
        assert_eq!(avail, Some(100), "min available across alive nodes + self");
    }

    #[test]
    fn min_disk_skips_unknown_peer_stats() {
        let state = ClusterState::new("self", None, None);
        state.set_peers(vec![peer("n2", true)]); // disk stats None
        // Peer's unknown stats are ignored; only local counts.
        assert_eq!(state.min_disk(Some(10), Some(5)), (Some(10), Some(5)));
    }

    #[test]
    fn available_mode_always_has_quorum() {
        let state = ClusterState::new("self", None, None);
        // No peers, alone: still writable in available mode.
        assert!(state.has_write_quorum());
        assert_eq!(state.live_node_count(), 1);
    }

    #[test]
    fn quorum_mode_needs_majority() {
        // cluster_size = 3 → write_quorum = 2.
        let state = ClusterState::new("self", Some(2), Some(3));
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
        let state = ClusterState::new("self", Some(2), Some(3));
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
        let state = ClusterState::new("self", None, None);
        assert!(state.peers().is_empty());
        state.set_peers(vec![peer("n2", true)]);
        assert_eq!(state.peers().len(), 1);
        assert_eq!(state.peers()[0].node_id, "n2");
    }

    #[test]
    fn worker_leader_single_node() {
        // No peers at all: trivially the leader (single-node deployments
        // and a freshly started node that has not discovered anyone yet).
        let state = ClusterState::new("self", None, None);
        assert!(state.is_worker_leader());
    }

    #[test]
    fn worker_leader_is_lowest_eligible_node_id() {
        // "b" leads while every eligible peer sorts above it...
        let state = ClusterState::new("b", Some(2), Some(3));
        state.set_peers(vec![peer("c", true), peer("d", true)]);
        assert!(state.is_worker_leader());
        // ...and yields as soon as a lower-id eligible peer appears.
        state.set_peers(vec![peer("a", true), peer("c", true)]);
        assert!(!state.is_worker_leader());
    }

    #[test]
    fn worker_leader_failover_to_next_lowest() {
        // "b" is not the leader while "a" is eligible; when "a" dies, the
        // role moves to "b" at the next membership tick.
        let state = ClusterState::new("b", Some(2), Some(3));
        state.set_peers(vec![peer("a", true), peer("c", true)]);
        assert!(!state.is_worker_leader());
        state.set_peers(vec![peer("a", false), peer("c", true)]);
        assert!(state.is_worker_leader());
    }

    #[test]
    fn worker_leader_ignores_ineligible_lower_peer() {
        // A lower-id peer that is alive but NOT eligible (an unauthenticated
        // rogue, a drifted node) must not steal the leadership and silence
        // the workers cluster-wide.
        let state = ClusterState::new("b", Some(2), Some(3));
        let mut rogue = peer("a", true);
        rogue.authenticated = false;
        state.set_peers(vec![rogue, peer("c", true)]);
        assert!(state.is_worker_leader());

        let mut drifted = peer("a", true);
        drifted.config_ok = false;
        state.set_peers(vec![drifted, peer("c", true)]);
        assert!(state.is_worker_leader());
    }

    #[test]
    fn snapshot_reports_worker_leader() {
        let state = ClusterState::new("b", Some(2), Some(3));
        state.set_peers(vec![peer("c", true)]);
        assert!(state.snapshot().worker_leader);
        state.set_peers(vec![peer("a", true)]);
        assert!(!state.snapshot().worker_leader);
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
            ControlOp::GrantUpsert {
                grant: Grant {
                    grant_id: "g1".to_string(),
                    name: "g".to_string(),
                    description: String::new(),
                    document: crate::policy::PolicyDocument {
                        version: "2012-10-17".to_string(),
                        statement: vec![],
                    },
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            },
            ControlOp::UserGrantAttach {
                user_id: "u1".to_string(),
                grant_id: "g1".to_string(),
            },
            ControlOp::TeamUpsert {
                team: Team {
                    team_id: "t1".to_string(),
                    name: "t".to_string(),
                    description: String::new(),
                    created_at: Utc::now(),
                },
            },
            ControlOp::TeamMemberAdd {
                team_id: "t1".to_string(),
                user_id: "u1".to_string(),
            },
            ControlOp::ServerConfigSet {
                key: "region".to_string(),
                value: "eu-west-1".to_string(),
            },
            ControlOp::ServerConfigDelete {
                key: "region".to_string(),
            },
            ControlOp::ObjectTags {
                bucket: "b".to_string(),
                key: "k".to_string(),
                version_id: String::new(),
                tags: vec![("k".to_string(), "v".to_string())],
            },
            ControlOp::MultipartCreate {
                record: MultipartUploadRecord {
                    upload_id: "u1".to_string(),
                    bucket: "b".to_string(),
                    key: "k".to_string(),
                    content_type: None,
                    initiated_at: Utc::now(),
                    metadata: Default::default(),
                    checksum_algorithm: None,
                },
            },
            ControlOp::PartUpsert {
                part: PartRecord {
                    upload_id: "u1".to_string(),
                    part_number: 1,
                    blob_id: crate::types::BlobId("blob-1".to_string()),
                    size: 4,
                    etag: "e".to_string(),
                    checksum_value: None,
                    last_modified: None,
                },
            },
            ControlOp::MultipartDelete {
                upload_id: "u1".to_string(),
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

    #[test]
    fn manifest_wire_serde_roundtrip() {
        // Request.
        let req = ClusterManifestRequest {
            since: 42,
            limit: 500,
        };
        let back: ClusterManifestRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back.since, 42);
        assert_eq!(back.limit, 500);

        // Response with one entry + the cursor.
        let record = ObjectRecord {
            bucket: "b".to_string(),
            key: "k".to_string(),
            blob_id: crate::types::BlobId("blob-1".to_string()),
            size: 4,
            etag: "e".to_string(),
            content_type: None,
            last_modified: Utc::now(),
            metadata: Default::default(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: Some("v1".to_string()),
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            replication_status: None,
            lock_updated_at: None,
        };
        let manifest = ClusterManifest {
            entries: vec![ManifestEntry { seq: 7, record }],
            cursor: 7,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: ClusterManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cursor, 7);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].seq, 7);
        assert_eq!(back.entries[0].record.key, "k");

        // Empty batch is valid (caller is caught up).
        let empty = ClusterManifest {
            entries: vec![],
            cursor: 42,
        };
        let back: ClusterManifest =
            serde_json::from_str(&serde_json::to_string(&empty).unwrap()).unwrap();
        assert!(back.entries.is_empty());
        assert_eq!(back.cursor, 42);
    }

    // --- plan_control_merge -------------------------------------------------

    fn ts(secs: i64) -> DateTime<Utc> {
        chrono::TimeZone::timestamp_opt(&Utc, secs, 0).unwrap()
    }

    fn tcred(id: &str, updated: i64) -> TimestampedCredential {
        TimestampedCredential {
            credential: Credential {
                access_key_id: id.to_string(),
                secret_access_key: "s".to_string(),
                description: String::new(),
                created_at: ts(updated),
                active: true,
                admin: false,
                user_id: "root".to_string(),
            },
            updated_at: ts(updated),
        }
    }

    fn tomb(entity_type: &str, key: &str, at: i64) -> ControlTombstone {
        ControlTombstone {
            entity_type: entity_type.to_string(),
            entity_key: key.to_string(),
            deleted_at: ts(at),
        }
    }

    fn snap_with_creds(
        credentials: Vec<TimestampedCredential>,
        tombstones: Vec<ControlTombstone>,
    ) -> ControlSnapshot {
        ControlSnapshot {
            credentials,
            tombstones,
            ..Default::default()
        }
    }

    #[test]
    fn merge_identical_snapshots_is_empty() {
        let s = snap_with_creds(vec![tcred("AK", 100)], vec![]);
        assert!(plan_control_merge(&s, &s).is_empty());
    }

    #[test]
    fn merge_pulls_newer_remote_credential() {
        let local = snap_with_creds(vec![tcred("AK", 100)], vec![]);
        let remote = snap_with_creds(vec![tcred("AK", 200)], vec![]);
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_credentials.len(), 1);
        assert_eq!(plan.upsert_credentials[0].updated_at, ts(200));
        assert!(plan.delete_credentials.is_empty());
    }

    #[test]
    fn merge_keeps_newer_local_credential() {
        let local = snap_with_creds(vec![tcred("AK", 300)], vec![]);
        let remote = snap_with_creds(vec![tcred("AK", 200)], vec![]);
        // Local is newer → no change pulled from remote.
        assert!(plan_control_merge(&local, &remote).is_empty());
    }

    #[test]
    fn merge_adopts_remote_create_when_local_missing() {
        let local = snap_with_creds(vec![], vec![]);
        let remote = snap_with_creds(vec![tcred("AK", 100)], vec![]);
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_credentials.len(), 1);
    }

    #[test]
    fn merge_deletes_when_remote_tombstone_is_newer() {
        // Local has a live credential; remote deleted it later.
        let local = snap_with_creds(vec![tcred("AK", 100)], vec![]);
        let remote = snap_with_creds(vec![], vec![tomb(TOMBSTONE_CREDENTIAL, "AK", 200)]);
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.delete_credentials, vec!["AK".to_string()]);
        assert_eq!(plan.adopt_tombstones.len(), 1);
        assert_eq!(plan.adopt_tombstones[0].deleted_at, ts(200));
        assert!(plan.upsert_credentials.is_empty());
    }

    #[test]
    fn merge_does_not_resurrect_when_local_tombstone_is_newer() {
        // Local deleted AK (newer); remote still has it alive (older).
        let local = snap_with_creds(vec![], vec![tomb(TOMBSTONE_CREDENTIAL, "AK", 300)]);
        let remote = snap_with_creds(vec![tcred("AK", 100)], vec![]);
        let plan = plan_control_merge(&local, &remote);
        // The stale remote credential must NOT be pulled in.
        assert!(plan.upsert_credentials.is_empty());
        assert!(plan.delete_credentials.is_empty());
    }

    // --- plan_blob_gc -------------------------------------------------------

    // --- config_fingerprint -------------------------------------------------

    #[test]
    fn config_fingerprint_is_deterministic_and_field_sensitive() {
        let base = config_fingerprint("c1", "quorum", Some(2), "secret", Some("ab12cd34"));
        // Same inputs → same fingerprint.
        assert_eq!(
            base,
            config_fingerprint("c1", "quorum", Some(2), "secret", Some("ab12cd34"))
        );
        // Each critical field changes it.
        assert_ne!(base, config_fingerprint("c2", "quorum", Some(2), "secret", Some("ab12cd34")));
        assert_ne!(base, config_fingerprint("c1", "available", None, "secret", Some("ab12cd34")));
        assert_ne!(base, config_fingerprint("c1", "quorum", Some(3), "secret", Some("ab12cd34")));
        assert_ne!(base, config_fingerprint("c1", "quorum", Some(2), "other", Some("ab12cd34")));
        assert_ne!(base, config_fingerprint("c1", "quorum", Some(2), "secret", Some("ffffffff")));
        assert_ne!(base, config_fingerprint("c1", "quorum", Some(2), "secret", None));
    }

    #[test]
    fn config_fingerprint_available_ignores_cluster_size() {
        // In available mode write_quorum is None regardless of cluster_size, so two
        // available nodes with different cluster_size still align.
        assert_eq!(
            config_fingerprint("c1", "available", None, "s", None),
            config_fingerprint("c1", "available", None, "s", None)
        );
    }

    #[test]
    fn blob_gc_reclaims_old_unreferenced_only() {
        use std::collections::HashSet;
        use std::time::{Duration, SystemTime};

        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        let young = now - Duration::from_secs(1);
        let grace = Duration::from_secs(60);

        let on_disk = vec![
            (BlobId("orphan-old".to_string()), old),    // unreferenced + old → GC
            (BlobId("orphan-young".to_string()), young), // unreferenced but young → kept
            (BlobId("live-old".to_string()), old),       // referenced → kept
        ];
        let referenced: HashSet<BlobId> = [BlobId("live-old".to_string())].into_iter().collect();

        let plan = plan_blob_gc(&on_disk, &referenced, now, grace);
        assert_eq!(plan, vec![BlobId("orphan-old".to_string())]);
    }

    #[test]
    fn blob_gc_empty_when_all_referenced() {
        use std::collections::HashSet;
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now();
        let on_disk = vec![(BlobId("a".to_string()), now - Duration::from_secs(10_000))];
        let referenced: HashSet<BlobId> = [BlobId("a".to_string())].into_iter().collect();
        assert!(plan_blob_gc(&on_disk, &referenced, now, Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn merge_recreate_beats_older_tombstone_and_clears_it() {
        // Local has a stale tombstone for AK; remote re-created it later.
        let local = snap_with_creds(vec![], vec![tomb(TOMBSTONE_CREDENTIAL, "AK", 100)]);
        let remote = snap_with_creds(vec![tcred("AK", 200)], vec![]);
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_credentials.len(), 1, "newer create wins");
        assert_eq!(plan.clear_tombstones.len(), 1, "stale local tombstone cleared");
        assert!(plan.delete_credentials.is_empty());
    }

    // --- plan_control_merge: R5 families (TD-016) ----------------------------

    fn tuser(id: &str, updated: i64) -> TimestampedUser {
        TimestampedUser {
            user: User {
                user_id: id.to_string(),
                username: id.to_string(),
                description: String::new(),
                is_root: false,
                created_at: ts(updated),
            },
            updated_at: ts(updated),
        }
    }

    fn tgrant(id: &str, updated: i64) -> Grant {
        Grant {
            grant_id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            document: crate::policy::PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![],
            },
            created_at: ts(updated),
            updated_at: ts(updated),
        }
    }

    fn tteam(id: &str, updated: i64) -> TimestampedTeam {
        TimestampedTeam {
            team: Team {
                team_id: id.to_string(),
                name: id.to_string(),
                description: String::new(),
                created_at: ts(updated),
            },
            updated_at: ts(updated),
        }
    }

    fn tug(user: &str, grant: &str, updated: i64) -> TimestampedUserGrant {
        TimestampedUserGrant {
            user_id: user.to_string(),
            grant_id: grant.to_string(),
            updated_at: ts(updated),
        }
    }

    fn upload(id: &str, initiated: i64) -> MultipartUploadRecord {
        MultipartUploadRecord {
            upload_id: id.to_string(),
            bucket: "b".to_string(),
            key: "k".to_string(),
            content_type: None,
            initiated_at: ts(initiated),
            metadata: std::collections::HashMap::new(),
            checksum_algorithm: None,
        }
    }

    fn part(upload_id: &str, n: u32, modified: i64) -> PartRecord {
        PartRecord {
            upload_id: upload_id.to_string(),
            part_number: n,
            blob_id: BlobId(format!("{upload_id}-{n}")),
            size: 1,
            etag: "e".to_string(),
            checksum_value: None,
            last_modified: Some(ts(modified)),
        }
    }

    #[test]
    fn merge_user_grant_detach_wins_over_stale_attach() {
        // Local still holds the attachment; the peer detached it later.
        let local = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            grants: vec![tgrant("g1", 50)],
            user_grants: vec![tug("u1", "g1", 100)],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            grants: vec![tgrant("g1", 50)],
            tombstones: vec![tomb(TOMBSTONE_USER_GRANT, &pair_key("u1", "g1"), 200)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(
            plan.delete_user_grants,
            vec![("u1".to_string(), "g1".to_string())]
        );
        assert!(plan
            .adopt_tombstones
            .iter()
            .any(|t| t.entity_type == TOMBSTONE_USER_GRANT));
        assert!(plan.upsert_user_grants.is_empty());
    }

    #[test]
    fn merge_user_grant_reattach_beats_older_tombstone() {
        // Local detached at 100; the peer re-attached at 200.
        let local = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            grants: vec![tgrant("g1", 50)],
            tombstones: vec![tomb(TOMBSTONE_USER_GRANT, &pair_key("u1", "g1"), 100)],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            grants: vec![tgrant("g1", 50)],
            user_grants: vec![tug("u1", "g1", 200)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_user_grants.len(), 1);
        assert!(plan
            .clear_tombstones
            .iter()
            .any(|t| t.entity_type == TOMBSTONE_USER_GRANT));
        assert!(plan.delete_user_grants.is_empty());
    }

    #[test]
    fn merge_child_upserts_filtered_when_parent_dead() {
        // The peer still holds an attachment and a membership, but their
        // parents (grant g1, team t1) are tombstoned NEWER than the children:
        // adopting the children would resurrect cascade-deleted rows (and
        // violate the join-table FKs on PostgreSQL).
        let local = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            tombstones: vec![
                tomb(TOMBSTONE_GRANT, "g1", 300),
                tomb(TOMBSTONE_TEAM, "t1", 300),
            ],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            grants: vec![tgrant("g1", 100)],
            teams: vec![tteam("t1", 100)],
            user_grants: vec![tug("u1", "g1", 100)],
            team_members: vec![TimestampedTeamMember {
                team_id: "t1".to_string(),
                user_id: "u1".to_string(),
                updated_at: ts(100),
            }],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert!(plan.upsert_user_grants.is_empty(), "dead grant parent");
        assert!(plan.upsert_team_members.is_empty(), "dead team parent");
        // The parents themselves resolve to deletion on the remote side only
        // (nothing to delete locally), and the children carry no upserts.
        assert!(plan.upsert_grants.is_empty());
        assert!(plan.upsert_teams.is_empty());
    }

    #[test]
    fn merge_bucket_config_lww_and_parent_filter() {
        let bucket = || BucketInfo {
            name: "b".to_string(),
            created_at: ts(10),
            owner: "root".to_string(),
        };
        let bc = |value: &str, at: i64| TimestampedBucketConfig {
            bucket: "b".to_string(),
            key: "versioning".to_string(),
            value: value.to_string(),
            updated_at: ts(at),
        };
        // Newer remote value wins.
        let local = ControlSnapshot {
            buckets: vec![bucket()],
            bucket_configs: vec![bc("Suspended", 100)],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            buckets: vec![bucket()],
            bucket_configs: vec![bc("Enabled", 200)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_bucket_configs.len(), 1);
        assert_eq!(plan.upsert_bucket_configs[0].value, "Enabled");

        // Same change, but the bucket is dead → config not adopted.
        let local_dead = ControlSnapshot {
            tombstones: vec![tomb(TOMBSTONE_BUCKET, "b", 300)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local_dead, &remote);
        assert!(plan.upsert_bucket_configs.is_empty(), "dead bucket parent");
    }

    #[test]
    fn merge_bucket_tags_are_one_set_level_entity() {
        let bucket = || BucketInfo {
            name: "b".to_string(),
            created_at: ts(10),
            owner: "root".to_string(),
        };
        let tags = |pairs: &[(&str, &str)], at: i64| TimestampedBucketTags {
            bucket: "b".to_string(),
            tags: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            updated_at: ts(at),
        };
        // A newer replace wins WHOLE-set: the dropped key disappears with it.
        let local = ControlSnapshot {
            buckets: vec![bucket()],
            bucket_tags: vec![tags(&[("env", "dev"), ("team", "x")], 100)],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            buckets: vec![bucket()],
            bucket_tags: vec![tags(&[("env", "prod")], 200)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_bucket_tags.len(), 1);
        assert_eq!(plan.upsert_bucket_tags[0].tags, vec![("env".to_string(), "prod".to_string())]);

        // A newer clear (tombstone) deletes the local set.
        let remote_cleared = ControlSnapshot {
            buckets: vec![bucket()],
            tombstones: vec![tomb(TOMBSTONE_BUCKET_TAGS, "b", 200)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote_cleared);
        assert_eq!(plan.delete_bucket_tags, vec!["b".to_string()]);
    }

    #[test]
    fn merge_server_config_skips_node_local_keys() {
        // Even a snapshot that (wrongly or maliciously) carries node_id must
        // not rewrite this node's identity — same double defense as D12.1.
        let sc = |key: &str, value: &str, at: i64| TimestampedServerConfig {
            key: key.to_string(),
            value: value.to_string(),
            updated_at: ts(at),
        };
        let local = ControlSnapshot::default();
        let remote = ControlSnapshot {
            server_configs: vec![sc(NODE_ID_KEY, "evil-node", 200), sc("region", "eu-south-1", 200)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_server_configs.len(), 1, "only the cluster-wide key");
        assert_eq!(plan.upsert_server_configs[0].key, "region");
    }

    #[test]
    fn merge_multipart_close_wins_and_drops_parts() {
        // Local was down during Complete/Abort: it still holds the upload and
        // its parts; the peer holds the `multipart` tombstone. The upload must
        // be deleted (cascading parts) and the peer's part rows — if any were
        // still in flight — must not be adopted.
        let local = ControlSnapshot {
            multipart_uploads: vec![upload("up1", 100)],
            parts: vec![part("up1", 1, 110)],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            tombstones: vec![tomb(TOMBSTONE_MULTIPART, "up1", 200)],
            parts: vec![part("up1", 2, 120)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.delete_multipart_uploads, vec!["up1".to_string()]);
        assert!(plan.upsert_parts.is_empty(), "closed upload's parts not adopted");
        assert!(plan
            .adopt_tombstones
            .iter()
            .any(|t| t.entity_type == TOMBSTONE_MULTIPART));
    }

    #[test]
    fn merge_multipart_catchup_adopts_upload_and_parts() {
        // A node that was down during CreateMultipartUpload + UploadPart pulls
        // both rows at re-entry; a re-uploaded part replaces by newer ts.
        let local = ControlSnapshot {
            multipart_uploads: vec![upload("up1", 100)],
            parts: vec![part("up1", 1, 110)],
            ..Default::default()
        };
        let remote = ControlSnapshot {
            multipart_uploads: vec![upload("up1", 100), upload("up2", 150)],
            parts: vec![part("up1", 1, 180), part("up2", 1, 160)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert_eq!(plan.upsert_multipart_uploads.len(), 1);
        assert_eq!(plan.upsert_multipart_uploads[0].upload_id, "up2");
        // Both the re-uploaded part (newer ts) and the new upload's part land.
        assert_eq!(plan.upsert_parts.len(), 2);
        // Re-running the same merge after convergence is a no-op for parts
        // with equal timestamps (ties keep local).
        let plan2 = plan_control_merge(&remote, &remote);
        assert!(plan2.upsert_parts.is_empty());
        assert!(plan2.is_empty());
    }

    #[test]
    fn merge_legacy_snapshot_without_r5_families_deletes_nothing() {
        // Rolling upgrade (H10): a pre-R5 peer's snapshot deserializes with the
        // new families empty. That must read as "no information", never as
        // "everything was deleted".
        let legacy_json = r#"{
            "credentials": [], "users": [], "teams": [], "grants": [],
            "buckets": [], "tombstones": []
        }"#;
        let remote: ControlSnapshot = serde_json::from_str(legacy_json).unwrap();
        let local = ControlSnapshot {
            users: vec![tuser("u1", 50)],
            grants: vec![tgrant("g1", 50)],
            user_grants: vec![tug("u1", "g1", 100)],
            multipart_uploads: vec![upload("up1", 100)],
            parts: vec![part("up1", 1, 110)],
            ..Default::default()
        };
        let plan = plan_control_merge(&local, &remote);
        assert!(plan.delete_user_grants.is_empty());
        assert!(plan.delete_multipart_uploads.is_empty());
        assert!(plan.adopt_tombstones.is_empty());
    }

    // --- ping challenge-response MAC (decision H12) -------------------------

    #[test]
    fn ping_mac_roundtrip_and_tamper() {
        let mac = ping_nonce_mac("supersecret", "nonce-123");
        assert_eq!(mac.len(), 64, "hex-encoded HMAC-SHA256");
        assert!(verify_ping_nonce_mac("supersecret", "nonce-123", &mac));
        // Wrong secret, wrong nonce, tampered/garbage MAC: all rejected.
        assert!(!verify_ping_nonce_mac("other-secret", "nonce-123", &mac));
        assert!(!verify_ping_nonce_mac("supersecret", "nonce-456", &mac));
        let tampered = format!("{}{}", &mac[..63], if &mac[63..] == "0" { "1" } else { "0" });
        assert!(!verify_ping_nonce_mac("supersecret", "nonce-123", &tampered));
        assert!(!verify_ping_nonce_mac("supersecret", "nonce-123", "not-hex"));
        assert!(!verify_ping_nonce_mac("supersecret", "nonce-123", ""));
    }

    #[test]
    fn ping_mac_is_deterministic_and_nonce_sensitive() {
        assert_eq!(ping_nonce_mac("s", "n"), ping_nonce_mac("s", "n"));
        assert_ne!(ping_nonce_mac("s", "n1"), ping_nonce_mac("s", "n2"));
        assert_ne!(ping_nonce_mac("s1", "n"), ping_nonce_mac("s2", "n"));
    }

    #[test]
    fn ping_response_serde_roundtrip_and_legacy_tolerance() {
        let resp = ClusterPingResponse {
            status: "ok".to_string(),
            node_id: "n1".to_string(),
            config_fingerprint: Some("ab12cd34".to_string()),
            disk_total: Some(100),
            disk_available: Some(40),
            max_seq: 7,
            nonce_mac: Some("deadbeef".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: ClusterPingResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node_id, "n1");
        assert_eq!(back.max_seq, 7);
        assert_eq!(back.nonce_mac.as_deref(), Some("deadbeef"));
        // Additive wire change (H10): a minimal body still parses.
        let minimal: ClusterPingResponse =
            serde_json::from_str(r#"{"status":"ok","node_id":"n2"}"#).unwrap();
        assert_eq!(minimal.node_id, "n2");
        assert_eq!(minimal.max_seq, 0);
        assert!(minimal.nonce_mac.is_none());
    }

    // --- peer eligibility (decisions H12/H7 — §3.7(A), D1) ------------------

    #[test]
    fn eligible_requires_alive_authenticated_and_aligned() {
        let mut p = peer("n2", true);
        assert!(p.eligible());
        p.authenticated = false; // a rogue / legacy peer: alive, not eligible
        assert!(!p.eligible());
        p.authenticated = true;
        p.config_ok = false; // drifted config: not eligible (H7)
        assert!(!p.eligible());
        p.config_ok = true;
        p.alive = false;
        assert!(!p.eligible());
    }

    #[test]
    fn unauthenticated_peer_does_not_sustain_quorum() {
        // 3-node quorum cluster: self + 1 eligible peer = quorum 2.
        let state = ClusterState::new("self", Some(2), Some(3));
        let mut rogue = peer("rogue", true);
        rogue.authenticated = false;
        state.set_peers(vec![rogue]);
        // The rogue is alive (visible) but must not make the quorum (ghost
        // quorum, review §3.7(A)): counting it would fan out all new writes
        // to a node that never proved possession of the secret.
        assert_eq!(state.live_node_count(), 2);
        assert_eq!(state.eligible_node_count(), 1);
        assert!(!state.has_write_quorum());
        // A real (authenticated) peer restores it.
        state.set_peers(vec![peer("n2", true)]);
        assert!(state.has_write_quorum());
    }

    #[test]
    fn drifted_peer_does_not_sustain_quorum() {
        // Decision H7: config_ok=false stays visible but exits the quorum.
        let state = ClusterState::new("self", Some(2), Some(3));
        let mut drifted = peer("n2", true);
        drifted.config_ok = false;
        state.set_peers(vec![drifted]);
        assert!(!state.has_write_quorum());
        assert_eq!(state.eligible_node_count(), 1);
        assert_eq!(state.live_node_count(), 2, "still visible as alive");
    }

    // --- cluster_size write gate (decision H6 — D3a) -------------------------

    #[test]
    fn size_exceeded_closes_the_write_gate() {
        // cluster_size = 3, quorum = 2, but FOUR eligible nodes are live: two
        // disjoint pairs could both reach "quorum 2" — split-brain. Fail closed.
        let state = ClusterState::new("self", Some(2), Some(3));
        state.set_peers(vec![peer("n2", true), peer("n3", true), peer("n4", true)]);
        assert_eq!(
            state.write_gate(),
            WriteGate::SizeExceeded {
                eligible: 4,
                cluster_size: 3
            }
        );
        assert!(!state.has_write_quorum());
        let snap = state.snapshot();
        assert!(snap.size_exceeded);
        assert!(!snap.has_write_quorum);

        // Exactly cluster_size eligible nodes: open.
        state.set_peers(vec![peer("n2", true), peer("n3", true)]);
        assert_eq!(state.write_gate(), WriteGate::Open);
        assert!(!state.snapshot().size_exceeded);
    }

    #[test]
    fn size_gate_ignores_non_eligible_extras() {
        // A 4th node that is alive but NOT authenticated must not close the
        // gate (otherwise a rogue mDNS registrant could DoS all writes).
        let state = ClusterState::new("self", Some(2), Some(3));
        let mut rogue = peer("rogue", true);
        rogue.authenticated = false;
        state.set_peers(vec![peer("n2", true), peer("n3", true), rogue]);
        assert_eq!(state.write_gate(), WriteGate::Open);
    }

    #[test]
    fn available_mode_never_size_gates() {
        // H6 applies to quorum mode only (available mode has no derived
        // majority to corrupt).
        let state = ClusterState::new("self", None, None);
        state.set_peers(vec![
            peer("n2", true),
            peer("n3", true),
            peer("n4", true),
            peer("n5", true),
        ]);
        assert_eq!(state.write_gate(), WriteGate::Open);
    }

    #[test]
    fn no_quorum_gate_reports_counts() {
        let state = ClusterState::new("self", Some(2), Some(3));
        assert_eq!(
            state.write_gate(),
            WriteGate::NoQuorum {
                eligible: 1,
                quorum: 2
            }
        );
    }

    // --- min_disk eligibility ------------------------------------------------

    #[test]
    fn min_disk_ignores_non_eligible_peers() {
        let state = ClusterState::new("self", None, None);
        // An unauthenticated "peer" advertising a tiny disk must not drag the
        // cluster minimum down (capacity-guard DoS).
        let mut rogue = peer("rogue", true);
        rogue.authenticated = false;
        rogue.disk_total = Some(1);
        rogue.disk_available = Some(1);
        let mut drifted = peer("n3", true);
        drifted.config_ok = false;
        drifted.disk_total = Some(2);
        drifted.disk_available = Some(2);
        state.set_peers(vec![rogue, drifted]);
        assert_eq!(state.min_disk(Some(100), Some(50)), (Some(100), Some(50)));
    }

    // --- tombstone GC liveness guard (review §3.2) ---------------------------

    #[test]
    fn gc_blockers_flags_peers_unseen_beyond_grace() {
        let now = ts(10_000);
        let grace = chrono::Duration::seconds(1_000);

        let fresh_alive = peer("n2", true);
        let mut dead_recent = peer("n3", false);
        dead_recent.last_seen = Some(ts(9_500)); // within grace → safe
        let mut dead_stale = peer("n4", false);
        dead_stale.last_seen = Some(ts(8_000)); // beyond grace → blocks
        let mut dead_unknown = peer("n5", false);
        dead_unknown.last_seen = None; // never contacted → conservative block

        let blockers = tombstone_gc_blockers(
            &[fresh_alive, dead_recent, dead_stale, dead_unknown],
            now,
            grace,
        );
        let ids: Vec<&str> = blockers.iter().map(|p| p.node_id.as_str()).collect();
        assert_eq!(ids, vec!["n4", "n5"]);
    }

    #[test]
    fn gc_blockers_empty_when_all_seen_recently() {
        let now = ts(10_000);
        let grace = chrono::Duration::seconds(1_000);
        let mut dead_recent = peer("n2", false);
        dead_recent.last_seen = Some(ts(9_999));
        assert!(tombstone_gc_blockers(&[peer("n3", true), dead_recent], now, grace).is_empty());
    }

    #[test]
    fn snapshot_carries_gc_blocked_flag() {
        let state = ClusterState::new("self", None, None);
        assert!(!state.snapshot().tombstone_gc_blocked);
        state.set_tombstone_gc_blocked(true);
        assert!(state.snapshot().tombstone_gc_blocked);
        state.set_tombstone_gc_blocked(false);
        assert!(!state.snapshot().tombstone_gc_blocked);
    }

    #[test]
    fn peer_node_deserializes_legacy_payload_as_unauthenticated() {
        // A payload without the `authenticated` field (pre-R3 producer) must
        // default to NOT authenticated — the secure default.
        let json = r#"{"node_id":"n2","endpoint":"http://n2:9000","alive":true,"last_seen":null}"#;
        let p: PeerNode = serde_json::from_str(json).unwrap();
        assert!(!p.authenticated);
        assert!(p.config_ok, "config_ok keeps its benign default");
        assert_eq!(p.max_seq, None, "no seq report from a legacy payload");
    }

    // --- syncing readiness (review D2) ---------------------------------------

    #[test]
    fn syncing_until_first_pass_completes_toward_every_eligible_peer() {
        let state = ClusterState::new("self", Some(2), Some(3));
        // No peers at all: degraded-but-ready, never syncing.
        assert!(!state.is_syncing());
        assert!(!state.snapshot().syncing);

        state.set_peers(vec![peer("n2", true), peer("n3", true)]);
        assert!(state.is_syncing(), "eligible peers never reconciled from");
        assert!(state.snapshot().syncing);

        state.record_reconcile_complete("n2", ts(100));
        assert!(state.is_syncing(), "one eligible peer still pending");
        state.record_reconcile_complete("n3", ts(101));
        assert!(!state.is_syncing(), "first pass done toward every eligible peer");
        assert!(!state.snapshot().syncing);

        // first_pass_done is latched: later passes only refresh last_reconcile.
        state.record_reconcile_complete("n2", ts(200));
        let s = state.peer_sync("n2");
        assert!(s.first_pass_done);
        assert_eq!(s.last_reconcile, Some(ts(200)));
    }

    #[test]
    fn syncing_ignores_non_eligible_peers() {
        // A rogue/drifted/dead peer is never reconciled from: requiring a pass
        // toward it would deadlock readiness forever.
        let state = ClusterState::new("self", None, None);
        let mut rogue = peer("rogue", true);
        rogue.authenticated = false;
        let mut drifted = peer("n3", true);
        drifted.config_ok = false;
        state.set_peers(vec![rogue, drifted, peer("n4", false)]);
        assert!(!state.is_syncing());
    }

    #[test]
    fn sync_hwm_roundtrip_skip_counter_and_prune() {
        let state = ClusterState::new("self", None, None);
        state.set_peers(vec![peer("n2", true), peer("n3", true)]);

        assert_eq!(state.peer_sync("n2").hwm, 0, "zeroed default");
        state.set_sync_hwm("n2", 42, ts(100));
        assert_eq!(state.peer_sync("n2").hwm, 42);
        assert_eq!(state.peer_sync("n2").hwm_at, Some(ts(100)));

        state.record_skipped_entry("n2");
        state.record_skipped_entry("n2");
        assert_eq!(state.peer_sync("n2").skipped_entries, 2);

        state.reset_sync_hwm("n2");
        let s = state.peer_sync("n2");
        assert_eq!(s.hwm, 0);
        assert_eq!(s.hwm_at, None);
        assert_eq!(s.skipped_entries, 2, "reset touches only the HWM");

        // A dead-but-known peer keeps its status; an unknown one is pruned.
        state.set_sync_hwm("n3", 7, ts(100));
        state.set_peers(vec![peer("n2", true), peer("n3", false)]);
        assert_eq!(state.peer_sync("n3").hwm, 7, "dead peer keeps its HWM");
        state.set_peers(vec![peer("n2", true)]);
        assert_eq!(state.peer_sync("n3").hwm, 0, "pruned peer's status dropped");
        assert_eq!(state.sync_status().len(), 1);
    }

    // --- restore/rewind detection (review D3c) --------------------------------

    #[test]
    fn rewind_detected_only_on_fresh_lower_report() {
        // Genuine rewind: the peer's report is FRESHER than our last HWM
        // advance and still lower than the HWM.
        assert!(sync_rewound(100, Some(ts(50)), Some(40), Some(ts(60))));
        // Stale report: taken before our last advance — under sustained writes
        // this is routine, not a rewind.
        assert!(!sync_rewound(100, Some(ts(50)), Some(40), Some(ts(40))));
        // Equal timestamps: not strictly fresher → not judged.
        assert!(!sync_rewound(100, Some(ts(50)), Some(40), Some(ts(50))));
        // Counter at or above the HWM: normal operation.
        assert!(!sync_rewound(100, Some(ts(50)), Some(100), Some(ts(60))));
        assert!(!sync_rewound(100, Some(ts(50)), Some(500), Some(ts(60))));
        // Nothing reported / never contacted: nothing to judge.
        assert!(!sync_rewound(100, Some(ts(50)), None, Some(ts(60))));
        assert!(!sync_rewound(100, Some(ts(50)), Some(40), None));
        // Fresh start (hwm 0): a rewind below 0 is impossible.
        assert!(!sync_rewound(0, None, Some(0), Some(ts(60))));
    }
}
