//! Cluster membership manager.
//!
//! Discovers peers and tracks their liveness, refreshing the shared
//! [`ClusterState`] consumed by the cluster endpoints, the admin API, the
//! console dashboard, and the store decorators' quorum gate.
//!
//! Discovery sources (selected by `[cluster].discovery`):
//! - `mdns`: zero-config LAN discovery via `_arca._tcp.local` (no daemon);
//! - `static`: a fixed seed list, identical on every node;
//! - `dns`: a name resolving to all peers (e.g. a Kubernetes headless Service).
//!
//! Regardless of source, liveness and identity come from each peer's public
//! `GET /cluster/v1/health`, which returns the peer's `node_id`. A node never
//! lists its own id as a peer. mDNS only supplies candidate endpoints; the
//! authoritative alive/identity signal is always the health probe.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_core::cluster::{ClusterState, PeerNode};
use chrono::Utc;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::config::{ClusterConfig, DiscoveryMode};

/// mDNS service type for Arca cluster nodes.
const SERVICE_TYPE: &str = "_arca._tcp.local.";

/// Spawns the membership manager as detached background task(s) that run for
/// the lifetime of the process, periodically refreshing `state`.
pub fn spawn(config: &ClusterConfig, state: Arc<ClusterState>, scheme: &str, advertise_port: u16) {
    let discovery = config.discovery;
    let cluster_id = config.cluster_id.clone();
    let seeds = config.seeds.clone();
    let dns_name = config.dns_name.clone();
    let advertise_addr = config.advertise_addr.clone();
    let health_interval = Duration::from_secs(config.health_interval_seconds.max(1));
    let request_timeout = Duration::from_secs(config.request_timeout_seconds.max(1));
    let scheme = scheme.to_string();
    let self_node_id = state.node_id().to_string();

    // HTTP client for health probes.
    // TECHDEBT(TD-015): under clustered TLS, peers use self-signed certs; until
    // the ClusterClient (M2) wires the shared cluster CA, accept invalid certs
    // for this lightweight, non-sensitive health probe (it only reads node_id).
    let client = {
        let mut builder = reqwest::Client::builder().timeout(request_timeout);
        if scheme == "https" {
            builder = builder.danger_accept_invalid_certs(true);
        }
        match builder.build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "cluster membership: failed to build HTTP client; membership disabled");
                return;
            }
        }
    };

    // mDNS-discovered candidate endpoints, shared with the health loop.
    let mdns_candidates: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));

    if discovery == DiscoveryMode::Mdns {
        spawn_mdns_discovery(
            cluster_id,
            self_node_id.clone(),
            advertise_addr,
            advertise_port,
            scheme.clone(),
            mdns_candidates.clone(),
        );
    }

    tracing::info!(
        node_id = %self_node_id,
        discovery = ?discovery,
        "cluster membership manager started"
    );

    tokio::spawn(async move {
        // endpoint -> last known node_id (a peer that goes down keeps its id so
        // it can still be reported as a known-but-dead member).
        let mut known: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut timer = tokio::time::interval(health_interval);

        loop {
            timer.tick().await;

            // 1) Resolve the current candidate endpoints from the discovery source.
            let candidates =
                resolve_candidates(&discovery, &seeds, &dns_name, &scheme, &mdns_candidates).await;
            for endpoint in candidates {
                known.entry(endpoint).or_insert(None);
            }

            // 2) Health-probe each candidate and rebuild the peer list.
            let mut peers = Vec::new();
            for (endpoint, last_id) in known.iter_mut() {
                let url = format!("{endpoint}/cluster/v1/health");
                match probe(&client, &url).await {
                    Some(node_id) if node_id == self_node_id => {
                        // It's us — never list self as a peer, but record our
                        // own advertised endpoint so the console can show it.
                        state.set_local_endpoint(endpoint.clone());
                    }
                    Some(node_id) => {
                        *last_id = Some(node_id.clone());
                        peers.push(PeerNode {
                            node_id,
                            endpoint: endpoint.clone(),
                            alive: true,
                            last_seen: Some(Utc::now()),
                        });
                    }
                    None => {
                        // Unreachable: report it as down, but only once we have
                        // ever learned its identity (avoids noise from seeds
                        // that never came up).
                        if let Some(id) = last_id.clone() {
                            peers.push(PeerNode {
                                node_id: id,
                                endpoint: endpoint.clone(),
                                alive: false,
                                last_seen: None,
                            });
                        }
                    }
                }
            }

            state.set_peers(peers);
        }
    });
}

