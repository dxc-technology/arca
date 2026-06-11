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
//! Regardless of source, a candidate endpoint only supplies an address; the
//! authoritative signal is the probe. Since R3 (decision H12, review §3.7(A))
//! the probe is the **authenticated challenge-response ping**: a signed
//! `GET /cluster/v1/ping` carrying a fresh nonce, whose response must contain
//! `HMAC(secret, nonce)`. A peer that answers correctly has *proven possession
//! of the cluster secret* and becomes `authenticated` (eligible for
//! replication fan-out and quorum accounting). A peer that merely answers
//! HTTP — a rogue mDNS registrant, a node with a different secret (403), a
//! legacy pre-ping node (404 → public-health fallback, H10) — stays visible
//! as alive but is NOT eligible.
//!
//! Failure detection tolerates one missed probe (D12.3): a peer is declared
//! dead after [`DEAD_AFTER_FAILURES`] consecutive failures and alive again at
//! the first success. Peers unreachable beyond the pruning window (M3,
//! default = the tombstone grace) are evicted from membership so they cannot
//! block tombstone GC (§3.2) forever.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_core::cluster::{verify_ping_nonce_mac, ClusterState, PeerNode, WriteGate};
use chrono::{DateTime, Utc};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::cluster::client::{ClusterClient, ClusterError};
use crate::config::{ClusterConfig, DiscoveryMode};

/// mDNS service type for Arca cluster nodes.
const SERVICE_TYPE: &str = "_arca._tcp.local.";

