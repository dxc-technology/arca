//! Background replication worker.
//!
//! Periodically claims a batch of journal entries whose `next_retry_at` has
//! elapsed, delivers each to its destination endpoint via the outbound S3
//! client, and updates the entry's status plus the object's
//! `replication_status` column.

use std::sync::Arc;
use std::time::Duration;

use arca_core::s3::replication::{ReplicationConfiguration, ReplicationStatus};
use arca_core::store::blob::BlobStore;
use arca_core::store::metadata::MetadataStore;
use arca_core::store::replication::{JournalEntry, ReplicationEventType, ReplicationStore};
use arca_core::store::server_config::ServerConfigStore;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::Receiver;

use crate::config::ReplicationConfig;
use crate::replicator::client::{OutboundClient, OutboundError, OutboundOp};
use crate::worker::BackgroundWorker;

/// Credential pair fetched from `server_config` at delivery time.
struct DestCreds {
    access_key_id: String,
    secret_access_key: String,
}

/// Spawn the replication worker. The worker runs as long as the returned
/// `BackgroundWorker` handle is kept alive.
///
/// `wake_rx` is an optional mpsc receiver that the emit code can signal to
/// wake the worker immediately after a new entry is inserted (instead of
/// waiting for the next tick). `None` is acceptable — the worker then
/// polls on the timer alone.
pub fn spawn_replication_worker(
    metadata: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStore>,
    replication_store: Arc<dyn ReplicationStore>,
    server_config: Arc<dyn ServerConfigStore>,
    config: ReplicationConfig,
    mut wake_rx: Option<Receiver<()>>,
) -> BackgroundWorker {
    let handle = tokio::spawn(async move {
        let client = match OutboundClient::new(
            config.source_endpoint_id.clone(),
            Duration::from_secs(config.request_timeout_seconds),
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "replication worker: failed to create HTTP client, exiting");
                return;
            }
        };

        let mut timer = tokio::time::interval(Duration::from_secs(config.poll_interval_seconds));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick.
        timer.tick().await;

        loop {
            // Wait for either the timer or an explicit wake-up from emit.
            match wake_rx.as_mut() {
                Some(rx) => {
                    tokio::select! {
                        _ = timer.tick() => {}
                        _ = rx.recv() => {
                            // Drain any queued wake-ups so we don't loop tightly.
                            while rx.try_recv().is_ok() {}
                        }
                    }
                }
                None => {
                    timer.tick().await;
                }
            }

            let batch = match replication_store.claim_batch(config.batch_size).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "replication: claim_batch failed");
                    continue;
                }
            };
            if batch.is_empty() {
                continue;
            }
            tracing::debug!(entries = batch.len(), "replication: processing batch");

            for entry in batch {
                process_entry(
                    &client,
                    metadata.as_ref(),
                    blob.as_ref(),
                    &replication_store,
                    server_config.as_ref(),
                    &config,
                    &entry,
                )
                .await;
            }
        }
    });
    BackgroundWorker::from_handle(handle)
}

