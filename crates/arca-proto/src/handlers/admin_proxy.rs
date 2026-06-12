//! `?node=` dispatch for the node-local admin view families (review D6,
//! decision H9).
//!
//! Four admin endpoints serve strictly node-local data: the audit log, the
//! metrics history, the notification event log, and the replication journal.
//! Behind a load balancer the console cannot choose which node answers, so
//! each of those endpoints accepts a `?node=<node_id>` selector:
//!
//! - absent (or this node's own id) → answer from local data, labeled with
//!   this node's id so the operator always knows WHICH node they are seeing;
//! - a peer's id → proxy the same query server-side to that peer's
//!   `POST /cluster/v1/admin/*` route via the signed cluster transport
//!   (browsers cannot reach cluster nodes directly — typically only the LB is
//!   exposed — and the cluster credential never leaves the server side);
//! - `all` → fan the query out to every eligible peer in parallel, merge the
//!   rows by timestamp (newest first), and label every row with its source
//!   node. Pagination is per-source-page: each source is asked for the same
//!   `offset`/`limit` window and the merge keeps the newest `limit` rows
//!   across sources — an approximation, documented rather than papered over
//!   with cross-node cursors.
//!
//! Only ELIGIBLE peers (alive + authenticated + config-aligned) are valid
//! targets, consistent with every other H12 gate: an unauthenticated or
//! drifted node is not part of the cluster for any purpose, reads included.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use arca_core::cluster::{ClusterAdminProxy, ClusterProxyError, PeerNode};

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// The `?node=` value selecting the merged all-nodes view.
pub const NODE_ALL: &str = "all";

/// Static description of one node-local admin view family: the peer route its
/// proxied queries POST to, and the response keys the merge logic needs.
pub struct ProxyFamily {
    /// Fixed `/cluster/v1/admin/*` route on the peer.
    pub path: &'static str,
    /// Key of the rows array in the family's response JSON.
    pub entries_key: &'static str,
    /// Key of the row-count field in the family's response JSON.
    pub total_key: &'static str,
    /// Key of the per-row RFC 3339 timestamp the merged view sorts on.
    pub ts_key: &'static str,
}

/// `GET /admin/audit` ↔ `POST /cluster/v1/admin/audit`.
pub const AUDIT_FAMILY: ProxyFamily = ProxyFamily {
    path: "/cluster/v1/admin/audit",
    entries_key: "entries",
    total_key: "total",
    ts_key: "timestamp",
};

/// `GET /admin/metrics/history` ↔ `POST /cluster/v1/admin/metrics-history`.
pub const METRICS_HISTORY_FAMILY: ProxyFamily = ProxyFamily {
    path: "/cluster/v1/admin/metrics-history",
    entries_key: "snapshots",
    total_key: "count",
    ts_key: "timestamp",
};

/// `GET /admin/notifications/events` ↔ `POST /cluster/v1/admin/notification-events`.
pub const NOTIFICATION_EVENTS_FAMILY: ProxyFamily = ProxyFamily {
    path: "/cluster/v1/admin/notification-events",
    entries_key: "entries",
    total_key: "total",
    ts_key: "created_at",
};

/// `GET /admin/replication/journal` ↔ `POST /cluster/v1/admin/replication-journal`.
pub const REPLICATION_JOURNAL_FAMILY: ProxyFamily = ProxyFamily {
    path: "/cluster/v1/admin/replication-journal",
    entries_key: "entries",
    total_key: "total",
    ts_key: "created_at",
};

/// Resolved `?node=` selector.
#[derive(Debug)]
pub enum NodeSelect {
    /// Answer from this node's own data.
    Local,
    /// Proxy to this eligible peer.
    Peer(PeerNode),
    /// Fan out to every eligible peer and merge.
    All,
}

/// Why a `?node=` value cannot be honored.
#[derive(Debug, PartialEq, Eq)]
pub enum NodeSelectError {
    /// `?node=` was given but this node is not part of a cluster.
    NotClustered,
    /// No known peer carries that node id.
    Unknown(String),
    /// The peer is known but not currently eligible (dead, unauthenticated,
    /// or config-drifted) — it is not a valid target for any cluster purpose.
    Ineligible(String),
}