/// D12.3 — failure-detector tolerance: a peer is declared dead only after this
/// many CONSECUTIVE probe failures (and alive again at the first success). One
/// missed probe is routinely a GC pause, a dropped packet, or a slow accept —
/// flapping a node out of the quorum for that causes far more churn (spurious
/// 503s, repair traffic) than the extra detection interval costs.
const DEAD_AFTER_FAILURES: u32 = 2;

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
    let prune_after = chrono::Duration::days(config.peer_prune_days() as i64);
    let secret = config.secret.clone();
    let scheme = scheme.to_string();
    let self_node_id = state.node_id().to_string();

    // Signed client for the authenticated ping probe (H12).
    let ping_client = match ClusterClient::new(&self_node_id, &secret, request_timeout) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "cluster membership: failed to build ping client; membership disabled");
            return;
        }
    };

    // Plain HTTP client for the public-health fallback (legacy peers, H10, and
    // identity recovery on a 403).
    // TECHDEBT(TD-015): under clustered TLS, peers use self-signed certs; until
    // R4 wires the shared cluster CA, accept invalid certs for this lightweight
    // probe (it only reads node_id and, from legacy peers, their health detail).
    let health_client = {
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

    // mDNS-discovered candidate endpoints, shared with the probe loop.
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
        // endpoint -> probe bookkeeping (identity, failure streak, last contact).
        let mut known: BTreeMap<String, ProbeState> = BTreeMap::new();
        // node_ids currently flagged (config drift / legacy / failed challenge),
        // so each condition logs once on transition rather than every tick.
        let mut mismatched: BTreeSet<String> = BTreeSet::new();
        let mut legacy: BTreeSet<String> = BTreeSet::new();
        let mut unproven: BTreeSet<String> = BTreeSet::new();
        // H6 size gate: log on transitions only.
        let mut size_exceeded_prev = false;
        let mut timer = tokio::time::interval(health_interval);

        loop {
            timer.tick().await;

            // 1) Resolve the current candidate endpoints from the discovery source.
            let candidates =
                resolve_candidates(&discovery, &seeds, &dns_name, &scheme, &mdns_candidates).await;
            for endpoint in candidates {
                known.entry(endpoint).or_default();
            }

            // 2) Probe every known endpoint in parallel (one slow/dead peer must
            // not serialize the tick — same rationale as the §2.4 fan-out).
            let probes = known.keys().cloned().map(|endpoint| {
                let ping_client = ping_client.clone();
                let health_client = health_client.clone();
                let secret = secret.clone();
                async move {
                    let outcome =
                        probe_peer(&ping_client, &health_client, &secret, &endpoint).await;
                    (endpoint, outcome)
                }
            });
            let outcomes = futures_util::future::join_all(probes).await;

            // 3) Integrate the outcomes into the per-endpoint probe state.
            let now = Utc::now();
            let my_fingerprint = state.config_fingerprint();
            for (endpoint, outcome) in outcomes {
                let Some(ps) = known.get_mut(&endpoint) else {
                    continue;
                };
                match outcome {
                    ProbeOutcome::SelfNode => {
                        // It's us — record our own advertised endpoint for the
                        // console; never track self as a peer.
                        state.set_local_endpoint(endpoint.clone());
                        ps.consecutive_failures = 0;
                        ps.last_contact = Some(now);
                        ps.node_id = None;
                        ps.last_live = None;
                    }
                    ProbeOutcome::Contact(info) => {
                        let config_ok = contact_config_ok(&info, &my_fingerprint);
                        log_contact_transitions(
                            &endpoint, &info, config_ok,
                            &mut mismatched, &mut legacy, &mut unproven,
                        );
                        ps.consecutive_failures = 0;
                        ps.last_contact = Some(now);
                        ps.node_id = Some(info.node_id.clone());
                        ps.last_live = Some(PeerNode {
                            node_id: info.node_id,
                            endpoint: endpoint.clone(),
                            alive: true,
                            last_seen: Some(now),
                            authenticated: info.auth == AuthState::Proven,
                            config_ok,
                            disk_total: info.disk_total,
                            disk_available: info.disk_available,
                        });
                    }
                    ProbeOutcome::Failure => {
                        ps.consecutive_failures = ps.consecutive_failures.saturating_add(1);
                    }
                }
            }

            // 4) M3 pruning: evict peers unreachable beyond the window (default
            // = the tombstone grace). A pruned peer stops blocking tombstone GC
            // (§3.2); if it ever returns it is re-discovered and re-probed like
            // a brand-new candidate (a beyond-grace re-entry is the documented
            // residual resurrection risk — see the HA guide).
            known.retain(|endpoint, ps| {
                let stale = ps.consecutive_failures > 0
                    && ps
                        .last_contact
                        .is_some_and(|t| now - t > prune_after);
                if stale {
                    let node_id = ps.node_id.clone().unwrap_or_default();
                    tracing::warn!(
                        peer = %endpoint,
                        peer_node_id = %node_id,
                        unseen_days = (now - ps.last_contact.unwrap_or(now)).num_days(),
                        "cluster membership: pruning peer unreachable beyond the prune window; \
                         it no longer blocks tombstone GC. If it ever returns it must re-sync \
                         (and a beyond-grace return can resurrect deleted data — see the HA guide)"
                    );
                    mismatched.remove(&node_id);
                    legacy.remove(&node_id);
                    unproven.remove(&node_id);
                }
                !stale
            });

            // 5) Build and publish the peer list.
            let peers: Vec<PeerNode> = known.values().filter_map(emit_peer).collect();
            state.set_peers(peers);

            // 6) H6 (D3a) size gate: shout on transitions (the gate itself is
            // enforced by ClusterState::write_gate in the store decorators).
            match state.write_gate() {
                WriteGate::SizeExceeded {
                    eligible,
                    cluster_size,
                } => {
                    if !size_exceeded_prev {
                        tracing::error!(
                            eligible,
                            cluster_size,
                            "cluster size exceeded: more eligible nodes than [cluster].cluster_size — \
                             REFUSING WRITES (fail-closed) to prevent split-brain. Remove the extra \
                             node(s) or resize the cluster with the documented cold procedure."
                        );
                    }
                    size_exceeded_prev = true;
                }
                _ => {
                    if size_exceeded_prev {
                        tracing::info!("cluster size back within cluster_size; write gate reopened");
                    }
                    size_exceeded_prev = false;
                }
            }
        }
    });
}

/// Per-endpoint probe bookkeeping kept across ticks.
#[derive(Default)]
struct ProbeState {
    /// Last known identity (kept while dead so the peer is still reported).
    node_id: Option<String>,
    /// Consecutive failed probes; reset on any successful contact (D12.3).
    consecutive_failures: u32,
    /// Wall-clock time of the last successful contact: the `last_seen`
    /// reported for dead peers, and what pruning (M3) and the tombstone-GC
    /// guard (§3.2) measure staleness against.
    last_contact: Option<DateTime<Utc>>,
    /// The peer view built from the last successful contact, re-emitted
    /// verbatim during the D12.3 tolerance window (one tick of stale disk
    /// stats is far cheaper than flapping the quorum).
    last_live: Option<PeerNode>,
}