async fn process_entry(
    client: &OutboundClient,
    metadata: &dyn MetadataStore,
    blob: &dyn BlobStore,
    replication_store: &Arc<dyn ReplicationStore>,
    server_config: &dyn ServerConfigStore,
    config: &ReplicationConfig,
    entry: &JournalEntry,
) {
    let event_type = match ReplicationEventType::parse(&entry.event_type) {
        Some(t) => t,
        None => {
            mark_failed(
                replication_store,
                entry,
                "unknown event_type".to_string(),
                config,
            )
            .await;
            return;
        }
    };

    let rep_config = match arca_proto::replication::fetch_replication_config(metadata, &entry.bucket).await {
        Some(c) => c,
        None => {
            // Rule was removed; drop the entry by marking it completed.
            complete(replication_store, entry).await;
            return;
        }
    };
    let rule = match find_rule(&rep_config, &entry.rule_id) {
        Some(r) => r,
        None => {
            complete(replication_store, entry).await;
            return;
        }
    };

    let creds = match fetch_credentials(server_config, &rule.destination.credential_ref).await {
        Some(c) => c,
        None => {
            mark_failed(
                replication_store,
                entry,
                format!(
                    "missing credential '{}' for rule '{}'",
                    rule.destination.credential_ref, rule.id
                ),
                config,
            )
            .await;
            return;
        }
    };

    // Conflict-resolution: HEAD the destination first, skip if it's newer.
    let destination_newer = match client
        .head(
            &rule.destination.endpoint,
            &rule.destination.bucket,
            &entry.key,
            &creds.access_key_id,
            &creds.secret_access_key,
            &rule.destination.region,
        )
        .await
    {
        Ok(head) => match (head.last_modified, local_last_modified(metadata, entry).await) {
            (Some(dest), Some(src)) => dest >= src,
            _ => false,
        },
        Err(e) => {
            // Don't block on HEAD failures — try the write anyway.
            tracing::debug!(error = %e, "replication: HEAD failed, proceeding with write");
            false
        }
    };
    if destination_newer && event_type == ReplicationEventType::Put {
        tracing::debug!(
            bucket = %entry.bucket,
            key = %entry.key,
            "replication: destination has newer last-modified, skipping"
        );
        complete(replication_store, entry).await;
        stamp_object(replication_store, entry, ReplicationStatus::Completed).await;
        return;
    }

    // Dispatch per event type.
    let delivery_result = match event_type {
        ReplicationEventType::Put => {
            deliver_put(client, blob, metadata, &rule.destination.endpoint,
                &rule.destination.bucket, &rule.destination.region, entry,
                &creds).await
        }
        ReplicationEventType::DeleteMarker => {
            client
                .execute(
                    &OutboundOp::Delete,
                    &rule.destination.endpoint,
                    &rule.destination.bucket,
                    &entry.key,
                    Vec::new(),
                    &creds.access_key_id,
                    &creds.secret_access_key,
                    &rule.destination.region,
                )
                .await
        }
        ReplicationEventType::Tag => {
            deliver_tags(client, metadata, &rule.destination.endpoint,
                &rule.destination.bucket, &rule.destination.region, entry,
                &creds).await
        }
    };

    match delivery_result {
        Ok(()) => {
            complete(replication_store, entry).await;
            stamp_object(replication_store, entry, ReplicationStatus::Completed).await;
        }
        Err(e) => {
            mark_failed(replication_store, entry, e.to_string(), config).await;
        }
    }
}

async fn deliver_put(
    client: &OutboundClient,
    blob: &dyn BlobStore,
    metadata: &dyn MetadataStore,
    endpoint: &str,
    dest_bucket: &str,
    region: &str,
    entry: &JournalEntry,
    creds: &DestCreds,
) -> Result<(), OutboundError> {
    // Fetch the object record + blob for streaming.
    let record = match entry.version_id.as_deref() {
        Some(vid) => metadata
            .get_object_version(&entry.bucket, &entry.key, vid)
            .await
            .map_err(|e| OutboundError::Network(format!("metadata.get_object_version: {e}")))?,
        None => metadata
            .get_object(&entry.bucket, &entry.key)
            .await
            .map_err(|e| OutboundError::Network(format!("metadata.get_object: {e}")))?,
    };
    let record = match record {
        Some(r) => r,
        None => {
            // Object was deleted after journal entry was written — nothing to do.
            return Ok(());
        }
    };

    let get = blob
        .get(&record.blob_id, None)
        .await
        .map_err(|e| OutboundError::Network(format!("blob.get: {e}")))?;

    // Collect the stream into memory. Objects up to `max_body_size` are fine;
    // streaming the reqwest body from an async source would be stronger but is
    // an MVP trade-off for Phase 28.
    let bytes = collect_stream(get.stream)
        .await
        .map_err(|e| OutboundError::Network(format!("blob stream: {e}")))?;

    let content_type = record.content_type.clone();
    let user_meta: Vec<(String, String)> = record
        .metadata
        .iter()
        .map(|(k, v)| (format!("x-amz-meta-{k}"), v.clone()))
        .collect();

    let op = OutboundOp::Put {
        content_length: bytes.len() as u64,
        content_type,
        user_metadata: user_meta,
    };
    client
        .execute(
            &op,
            endpoint,
            dest_bucket,
            &entry.key,
            bytes,
            &creds.access_key_id,
            &creds.secret_access_key,
            region,
        )
        .await
}

async fn deliver_tags(
    client: &OutboundClient,
    metadata: &dyn MetadataStore,
    endpoint: &str,
    dest_bucket: &str,
    region: &str,
    entry: &JournalEntry,
    creds: &DestCreds,
) -> Result<(), OutboundError> {
    let vid = entry.version_id.as_deref().unwrap_or("");
    let tags = metadata
        .get_object_tags(&entry.bucket, &entry.key, vid)
        .await
        .map_err(|e| OutboundError::Network(format!("metadata.get_object_tags: {e}")))?;

    let body = serialize_tagging_xml(&tags);
    client
        .execute(
            &OutboundOp::PutTagging,
            endpoint,
            dest_bucket,
            &entry.key,
            body.into_bytes(),
            &creds.access_key_id,
            &creds.secret_access_key,
            region,
        )
        .await
}