/// Pure `?node=` resolution over a snapshot of the cluster view:
/// `cluster = (own node_id, known peers)`, `None` when clustering is off.
pub fn select_node(
    node: Option<&str>,
    cluster: Option<(&str, &[PeerNode])>,
) -> Result<NodeSelect, NodeSelectError> {
    let Some(sel) = node.filter(|s| !s.is_empty()) else {
        return Ok(NodeSelect::Local);
    };
    let Some((self_id, peers)) = cluster else {
        return Err(NodeSelectError::NotClustered);
    };
    if sel == self_id {
        return Ok(NodeSelect::Local);
    }
    if sel == NODE_ALL {
        return Ok(NodeSelect::All);
    }
    match peers.iter().find(|p| p.node_id == sel) {
        Some(p) if p.eligible() => Ok(NodeSelect::Peer(p.clone())),
        Some(_) => Err(NodeSelectError::Ineligible(sel.to_string())),
        None => Err(NodeSelectError::Unknown(sel.to_string())),
    }
}

impl NodeSelectError {
    fn into_admin_error(self) -> AdminError {
        match self {
            Self::NotClustered => AdminError::bad_request(
                "?node= requires clustering: this node is not part of a cluster",
            ),
            Self::Unknown(id) => {
                AdminError::not_found(format!("unknown cluster node '{id}'"))
            }
            Self::Ineligible(id) => AdminError::unavailable(format!(
                "cluster node '{id}' is not currently eligible (dead, unauthenticated, or config-drifted)"
            )),
        }
    }
}

/// Per-source outcome of a merged query, reported alongside the merged rows so
/// a partially-failed fan-out is visible instead of silently smaller.
struct SourcePage {
    node_id: String,
    page: Result<serde_json::Value, String>,
}

/// Merges per-node pages into one descending-timestamp page (pure, for unit
/// tests). Every row gains a `node` field naming its source; rows whose
/// timestamp is missing or unparseable sort last. Returns the family-shaped
/// response: rows under `entries_key`, summed totals under `total_key`,
/// `node: "all"`, and a per-source `sources` report (total or error).
fn merge_node_pages(
    pages: Vec<SourcePage>,
    family: &ProxyFamily,
    limit: usize,
) -> serde_json::Value {
    let mut rows: Vec<(DateTime<Utc>, serde_json::Value)> = Vec::new();
    let mut total: u64 = 0;
    let mut sources: Vec<serde_json::Value> = Vec::new();

    for SourcePage { node_id, page } in pages {
        match page {
            Ok(mut page) => {
                let page_entries = page
                    .get_mut(family.entries_key)
                    .and_then(|v| v.as_array_mut())
                    .map(std::mem::take)
                    .unwrap_or_default();
                let page_total = page
                    .get(family.total_key)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(page_entries.len() as u64);
                total += page_total;
                sources.push(serde_json::json!({
                    "node_id": node_id,
                    family.total_key: page_total,
                }));
                for mut entry in page_entries {
                    let ts = entry
                        .get(family.ts_key)
                        .and_then(|v| v.as_str())
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|t| t.with_timezone(&Utc))
                        .unwrap_or(DateTime::<Utc>::MIN_UTC);
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert("node".to_string(), serde_json::json!(node_id));
                    }
                    rows.push((ts, entry));
                }
            }
            Err(error) => {
                sources.push(serde_json::json!({
                    "node_id": node_id,
                    "error": error,
                }));
            }
        }
    }

    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.truncate(limit);
    let entries: Vec<serde_json::Value> = rows.into_iter().map(|(_, e)| e).collect();

    serde_json::json!({
        family.entries_key: entries,
        family.total_key: total,
        "node": NODE_ALL,
        "sources": sources,
    })
}