/// How (whether) a contacted peer authenticated itself to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthState {
    /// Valid `HMAC(secret, nonce)` over our fresh nonce: possession proven.
    Proven,
    /// Ping answered but the challenge MAC was missing or wrong — treated as
    /// hostile-until-proven (a rogue can answer 200 to anything).
    BadMac,
    /// Our signed ping was rejected (403): the peer verifies with a DIFFERENT
    /// secret — config drift at the auth layer.
    AuthRejected,
    /// No ping route (404): a pre-R3 node — legacy fallback via public health
    /// (H10). Excluded from fan-out/quorum until upgraded.
    Legacy,
}

/// What a successful contact told us about the peer.
struct ContactInfo {
    node_id: String,
    auth: AuthState,
    fingerprint: Option<String>,
    disk_total: Option<u64>,
    disk_available: Option<u64>,
}

/// The result of probing one endpoint.
enum ProbeOutcome {
    /// The endpoint answered and identified itself.
    Contact(ContactInfo),
    /// The endpoint is this node itself (the loop-prevention 409).
    SelfNode,
    /// Network-level failure or an unusable answer — counts toward D12.3.
    Failure,
}

/// Probes one endpoint: authenticated ping first (with a fresh challenge
/// nonce), falling back to the public health probe to identify peers that
/// cannot answer it (legacy 404, secret-mismatch 403).
async fn probe_peer(
    ping_client: &ClusterClient,
    health_client: &reqwest::Client,
    secret: &str,
    endpoint: &str,
) -> ProbeOutcome {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    match ping_client.ping(endpoint, &nonce).await {
        Ok(resp) => {
            let proven = resp
                .nonce_mac
                .as_deref()
                .is_some_and(|mac| verify_ping_nonce_mac(secret, &nonce, mac));
            ProbeOutcome::Contact(ContactInfo {
                node_id: resp.node_id,
                auth: if proven {
                    AuthState::Proven
                } else {
                    AuthState::BadMac
                },
                fingerprint: resp.config_fingerprint,
                disk_total: resp.disk_total,
                disk_available: resp.disk_available,
            })
        }
        // 409 LoopDetected: the receiver saw OUR node id in the source header —
        // the endpoint is this node itself.
        Err(ClusterError::Http { status: 409, .. }) => ProbeOutcome::SelfNode,
        // 404: no ping route — a legacy (pre-ping) peer, or not clustered at
        // all; the public health fallback distinguishes (it also 404s when not
        // clustered). Legacy health still publishes fingerprint + disk stats.
        Err(ClusterError::Http { status: 404, .. }) => {
            match probe_health(health_client, endpoint).await {
                Some(h) => ProbeOutcome::Contact(ContactInfo {
                    auth: AuthState::Legacy,
                    node_id: h.node_id,
                    fingerprint: h.fingerprint,
                    disk_total: h.disk_total,
                    disk_available: h.disk_available,
                }),
                None => ProbeOutcome::Failure,
            }
        }
        // 403: the peer rejected our signature — it runs with a different
        // secret. Recover its identity from the public health so the operator
        // sees WHO is drifted instead of a silent dead slot.
        Err(ClusterError::Http { status: 403, .. }) => {
            match probe_health(health_client, endpoint).await {
                Some(h) => ProbeOutcome::Contact(ContactInfo {
                    auth: AuthState::AuthRejected,
                    node_id: h.node_id,
                    fingerprint: h.fingerprint,
                    disk_total: h.disk_total,
                    disk_available: h.disk_available,
                }),
                None => ProbeOutcome::Failure,
            }
        }
        Err(_) => ProbeOutcome::Failure,
    }
}