fn serialize_tagging_xml(tags: &[(String, String)]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Tagging><TagSet>",
    );
    for (k, v) in tags {
        out.push_str("<Tag><Key>");
        out.push_str(&xml_escape(k));
        out.push_str("</Key><Value>");
        out.push_str(&xml_escape(v));
        out.push_str("</Value></Tag>");
    }
    out.push_str("</TagSet></Tagging>");
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn collect_stream(
    mut stream: arca_core::store::blob::ByteStream,
) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

async fn local_last_modified(
    metadata: &dyn MetadataStore,
    entry: &JournalEntry,
) -> Option<DateTime<Utc>> {
    let record = match entry.version_id.as_deref() {
        Some(vid) => metadata
            .get_object_version(&entry.bucket, &entry.key, vid)
            .await
            .ok()
            .flatten(),
        None => metadata
            .get_object(&entry.bucket, &entry.key)
            .await
            .ok()
            .flatten(),
    };
    record.map(|r| r.last_modified)
}

fn find_rule<'a>(
    config: &'a ReplicationConfiguration,
    rule_id: &str,
) -> Option<&'a arca_core::s3::replication::ReplicationRule> {
    config.rules.iter().find(|r| r.id == rule_id)
}

async fn fetch_credentials(
    server_config: &dyn ServerConfigStore,
    credential_ref: &str,
) -> Option<DestCreds> {
    let key = format!("replication.credentials.{credential_ref}");
    let value = server_config.get_server_config(&key).await.ok().flatten()?;
    // Stored as "access_key_id:secret_access_key" (colon-separated).
    let (ak, sk) = value.split_once(':')?;
    Some(DestCreds {
        access_key_id: ak.to_string(),
        secret_access_key: sk.to_string(),
    })
}

async fn complete(store: &Arc<dyn ReplicationStore>, entry: &JournalEntry) {
    if let Err(e) = store
        .update_status(&entry.id, "completed", entry.attempts, None, Utc::now())
        .await
    {
        tracing::warn!(id = %entry.id, error = %e, "replication: update_status(completed) failed");
    }
}

async fn mark_failed(
    store: &Arc<dyn ReplicationStore>,
    entry: &JournalEntry,
    error: String,
    config: &ReplicationConfig,
) {
    let attempts = entry.attempts + 1;
    let terminal = attempts >= config.max_retries;
    let status = if terminal { "failed" } else { "failed" };

    // Exponential backoff capped at 1h.
    let delay_secs = if terminal {
        0
    } else {
        std::cmp::min(
            config.retry_base_seconds * (1u64 << attempts.saturating_sub(1).min(12)),
            3600,
        )
    };
    let next_retry = Utc::now() + chrono::Duration::seconds(delay_secs as i64);

    if let Err(e) = store
        .update_status(&entry.id, status, attempts, Some(&error), next_retry)
        .await
    {
        tracing::warn!(id = %entry.id, error = %e, "replication: update_status(failed) failed");
    }
    if terminal {
        // Only stamp the object FAILED after the last attempt, so the client
        // isn't briefly told "FAILED" during transient errors.
        let obj_status = ReplicationStatus::Failed.as_header();
        if let Err(e) = store
            .set_object_replication_status(
                &entry.bucket,
                &entry.key,
                entry.version_id.as_deref(),
                Some(obj_status),
            )
            .await
        {
            tracing::warn!(id = %entry.id, error = %e, "replication: set_object_replication_status(FAILED) failed");
        }
    }
    tracing::debug!(
        id = %entry.id,
        attempts,
        delay_secs,
        error = %error,
        "replication: delivery attempt failed"
    );
}

async fn stamp_object(
    store: &Arc<dyn ReplicationStore>,
    entry: &JournalEntry,
    status: ReplicationStatus,
) {
    if let Err(e) = store
        .set_object_replication_status(
            &entry.bucket,
            &entry.key,
            entry.version_id.as_deref(),
            Some(status.as_header()),
        )
        .await
    {
        tracing::warn!(id = %entry.id, error = %e, "replication: set_object_replication_status failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_basic() {
        assert_eq!(xml_escape("a & b"), "a &amp; b");
        assert_eq!(xml_escape("<script>"), "&lt;script&gt;");
        assert_eq!(xml_escape(r#"key"value"#), "key&quot;value");
    }

    #[test]
    fn serialize_tagging_xml_round_trip_shape() {
        let tags = vec![
            ("env".to_string(), "prod".to_string()),
            ("app".to_string(), "a<b&c".to_string()),
        ];
        let xml = serialize_tagging_xml(&tags);
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<Tagging><TagSet>"));
        assert!(xml.contains("<Key>env</Key>"));
        assert!(xml.contains("<Value>a&lt;b&amp;c</Value>"));
        assert!(xml.ends_with("</TagSet></Tagging>"));
    }
}