/// Maps a peer's HTTP-level answer to an admin error: 4xx pass through with
/// the peer's own message (e.g. "audit logging is not enabled" on that node, or
/// a 404 from a pre-R8 peer with no admin proxy routes); anything else becomes
/// a 502 — the peer misbehaved, not the caller.
fn map_peer_error(node_id: &str, err: ClusterProxyError) -> AdminError {
    match err {
        ClusterProxyError::Http { status, body } => {
            // Peer admin errors are `{"error", "message"}` JSON; surface the
            // message when present, the raw body otherwise.
            let detail = serde_json::from_slice::<serde_json::Value>(body.as_bytes())
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or(body);
            let message = format!("node '{node_id}' answered HTTP {status}: {detail}");
            match http::StatusCode::from_u16(status) {
                Ok(code) if code.is_client_error() => AdminError::peer(code, message),
                _ => AdminError::bad_gateway(message),
            }
        }
        ClusterProxyError::Unreachable(e) => {
            AdminError::bad_gateway(format!("node '{node_id}' unreachable: {e}"))
        }
    }
}

/// Proxies one family query to one eligible peer and returns its page, labeled
/// with the peer's node id.
async fn proxy_to_peer(
    proxy: &Arc<dyn ClusterAdminProxy>,
    peer: &PeerNode,
    family: &ProxyFamily,
    body: Vec<u8>,
) -> Result<serde_json::Value, AdminError> {
    let bytes = proxy
        .admin_query(&peer.endpoint, family.path, body)
        .await
        .map_err(|e| map_peer_error(&peer.node_id, e))?;
    let mut page: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        AdminError::bad_gateway(format!(
            "node '{}' returned malformed JSON: {e}",
            peer.node_id
        ))
    })?;
    if let Some(obj) = page.as_object_mut() {
        obj.insert("node".to_string(), serde_json::json!(peer.node_id));
    }
    Ok(page)
}