/// Drift verdict for a contacted peer. A 403 on the ping IS the drift (the
/// secret feeds both the SigV4 key and the fingerprint); otherwise compare
/// fingerprints, with "unknown on either side" NOT flagged (don't cry wolf).
fn contact_config_ok(info: &ContactInfo, my_fingerprint: &Option<String>) -> bool {
    if info.auth == AuthState::AuthRejected {
        return false;
    }
    match (my_fingerprint, &info.fingerprint) {
        (Some(mine), Some(theirs)) => mine == theirs,
        _ => true,
    }
}

/// Builds the published [`PeerNode`] for one probe state, if it should be
/// visible at all (identity known). Implements the D12.3 tolerance: within the
/// failure window the last live view is re-emitted; at [`DEAD_AFTER_FAILURES`]
/// the peer turns dead, keeping `last_seen` (the §3.2 GC guard and the admin
/// view reason about how long it has been unseen) and dropping its disk stats
/// (stale free space must not gate writes).
fn emit_peer(ps: &ProbeState) -> Option<PeerNode> {
    let node_id = ps.node_id.clone()?;
    if ps.consecutive_failures == 0 {
        return ps.last_live.clone();
    }
    if ps.consecutive_failures < DEAD_AFTER_FAILURES {
        return ps.last_live.clone(); // tolerated blip — keep the last live view
    }
    Some(PeerNode {
        node_id,
        endpoint: ps.last_live.as_ref().map(|p| p.endpoint.clone()).unwrap_or_default(),
        alive: false,
        last_seen: ps.last_contact,
        authenticated: false,
        config_ok: true, // no fresh evidence to judge a dead peer
        disk_total: None,
        disk_available: None,
    })
}

/// Logs once on each transition into/out of the "config drifted", "legacy" and
/// "challenge failed" conditions for a contacted peer.
fn log_contact_transitions(
    endpoint: &str,
    info: &ContactInfo,
    config_ok: bool,
    mismatched: &mut BTreeSet<String>,
    legacy: &mut BTreeSet<String>,
    unproven: &mut BTreeSet<String>,
) {
    let id = &info.node_id;

    // Config drift — one flag, two causes: the peer rejects our secret (403),
    // or it authenticated fine but its fingerprint differs (e.g. another
    // master key). Either way it is out of fan-out and quorum (H7).
    if !config_ok {
        if mismatched.insert(id.clone()) {
            if info.auth == AuthState::AuthRejected {
                tracing::warn!(
                    peer = %endpoint,
                    peer_node_id = %id,
                    "cluster config mismatch: this peer rejects our cluster secret \
                     (403 on the authenticated ping) — it is excluded from replication \
                     fan-out and the write quorum until the configs align."
                );
            } else {
                tracing::warn!(
                    peer = %endpoint,
                    peer_node_id = %id,
                    "cluster config mismatch: this peer's cluster-critical config \
                     (cluster_id / mode / cluster_size / master key) differs from ours — \
                     it is excluded from replication fan-out and the write quorum until \
                     the configs align."
                );
            }
        }
    } else if mismatched.remove(id) {
        tracing::info!(peer = %endpoint, peer_node_id = %id, "cluster config mismatch resolved");
    }

    // Legacy (no ping route — pre-upgrade version, H10).
    if info.auth == AuthState::Legacy {
        if legacy.insert(id.clone()) {
            tracing::warn!(
                peer = %endpoint,
                peer_node_id = %id,
                "legacy peer without the authenticated ping (pre-upgrade version): it \
                 counts as alive but is EXCLUDED from replication fan-out and the write \
                 quorum until it is upgraded (rolling-upgrade window). Its own fan-out \
                 and anti-entropy keep data converging meanwhile — upgrade it promptly."
            );
        }
    } else if legacy.remove(id) {
        tracing::info!(peer = %endpoint, peer_node_id = %id, "peer upgraded: authenticated ping available");
    }

    // Answered the ping but failed the challenge — hostile until proven.
    if info.auth == AuthState::BadMac {
        if unproven.insert(id.clone()) {
            tracing::warn!(
                peer = %endpoint,
                peer_node_id = %id,
                "peer answered the cluster ping WITHOUT proving possession of the \
                 cluster secret (missing/invalid challenge MAC) — treating it as \
                 untrusted: no replication fan-out, no quorum contribution."
            );
        }
    } else if unproven.remove(id) {
        tracing::info!(peer = %endpoint, peer_node_id = %id, "peer now proves possession of the cluster secret");
    }
}

