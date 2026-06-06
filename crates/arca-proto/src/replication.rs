//! Journal emission helper used by object handlers.
//!
//! When a client write (PutObject / CompleteMultipartUpload / DeleteObject-
//! delete-marker / PutObjectTagging) succeeds on a bucket that has
//! replication rules, the handler calls [`maybe_emit`] to decide — based on
//! the replication configuration and the incoming headers — whether to
//! insert a journal row and mark the object as `PENDING`.
//!
//! Loop prevention: if the incoming request carries a non-empty
//! [`REPLICATION_SOURCE_HEADER`], we treat the object as a replica (set
//! `replication_status = REPLICA` on it) and do NOT insert a journal entry.

use std::sync::Arc;

use arca_core::s3::replication::{
    ReplicationConfiguration, ReplicationFilter, ReplicationStatus, RuleStatus,
    REPLICATION_SOURCE_HEADER,
};
use arca_core::store::metadata::MetadataStore;
use arca_core::store::replication::{JournalEntry, ReplicationEventType, ReplicationStore};
use chrono::Utc;

/// Read the current replication configuration for a bucket, or `None` if
/// replication is not configured / config is malformed.
pub async fn fetch_replication_config(
    metadata: &dyn MetadataStore,
    bucket: &str,
) -> Option<ReplicationConfiguration> {
    match metadata
        .get_bucket_config(bucket, "replication_configuration")
        .await
    {
        Ok(Some(json)) => match serde_json::from_str::<ReplicationConfiguration>(&json) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                tracing::warn!(
                    bucket = %bucket,
                    error = %e,
                    "replication: corrupted configuration in bucket_config"
                );
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                bucket = %bucket,
                error = %e,
                "replication: failed to read bucket_config"
            );
            None
        }
    }
}

/// Is the given header list carrying the arca replication-source marker?
/// Used to detect an incoming replicated write and skip journal emission.
pub fn is_replica_write(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case(REPLICATION_SOURCE_HEADER) && !v.is_empty()
    })
}

/// Inputs to [`maybe_emit`].
pub struct EmitInput<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub version_id: Option<&'a str>,
    pub event_type: ReplicationEventType,
    /// Object tags currently associated with this object (for tag-filter evaluation).
    pub tags: &'a [(String, String)],
    /// Normalized (lowercase) request headers on the originating write. The
    /// handler is responsible for lowercasing keys before passing.
    pub request_headers: &'a [(String, String)],
}

/// Decide whether to emit replication journal entries for a write, and do so.
///
/// Returns `Some(ReplicationStatus)` when a status should be stamped on the
/// object record (either `PENDING` because journal rows were inserted, or
/// `REPLICA` because this is an incoming replica). `None` means this bucket
/// has no matching rule; the handler should leave the object's
/// `replication_status` column untouched.
pub async fn maybe_emit(
    metadata: &dyn MetadataStore,
    replication_store: &Arc<dyn ReplicationStore>,
    input: &EmitInput<'_>,
) -> Option<ReplicationStatus> {
    // Loop prevention: if this write came in with the arca replication-source
    // marker, the object is a REPLICA. Return that status WITHOUT emitting
    // any journal rows.
    if is_replica_write(input.request_headers) {
        return Some(ReplicationStatus::Replica);
    }

    let config = fetch_replication_config(metadata, input.bucket).await?;
    if config.rules.is_empty() {
        return None;
    }

    let now = Utc::now();
    let mut any_inserted = false;

    for rule in &config.rules {
        if rule.status != RuleStatus::Enabled {
            continue;
        }
        if !rule_applies(rule, input) {
            continue;
        }

        // Delete-marker replication is gated by its own flag on the rule.
        if input.event_type == ReplicationEventType::DeleteMarker
            && rule.delete_marker_replication != RuleStatus::Enabled
        {
            continue;
        }

        let entry = JournalEntry {
            id: uuid::Uuid::new_v4().to_string(),
            bucket: input.bucket.to_string(),
            key: input.key.to_string(),
            version_id: input.version_id.map(|s| s.to_string()),
            rule_id: rule.id.clone(),
            event_type: input.event_type.as_db().to_string(),
            destination_endpoint: rule.destination.endpoint.clone(),
            destination_bucket: rule.destination.bucket.clone(),
            status: "pending".to_string(),
            attempts: 0,
            last_error: None,
            next_retry_at: now,
            created_at: now,
            updated_at: now,
        };

        match replication_store.insert_journal_entry(&entry).await {
            Ok(()) => any_inserted = true,
            Err(e) => {
                tracing::warn!(
                    bucket = %input.bucket,
                    key = %input.key,
                    rule = %rule.id,
                    error = %e,
                    "replication: failed to insert journal entry (dropping)"
                );
            }
        }
    }

    if any_inserted {
        Some(ReplicationStatus::Pending)
    } else {
        None
    }
}

fn rule_applies(
    rule: &arca_core::s3::replication::ReplicationRule,
    input: &EmitInput<'_>,
) -> bool {
    // Prefix/tag filter.
    let tag_owned: Vec<(String, String)> = input
        .tags
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !matches_filter(&rule.filter, input.key, &tag_owned) {
        return false;
    }
    true
}

fn matches_filter(
    filter: &ReplicationFilter,
    key: &str,
    tags: &[(String, String)],
) -> bool {
    filter.matches(key, tags)
}

/// Convenience wrapper used by handlers. Extracts the needed headers from
/// `http::HeaderMap`, calls [`maybe_emit`], and (when applicable) stamps the
/// object's `replication_status` column. Returns the value to put on the
/// `x-amz-replication-status` response header, if any.
///
/// The caller passes the full request HeaderMap; we extract only the
/// replication-source marker to keep the emit logic small.
pub async fn emit_and_stamp(
    state: &crate::state::AppState,
    headers: &http::HeaderMap,
    bucket: &str,
    key: &str,
    version_id: Option<&str>,
    event_type: ReplicationEventType,
    tags: &[(String, String)],
) -> Option<ReplicationStatus> {
    let header_pairs = extract_replication_headers(headers);
    // Common case: a bucket with no replication configured skips the whole emit
    // path (including the `bucket_config` read inside `maybe_emit`). The result
    // is cached for 30s in AppState, mirroring the per-bucket encryption cache.
    // Replica writes must still be stamped REPLICA, so they are never
    // short-circuited here.
    if !is_replica_write(&header_pairs) && !state.replication_enabled_for(bucket).await {
        return None;
    }
    let input = EmitInput {
        bucket,
        key,
        version_id,
        event_type,
        tags,
        request_headers: &header_pairs,
    };
    let status = maybe_emit(state.metadata.as_ref(), &state.replication_store, &input).await?;

    // For REPLICA or PENDING, write the status column on the object record so
    // subsequent GETs/HEADs reflect it.
    if let Err(e) = state
        .replication_store
        .set_object_replication_status(bucket, key, version_id, Some(status.as_header()))
        .await
    {
        tracing::warn!(
            bucket = %bucket,
            key = %key,
            error = %e,
            "replication: failed to stamp replication_status on object"
        );
    }
    Some(status)
}

/// Extract just the replication-related headers (lowercased) from a full HeaderMap.
/// Only the `x-amz-arca-replication-source` marker matters for the emit logic.
fn extract_replication_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let n = name.as_str().to_ascii_lowercase();
            if n == REPLICATION_SOURCE_HEADER {
                let v = value.to_str().unwrap_or("").to_string();
                Some((n, v))
            } else {
                None
            }
        })
        .collect()
}