/// Advertises this node over mDNS and browses for peers, inserting discovered
/// peer base URLs into `candidates`. The health loop then probes them.
fn spawn_mdns_discovery(
    cluster_id: String,
    self_node_id: String,
    advertise_addr: Option<String>,
    advertise_port: u16,
    scheme: String,
    candidates: Arc<Mutex<BTreeSet<String>>>,
) {
    tokio::spawn(async move {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "cluster mDNS: failed to start daemon; mDNS discovery disabled");
                return;
            }
        };

        // Advertise this node. TXT records carry the cluster id and node id so
        // peers can filter by cluster and skip themselves.
        let props = [
            ("cluster_id", cluster_id.as_str()),
            ("node_id", self_node_id.as_str()),
        ];
        let host_name = format!("{self_node_id}.arca.local.");
        let info = match &advertise_addr {
            Some(addr) => {
                ServiceInfo::new(SERVICE_TYPE, &self_node_id, &host_name, addr.as_str(), advertise_port, &props[..])
            }
            None => ServiceInfo::new(SERVICE_TYPE, &self_node_id, &host_name, "0.0.0.0", advertise_port, &props[..])
                .map(|i| i.enable_addr_auto()),
        };
        match info {
            Ok(info) => {
                if let Err(e) = daemon.register(info) {
                    tracing::warn!(error = %e, "cluster mDNS: failed to register service");
                }
            }
            Err(e) => tracing::warn!(error = %e, "cluster mDNS: invalid service info"),
        }

        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "cluster mDNS: failed to browse; discovery disabled");
                return;
            }
        };

        while let Ok(event) = receiver.recv_async().await {
            if let ServiceEvent::ServiceResolved(info) = event {
                // Only same-cluster peers, and never ourselves.
                if info.get_property_val_str("cluster_id") != Some(cluster_id.as_str()) {
                    continue;
                }
                if info.get_property_val_str("node_id") == Some(self_node_id.as_str()) {
                    continue;
                }
                let port = info.get_port();
                if let Ok(mut set) = candidates.lock() {
                    for ip in info.get_addresses_v4() {
                        set.insert(format!("{scheme}://{ip}:{port}"));
                    }
                }
            }
        }
    });
}

/// Health probe: returns the peer's `node_id` on a 2xx response, else `None`.
async fn probe(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("node_id")?.as_str().map(|s| s.to_string())
}

/// Resolves candidate peer endpoints (base URLs) from the discovery source.
async fn resolve_candidates(
    discovery: &DiscoveryMode,
    seeds: &[String],
    dns_name: &Option<String>,
    scheme: &str,
    mdns_candidates: &Arc<Mutex<BTreeSet<String>>>,
) -> Vec<String> {
    match discovery {
        DiscoveryMode::Static => seeds.iter().map(|s| to_base_url(s, scheme)).collect(),
        DiscoveryMode::Dns => match dns_name {
            Some(name) => resolve_dns(name, scheme).await,
            None => Vec::new(),
        },
        DiscoveryMode::Mdns => mdns_candidates
            .lock()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default(),
    }
}

/// Normalizes a seed (`host:port` or a full URL) into a base URL with scheme.
fn to_base_url(seed: &str, scheme: &str) -> String {
    if seed.starts_with("http://") || seed.starts_with("https://") {
        seed.trim_end_matches('/').to_string()
    } else {
        format!("{scheme}://{}", seed.trim_end_matches('/'))
    }
}

/// Resolves a DNS name (`host[:port]`) to one base URL per resolved address.
async fn resolve_dns(name: &str, scheme: &str) -> Vec<String> {
    let with_port = if name.contains(':') {
        name.to_string()
    } else {
        format!("{name}:9000")
    };
    match tokio::net::lookup_host(with_port).await {
        Ok(addrs) => addrs
            .map(|a| format!("{scheme}://{}:{}", a.ip(), a.port()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        Err(e) => {
            tracing::debug!(name = %name, error = %e, "cluster membership: DNS resolution failed");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_from_host_port() {
        assert_eq!(to_base_url("arca-2:9000", "http"), "http://arca-2:9000");
        assert_eq!(to_base_url("arca-2:9000", "https"), "https://arca-2:9000");
    }

    #[test]
    fn base_url_passes_through_full_url() {
        assert_eq!(
            to_base_url("https://node-b.internal:9000/", "http"),
            "https://node-b.internal:9000"
        );
    }
}