/// What a peer reports on its (legacy) public `/cluster/v1/health`. R3
/// minimized that endpoint to `{status, node_id}` (§3.5), but pre-R3 peers
/// still publish their fingerprint and disk stats there — read them when
/// present so a legacy peer keeps its drift detection and capacity accounting
/// through the rolling-upgrade window.
struct PeerHealth {
    node_id: String,
    fingerprint: Option<String>,
    disk_total: Option<u64>,
    disk_available: Option<u64>,
}

/// Public health probe: on a 2xx response returns the peer's identity plus
/// whatever detail it advertises, else `None`.
async fn probe_health(client: &reqwest::Client, endpoint: &str) -> Option<PeerHealth> {
    let url = format!("{endpoint}/cluster/v1/health");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let node_id = body.get("node_id")?.as_str()?.to_string();
    Some(PeerHealth {
        node_id,
        fingerprint: body
            .get("config_fingerprint")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        disk_total: body.get("disk_total").and_then(|v| v.as_u64()),
        disk_available: body.get("disk_available").and_then(|v| v.as_u64()),
    })
}

/// Advertises this node over mDNS and browses for peers, inserting discovered
/// peer base URLs into `candidates`. The probe loop then authenticates them.
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
    use arca_core::cluster::ping_nonce_mac;

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

    // --- probe-state integration (D12.3 tolerance, dead view, §3.2 inputs) ---

    fn contacted(auth: AuthState) -> ContactInfo {
        ContactInfo {
            node_id: "n2".to_string(),
            auth,
            fingerprint: Some("ab12cd34".to_string()),
            disk_total: Some(100),
            disk_available: Some(40),
        }
    }

    /// Applies a Contact outcome the way the probe loop does (kept in sync by
    /// the emit tests below — this is the integration step distilled).
    fn integrate_contact(ps: &mut ProbeState, info: ContactInfo, now: DateTime<Utc>) {
        let config_ok = contact_config_ok(&info, &Some("ab12cd34".to_string()));
        ps.consecutive_failures = 0;
        ps.last_contact = Some(now);
        ps.node_id = Some(info.node_id.clone());
        ps.last_live = Some(PeerNode {
            node_id: info.node_id,
            endpoint: "http://n2:9000".to_string(),
            alive: true,
            last_seen: Some(now),
            authenticated: info.auth == AuthState::Proven,
            config_ok,
            disk_total: info.disk_total,
            disk_available: info.disk_available,
        });
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        chrono::TimeZone::timestamp_opt(&Utc, secs, 0).unwrap()
    }

    #[test]
    fn one_failure_is_tolerated_two_kill() {
        let mut ps = ProbeState::default();
        integrate_contact(&mut ps, contacted(AuthState::Proven), ts(100));
        let live = emit_peer(&ps).unwrap();
        assert!(live.alive && live.authenticated);

        // First miss (D12.3): still reported alive, last view re-emitted.
        ps.consecutive_failures += 1;
        let blip = emit_peer(&ps).unwrap();
        assert!(blip.alive, "one missed probe must not flap the peer");
        assert_eq!(blip.last_seen, Some(ts(100)));

        // Second consecutive miss: dead, with last_seen preserved (the §3.2
        // guard and the admin view need how long it has been unseen) and the
        // stale disk stats dropped.
        ps.consecutive_failures += 1;
        let dead = emit_peer(&ps).unwrap();
        assert!(!dead.alive);
        assert!(!dead.authenticated);
        assert_eq!(dead.last_seen, Some(ts(100)));
        assert_eq!(dead.disk_total, None);
        assert_eq!(dead.disk_available, None);
        assert!(dead.config_ok, "a dead peer is not judged for drift");

        // First success: alive again immediately.
        integrate_contact(&mut ps, contacted(AuthState::Proven), ts(200));
        assert!(emit_peer(&ps).unwrap().alive);
    }

    #[test]
    fn unknown_identity_emits_nothing() {
        let mut ps = ProbeState::default();
        assert!(emit_peer(&ps).is_none(), "never-contacted candidate is invisible");
        ps.consecutive_failures = 5;
        assert!(emit_peer(&ps).is_none());
    }

    #[test]
    fn legacy_and_badmac_contacts_are_alive_but_not_authenticated() {
        for auth in [AuthState::Legacy, AuthState::BadMac] {
            let mut ps = ProbeState::default();
            integrate_contact(&mut ps, contacted(auth), ts(100));
            let p = emit_peer(&ps).unwrap();
            assert!(p.alive, "{auth:?} answers HTTP — visible as alive");
            assert!(!p.authenticated, "{auth:?} must not be eligible");
            assert!(!p.eligible());
            assert!(p.config_ok, "fingerprints match — drift not implied by {auth:?}");
        }
    }

    #[test]
    fn auth_rejected_contact_is_drifted() {
        let mut ps = ProbeState::default();
        integrate_contact(&mut ps, contacted(AuthState::AuthRejected), ts(100));
        let p = emit_peer(&ps).unwrap();
        assert!(p.alive && !p.authenticated && !p.config_ok);
        assert!(!p.eligible());
    }

    #[test]
    fn fingerprint_mismatch_flags_drift() {
        let info = contacted(AuthState::Proven);
        assert!(contact_config_ok(&info, &Some("ab12cd34".to_string())));
        assert!(!contact_config_ok(&info, &Some("ffffffff".to_string())));
        // Unknown on either side: not flagged (don't cry wolf).
        assert!(contact_config_ok(&info, &None));
        let mut anon = contacted(AuthState::Proven);
        anon.fingerprint = None;
        assert!(contact_config_ok(&anon, &Some("ab12cd34".to_string())));
    }

    // --- end-to-end probe against a fake peer (pins the wire contract) -------

    /// A minimal HTTP/1.1 peer that implements the ping contract: it reads the
    /// nonce header and answers with a real HMAC over it (or a wrong one), so
    /// the full client→handler challenge round-trip is exercised.
    async fn spawn_fake_ping_peer(
        secret: &'static str,
        lie_in_mac: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let headers = String::from_utf8_lossy(&buf).to_lowercase();
                    let nonce = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("x-arca-cluster-nonce:"))
                        .map(|v| v.trim().to_string())
                        .unwrap_or_default();
                    let mac = if lie_in_mac {
                        "0".repeat(64)
                    } else {
                        ping_nonce_mac(secret, &nonce)
                    };
                    let body = format!(
                        r#"{{"status":"ok","node_id":"fake-peer","config_fingerprint":"ab12cd34","max_seq":3,"nonce_mac":"{mac}"}}"#
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn probe_authenticates_peer_with_correct_mac() {
        let (endpoint, _h) = spawn_fake_ping_peer("shared-secret", false).await;
        let ping = ClusterClient::new("self-node", "shared-secret", Duration::from_secs(2)).unwrap();
        let health = reqwest::Client::new();
        match probe_peer(&ping, &health, "shared-secret", &endpoint).await {
            ProbeOutcome::Contact(info) => {
                assert_eq!(info.node_id, "fake-peer");
                assert_eq!(info.auth, AuthState::Proven);
            }
            _ => panic!("expected an authenticated contact"),
        }
    }

    #[tokio::test]
    async fn probe_rejects_peer_with_wrong_mac() {
        // The fake answers 200 with a bogus MAC — exactly what a rogue that
        // ignores the challenge would do. It must NOT become authenticated.
        let (endpoint, _h) = spawn_fake_ping_peer("shared-secret", true).await;
        let ping = ClusterClient::new("self-node", "shared-secret", Duration::from_secs(2)).unwrap();
        let health = reqwest::Client::new();
        match probe_peer(&ping, &health, "shared-secret", &endpoint).await {
            ProbeOutcome::Contact(info) => {
                assert_eq!(info.auth, AuthState::BadMac);
            }
            _ => panic!("expected a contact"),
        }
    }

    #[tokio::test]
    async fn probe_unreachable_endpoint_is_failure() {
        let ping = ClusterClient::new("self-node", "s", Duration::from_millis(300)).unwrap();
        let health = reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        match probe_peer(&ping, &health, "s", "http://127.0.0.1:1").await {
            ProbeOutcome::Failure => {}
            _ => panic!("expected a failure"),
        }
    }
}