/// Dispatches one node-local admin query according to its `?node=` selector.
///
/// `params` is the family's filter (re-serialized as the proxied JSON body —
/// the selector itself is `skip_serializing`, so a forwarded query can never
/// re-proxy). `limit` bounds the merged view. `local` produces this node's own
/// page; it runs for the Local branch and as the self source of the merged
/// view, and is exactly what the `/cluster/v1/admin/*` receive handlers call.
pub async fn dispatch<P, F, Fut>(
    state: &AppState,
    family: &'static ProxyFamily,
    node: Option<&str>,
    params: &P,
    limit: usize,
    local: F,
) -> Result<serde_json::Value, AdminError>
where
    P: Serialize,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, AdminError>>,
{
    let cluster_view = state.cluster.as_ref().map(|c| (c.node_id().to_string(), c.peers()));
    let selection = select_node(
        node,
        cluster_view.as_ref().map(|(id, peers)| (id.as_str(), peers.as_slice())),
    )
    .map_err(NodeSelectError::into_admin_error)?;

    match selection {
        NodeSelect::Local => {
            let mut page = local().await?;
            // Label which node answered, so the LB-routed default still tells
            // the operator what they are looking at.
            if let (Some((self_id, _)), Some(obj)) = (&cluster_view, page.as_object_mut()) {
                obj.insert("node".to_string(), serde_json::json!(self_id));
            }
            Ok(page)
        }
        NodeSelect::Peer(peer) => {
            let proxy = state
                .cluster_admin_proxy
                .as_ref()
                .ok_or_else(|| AdminError::internal("cluster admin proxy not wired"))?;
            let body = serde_json::to_vec(params)
                .map_err(|e| AdminError::internal(e.to_string()))?;
            proxy_to_peer(proxy, &peer, family, body).await
        }
        NodeSelect::All => {
            let proxy = state
                .cluster_admin_proxy
                .as_ref()
                .ok_or_else(|| AdminError::internal("cluster admin proxy not wired"))?;
            let body = serde_json::to_vec(params)
                .map_err(|e| AdminError::internal(e.to_string()))?;
            let (self_id, peers) = cluster_view.expect("All implies a cluster view");

            // Query every eligible peer in parallel (§2.4 spirit: one dead
            // peer must not serialize the rest) while the local page builds.
            let handles: Vec<(String, tokio::task::JoinHandle<Result<Vec<u8>, ClusterProxyError>>)> =
                peers
                    .into_iter()
                    .filter(|p| p.eligible())
                    .map(|p| {
                        let proxy = proxy.clone();
                        let body = body.clone();
                        let endpoint = p.endpoint.clone();
                        let path = family.path;
                        (
                            p.node_id,
                            tokio::spawn(async move {
                                proxy.admin_query(&endpoint, path, body).await
                            }),
                        )
                    })
                    .collect();

            let mut pages = vec![SourcePage {
                node_id: self_id,
                page: local().await.map_err(|e| e.message().to_string()),
            }];
            for (node_id, handle) in handles {
                let page = match handle.await {
                    Ok(Ok(bytes)) => serde_json::from_slice(&bytes)
                        .map_err(|e| format!("malformed JSON: {e}")),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(format!("query task failed: {e}")),
                };
                pages.push(SourcePage { node_id, page });
            }
            Ok(merge_node_pages(pages, family, limit))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(node_id: &str, eligible: bool) -> PeerNode {
        PeerNode {
            node_id: node_id.to_string(),
            endpoint: format!("http://{node_id}:9000"),
            alive: eligible,
            last_seen: None,
            authenticated: eligible,
            config_ok: true,
            disk_total: None,
            disk_available: None,
            max_seq: None,
        }
    }

    #[test]
    fn select_no_node_is_local_even_unclustered() {
        assert!(matches!(select_node(None, None), Ok(NodeSelect::Local)));
        assert!(matches!(select_node(Some(""), None), Ok(NodeSelect::Local)));
    }

    #[test]
    fn select_node_requires_cluster() {
        assert!(matches!(
            select_node(Some("n1"), None),
            Err(NodeSelectError::NotClustered)
        ));
    }

    #[test]
    fn select_self_is_local() {
        let peers = [peer("n2", true)];
        assert!(matches!(
            select_node(Some("n1"), Some(("n1", &peers))),
            Ok(NodeSelect::Local)
        ));
    }

    #[test]
    fn select_all_is_all() {
        let peers = [peer("n2", true)];
        assert!(matches!(
            select_node(Some("all"), Some(("n1", &peers))),
            Ok(NodeSelect::All)
        ));
    }

    #[test]
    fn select_eligible_peer() {
        let peers = [peer("n2", true)];
        match select_node(Some("n2"), Some(("n1", &peers))) {
            Ok(NodeSelect::Peer(p)) => assert_eq!(p.node_id, "n2"),
            other => panic!("expected Peer, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn select_ineligible_peer_refused() {
        let peers = [peer("n2", false)];
        assert!(matches!(
            select_node(Some("n2"), Some(("n1", &peers))),
            Err(NodeSelectError::Ineligible(id)) if id == "n2"
        ));
    }

    #[test]
    fn select_unknown_peer_refused() {
        let peers = [peer("n2", true)];
        assert!(matches!(
            select_node(Some("nx"), Some(("n1", &peers))),
            Err(NodeSelectError::Unknown(id)) if id == "nx"
        ));
    }

    fn audit_page(node: &str, stamps: &[&str]) -> SourcePage {
        let entries: Vec<serde_json::Value> = stamps
            .iter()
            .map(|ts| serde_json::json!({"timestamp": ts, "operation": format!("op-{node}")}))
            .collect();
        SourcePage {
            node_id: node.to_string(),
            page: Ok(serde_json::json!({
                "entries": entries,
                "total": stamps.len(),
            })),
        }
    }

    #[test]
    fn merge_orders_newest_first_and_labels_rows() {
        let pages = vec![
            audit_page("n1", &["2026-06-12T10:00:00Z", "2026-06-12T08:00:00Z"]),
            audit_page("n2", &["2026-06-12T09:00:00Z"]),
        ];
        let merged = merge_node_pages(pages, &AUDIT_FAMILY, 10);
        let entries = merged["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        let order: Vec<&str> = entries.iter().map(|e| e["node"].as_str().unwrap()).collect();
        assert_eq!(order, vec!["n1", "n2", "n1"]);
        assert_eq!(merged["total"], 3);
        assert_eq!(merged["node"], "all");
        assert_eq!(merged["sources"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn merge_subsecond_precision_not_fooled_by_string_order() {
        // "...00Z" vs "...00.5Z": lexicographic comparison would order these
        // wrongly ('Z' > '.'), so the merge must parse, not compare strings.
        let pages = vec![
            audit_page("n1", &["2026-06-12T10:00:00Z"]),
            audit_page("n2", &["2026-06-12T10:00:00.500Z"]),
        ];
        let merged = merge_node_pages(pages, &AUDIT_FAMILY, 10);
        let order: Vec<&str> = merged["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["node"].as_str().unwrap())
            .collect();
        assert_eq!(order, vec!["n2", "n1"]);
    }

    #[test]
    fn merge_truncates_to_limit_keeping_newest() {
        let pages = vec![
            audit_page("n1", &["2026-06-12T10:00:00Z", "2026-06-12T07:00:00Z"]),
            audit_page("n2", &["2026-06-12T09:00:00Z", "2026-06-12T06:00:00Z"]),
        ];
        let merged = merge_node_pages(pages, &AUDIT_FAMILY, 2);
        let entries = merged["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["node"], "n1");
        assert_eq!(entries[1]["node"], "n2");
        // The summed total still reports everything that exists.
        assert_eq!(merged["total"], 4);
    }

    #[test]
    fn merge_reports_failed_source_and_keeps_the_rest() {
        let pages = vec![
            audit_page("n1", &["2026-06-12T10:00:00Z"]),
            SourcePage {
                node_id: "n2".to_string(),
                page: Err("peer unreachable: timeout".to_string()),
            },
        ];
        let merged = merge_node_pages(pages, &AUDIT_FAMILY, 10);
        assert_eq!(merged["entries"].as_array().unwrap().len(), 1);
        assert_eq!(merged["total"], 1);
        let sources = merged["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1]["node_id"], "n2");
        assert!(sources[1]["error"].as_str().unwrap().contains("unreachable"));
    }

    #[test]
    fn merge_rows_without_timestamp_sort_last() {
        let mut no_ts = audit_page("n1", &[]);
        no_ts.page = Ok(serde_json::json!({
            "entries": [{"operation": "no-ts"}],
            "total": 1,
        }));
        let pages = vec![no_ts, audit_page("n2", &["2026-06-12T09:00:00Z"])];
        let merged = merge_node_pages(pages, &AUDIT_FAMILY, 10);
        let entries = merged["entries"].as_array().unwrap();
        assert_eq!(entries[0]["node"], "n2");
        assert_eq!(entries[1]["node"], "n1");
    }

    #[test]
    fn merge_metrics_family_uses_snapshots_and_count() {
        let pages = vec![SourcePage {
            node_id: "n1".to_string(),
            page: Ok(serde_json::json!({
                "snapshots": [{"timestamp": "2026-06-12T09:00:00Z", "object_count": 5}],
                "count": 1,
            })),
        }];
        let merged = merge_node_pages(pages, &METRICS_HISTORY_FAMILY, 10);
        assert_eq!(merged["snapshots"].as_array().unwrap().len(), 1);
        assert_eq!(merged["count"], 1);
        assert_eq!(merged["snapshots"][0]["node"], "n1");
    }

    #[test]
    fn peer_4xx_passes_through_with_message() {
        let err = map_peer_error(
            "n2",
            ClusterProxyError::Http {
                status: 400,
                body: r#"{"error":"BadRequest","message":"Audit logging is not enabled"}"#
                    .to_string(),
            },
        );
        assert_eq!(err.status_code(), http::StatusCode::BAD_REQUEST);
        assert!(err.message().contains("Audit logging is not enabled"));
        assert!(err.message().contains("n2"));
    }

    #[test]
    fn peer_5xx_and_unreachable_become_bad_gateway() {
        let err = map_peer_error(
            "n2",
            ClusterProxyError::Http {
                status: 500,
                body: "boom".to_string(),
            },
        );
        assert_eq!(err.status_code(), http::StatusCode::BAD_GATEWAY);

        let err = map_peer_error("n2", ClusterProxyError::Unreachable("timeout".to_string()));
        assert_eq!(err.status_code(), http::StatusCode::BAD_GATEWAY);
        assert!(err.message().contains("unreachable"));
    }
}
