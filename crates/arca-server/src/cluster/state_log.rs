//! Cluster state logger: one line per transition of the states that make a
//! node answer 5xx on purpose, escalated while they last.
//!
//! The access log keeps each deliberate refusal at DEBUG (see
//! `arca_proto::middleware::failure_log`); this logger is where an operator sees
//! what the cluster is doing. Deterministic rule table:
//!
//! | State                         | Start         | Lasting                         | End                    |
//! |-------------------------------|---------------|---------------------------------|------------------------|
//! | No write quorum (after having it) | WARN      | ERROR after 1m, every 5m        | INFO, writes refused   |
//! | No write quorum yet (startup) | INFO          | ERROR after 1m, every 5m        | INFO, writes refused   |
//! | Syncing (first pass)          | INFO          | WARN after 5m, every 5m         | INFO                   |
//! | Known peer unreachable        | WARN          | ERROR after 5m, every 5m        | INFO                   |
//! | Seed not reached since start  | INFO          | ERROR after 5m, every 5m        | INFO when reached      |
//! | Writes refused for space (507)| ERROR         | ERROR every 5m while refusing   | (no end: rate-limited) |
//!
//! Lasting states repeat their line every [`REPEAT_EVERY`] so an outage is
//! never a single line lost in the scrollback, and the quorum and space lines
//! carry how many writes were refused, so client impact shows at INFO without
//! a line per request. Syncing escalates to WARN only: a long first pass can be
//! legitimate (a large re-pull), and the line carries the elapsed time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arca_core::cluster::{ClusterState, WriteGate};
use chrono::{DateTime, Utc};
use tracing::Level;

/// A write quorum missing this long is an error.
const QUORUM_ERROR_AFTER: Duration = Duration::from_secs(60);
/// A first anti-entropy pass running this long is worth a warning.
const SYNCING_WARN_AFTER: Duration = Duration::from_secs(5 * 60);
/// A peer unreachable (or a seed never reached) this long is an error.
const PEER_ERROR_AFTER: Duration = Duration::from_secs(5 * 60);
/// How often a lasting degraded state repeats its line.
const REPEAT_EVERY: Duration = Duration::from_secs(5 * 60);

/// What the logger needs from the cluster state at one tick.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// `Some((eligible, quorum))` while the write gate is closed for lack of
    /// quorum; `None` when it is open, in `available` mode, or closed for an
    /// over-sized membership (logged by the membership manager).
    pub no_quorum: Option<(usize, u32)>,
    pub syncing: bool,
    /// Known peers: `(node_id, endpoint, alive)`.
    pub peers: Vec<(String, String, bool)>,
    /// Seeds not reached since startup.
    pub unreached: Vec<String>,
    /// Totals since startup (monotonic).
    pub writes_refused_no_quorum: u64,
    pub writes_refused_no_space: u64,
    /// The last 507's `(needed, min_available)` bytes.
    pub last_space_refusal: Option<(u64, u64)>,
}

impl Observation {
    fn from_state(state: &ClusterState) -> Self {
        let no_quorum = match state.write_gate() {
            WriteGate::NoQuorum { eligible, quorum } => Some((eligible, quorum)),
            WriteGate::Open | WriteGate::SizeExceeded { .. } => None,
        };
        let (writes_refused_no_space, last_space_refusal) = state.writes_refused_no_space();
        Self {
            no_quorum,
            syncing: state.is_syncing(),
            peers: state
                .peers()
                .into_iter()
                .map(|p| (p.node_id, p.endpoint, p.alive))
                .collect(),
            unreached: state.unreached_endpoints(),
            writes_refused_no_quorum: state.writes_refused_no_quorum(),
            writes_refused_no_space,
            last_space_refusal,
        }
    }
}

/// One line to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    pub message: String,
}

impl LogLine {
    fn new(level: Level, message: String) -> Self {
        Self { level, message }
    }
}

/// A degraded state in progress.
#[derive(Debug, Clone)]
struct Episode {
    since: DateTime<Utc>,
    last_repeat: Option<DateTime<Utc>>,
    /// The relevant refusal counter when the episode started.
    refused_at_start: u64,
}

impl Episode {
    fn start(now: DateTime<Utc>, refused: u64) -> Self {
        Self { since: now, last_repeat: None, refused_at_start: refused }
    }

    fn elapsed(&self, now: DateTime<Utc>) -> Duration {
        (now - self.since).to_std().unwrap_or_default()
    }

    /// True when the episode lasted `after` and its line is due again (first
    /// time, or [`REPEAT_EVERY`] since the last repetition); records it.
    fn repeat_due(&mut self, now: DateTime<Utc>, after: Duration) -> bool {
        if self.elapsed(now) < after {
            return false;
        }
        let due = self
            .last_repeat
            .is_none_or(|t| (now - t).to_std().unwrap_or_default() >= REPEAT_EVERY);
        if due {
            self.last_repeat = Some(now);
        }
        due
    }
}

#[derive(Debug, Clone)]
struct PeerTrack {
    endpoint: String,
    down: Option<Episode>,
}

/// The logger's memory between ticks.
#[derive(Debug, Default)]
pub struct StateLog {
    ever_had_quorum: bool,
    no_quorum: Option<Episode>,
    syncing: Option<Episode>,
    peers: HashMap<String, PeerTrack>,
    unreached: HashMap<String, Episode>,
    space_seen: u64,
    space_pending: u64,
    space_logged_at: Option<DateTime<Utc>>,
}

impl StateLog {
    /// Compares `obs` with the previous tick and returns the lines to log.
    pub fn observe(&mut self, obs: &Observation, now: DateTime<Utc>) -> Vec<LogLine> {
        let mut out = Vec::new();
        self.observe_peers(obs, now, &mut out);
        self.observe_quorum(obs, now, &mut out);
        self.observe_syncing(obs, now, &mut out);
        self.observe_space(obs, now, &mut out);
        out
    }

    fn observe_quorum(&mut self, obs: &Observation, now: DateTime<Utc>, out: &mut Vec<LogLine>) {
        match (obs.no_quorum, self.no_quorum.as_mut()) {
            (Some((eligible, quorum)), None) => {
                let line = if self.ever_had_quorum {
                    LogLine::new(
                        Level::WARN,
                        format!(
                            "cluster write quorum lost: {eligible} of {quorum} required node(s) \
                             eligible; writes are refused with 503 until it returns"
                        ),
                    )
                } else {
                    LogLine::new(
                        Level::INFO,
                        format!(
                            "waiting for the cluster write quorum: {eligible} of {quorum} \
                             required node(s) eligible"
                        ),
                    )
                };
                out.push(line);
                self.no_quorum = Some(Episode::start(now, obs.writes_refused_no_quorum));
            }
            (Some((eligible, quorum)), Some(ep)) => {
                if ep.repeat_due(now, QUORUM_ERROR_AFTER) {
                    let refused = obs.writes_refused_no_quorum - ep.refused_at_start;
                    out.push(LogLine::new(
                        Level::ERROR,
                        format!(
                            "no cluster write quorum for {}: {eligible} of {quorum} required \
                             node(s) eligible{}; {refused} write(s) refused so far",
                            human(ep.elapsed(now)),
                            missing_peers(obs),
                        ),
                    ));
                }
            }
            (None, Some(ep)) => {
                let refused = obs.writes_refused_no_quorum - ep.refused_at_start;
                let what = if self.ever_had_quorum { "back" } else { "reached" };
                out.push(LogLine::new(
                    Level::INFO,
                    format!(
                        "cluster write quorum {what} after {}; {refused} write(s) refused meanwhile",
                        human(ep.elapsed(now)),
                    ),
                ));
                self.no_quorum = None;
                self.ever_had_quorum = true;
            }
            (None, None) => self.ever_had_quorum = true,
        }
    }

    fn observe_syncing(&mut self, obs: &Observation, now: DateTime<Utc>, out: &mut Vec<LogLine>) {
        match (obs.syncing, self.syncing.as_mut()) {
            (true, None) => {
                out.push(LogLine::new(
                    Level::INFO,
                    "synchronizing with peers (first anti-entropy pass): /admin/health answers \
                     503 \"syncing\", so load balancers keep this node out of rotation"
                        .to_string(),
                ));
                self.syncing = Some(Episode::start(now, 0));
            }
            (true, Some(ep)) => {
                if ep.repeat_due(now, SYNCING_WARN_AFTER) {
                    out.push(LogLine::new(
                        Level::WARN,
                        format!(
                            "still synchronizing with peers after {} (first anti-entropy pass; \
                             per-peer lag on /admin/cluster)",
                            human(ep.elapsed(now)),
                        ),
                    ));
                }
            }
            (false, Some(ep)) => {
                out.push(LogLine::new(
                    Level::INFO,
                    format!("synchronized with peers after {}", human(ep.elapsed(now))),
                ));
                self.syncing = None;
            }
            (false, None) => {}
        }
    }

    fn observe_peers(&mut self, obs: &Observation, now: DateTime<Utc>, out: &mut Vec<LogLine>) {
        for (node_id, endpoint, alive) in &obs.peers {
            match self.peers.get_mut(node_id) {
                None => {
                    // First sight of this identity: a seed reached, or a peer
                    // discovered later.
                    let waited = self.unreached.remove(endpoint);
                    let down = if *alive {
                        let line = match waited {
                            Some(ep) => format!(
                                "peer {node_id} reached at {endpoint} after {}",
                                human(ep.elapsed(now))
                            ),
                            None => format!("peer {node_id} at {endpoint} is reachable"),
                        };
                        out.push(LogLine::new(Level::INFO, line));
                        None
                    } else {
                        out.push(LogLine::new(
                            Level::WARN,
                            format!("peer {node_id} at {endpoint} is unreachable"),
                        ));
                        Some(Episode::start(now, 0))
                    };
                    self.peers.insert(
                        node_id.clone(),
                        PeerTrack { endpoint: endpoint.clone(), down },
                    );
                }
                Some(track) => {
                    track.endpoint.clone_from(endpoint);
                    match (*alive, track.down.as_mut()) {
                        (false, None) => {
                            out.push(LogLine::new(
                                Level::WARN,
                                format!("peer {node_id} at {endpoint} is unreachable"),
                            ));
                            track.down = Some(Episode::start(now, 0));
                        }
                        (false, Some(ep)) => {
                            if ep.repeat_due(now, PEER_ERROR_AFTER) {
                                out.push(LogLine::new(
                                    Level::ERROR,
                                    format!(
                                        "peer {node_id} at {endpoint} unreachable for {}",
                                        human(ep.elapsed(now))
                                    ),
                                ));
                            }
                        }
                        (true, Some(ep)) => {
                            out.push(LogLine::new(
                                Level::INFO,
                                format!(
                                    "peer {node_id} at {endpoint} is back after {}",
                                    human(ep.elapsed(now))
                                ),
                            ));
                            track.down = None;
                        }
                        (true, None) => {}
                    }
                }
            }
        }
        // Pruned by membership (it logs that itself): forget them.
        self.peers
            .retain(|id, _| obs.peers.iter().any(|(node_id, _, _)| node_id == id));

        for endpoint in &obs.unreached {
            match self.unreached.get_mut(endpoint) {
                None => {
                    out.push(LogLine::new(
                        Level::INFO,
                        format!("waiting for the peer at {endpoint} (not reached since startup)"),
                    ));
                    self.unreached.insert(endpoint.clone(), Episode::start(now, 0));
                }
                Some(ep) => {
                    if ep.repeat_due(now, PEER_ERROR_AFTER) {
                        out.push(LogLine::new(
                            Level::ERROR,
                            format!(
                                "peer at {endpoint} not reached since startup ({})",
                                human(ep.elapsed(now))
                            ),
                        ));
                    }
                }
            }
        }
        self.unreached.retain(|endpoint, _| obs.unreached.contains(endpoint));
    }

    fn observe_space(&mut self, obs: &Observation, now: DateTime<Utc>, out: &mut Vec<LogLine>) {
        self.space_pending += obs.writes_refused_no_space.saturating_sub(self.space_seen);
        self.space_seen = obs.writes_refused_no_space;
        if self.space_pending == 0 {
            return;
        }
        let due = self
            .space_logged_at
            .is_none_or(|t| (now - t).to_std().unwrap_or_default() >= REPEAT_EVERY);
        if !due {
            return;
        }
        let last = obs
            .last_space_refusal
            .map(|(needed, available)| {
                format!("; last: {needed} byte(s) needed, {available} free on the fullest node")
            })
            .unwrap_or_default();
        out.push(LogLine::new(
            Level::ERROR,
            format!(
                "{} write(s) refused with 507: a node in the cluster lacks the disk space{last}",
                self.space_pending
            ),
        ));
        self.space_pending = 0;
        self.space_logged_at = Some(now);
    }
}

/// `"; missing: a, b"` for the dead peers and unreached seeds, or nothing.
fn missing_peers(obs: &Observation) -> String {
    let mut missing: Vec<&str> = obs
        .peers
        .iter()
        .filter(|(_, _, alive)| !alive)
        .map(|(_, endpoint, _)| endpoint.as_str())
        .chain(obs.unreached.iter().map(String::as_str))
        .collect();
    missing.sort_unstable();
    if missing.is_empty() {
        String::new()
    } else {
        format!("; missing: {}", missing.join(", "))
    }
}

/// `"42s"`, `"5m 3s"`, `"2h 5m"`.
fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

/// Runs the logger every `interval` (the membership health interval).
pub fn spawn(state: Arc<ClusterState>, interval: Duration) {
    tokio::spawn(async move {
        let mut log = StateLog::default();
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            for line in log.observe(&Observation::from_state(&state), Utc::now()) {
                match line.level {
                    Level::ERROR => tracing::error!("{}", line.message),
                    Level::WARN => tracing::warn!("{}", line.message),
                    _ => tracing::info!("{}", line.message),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        chrono::TimeZone::timestamp_opt(&Utc, 1_800_000_000 + secs, 0).unwrap()
    }

    fn levels(lines: &[LogLine]) -> Vec<Level> {
        lines.iter().map(|l| l.level).collect()
    }

    fn peer(id: &str, endpoint: &str, alive: bool) -> (String, String, bool) {
        (id.into(), endpoint.into(), alive)
    }

    /// A normal cold start: quorum missing, then reached within seconds, a
    /// short first pass. Nothing above INFO.
    #[test]
    fn normal_startup_logs_info_only() {
        let mut log = StateLog::default();
        let mut obs = Observation {
            no_quorum: Some((1, 2)),
            unreached: vec!["http://b:9000".into(), "http://c:9000".into()],
            ..Default::default()
        };
        let first = log.observe(&obs, t(0));
        assert_eq!(levels(&first), vec![Level::INFO; 3], "{first:?}");
        assert!(first.iter().any(|l| l.message.starts_with("waiting for the cluster write quorum")));

        obs.no_quorum = None;
        obs.syncing = true;
        obs.unreached = vec!["http://c:9000".into()];
        obs.peers = vec![peer("b", "http://b:9000", true)];
        let second = log.observe(&obs, t(5));
        assert_eq!(levels(&second), vec![Level::INFO; 3], "{second:?}");
        assert!(second.iter().any(|l| l.message == "peer b reached at http://b:9000 after 5s"));
        assert!(second
            .iter()
            .any(|l| l.message == "cluster write quorum reached after 5s; 0 write(s) refused meanwhile"));

        obs.syncing = false;
        let third = log.observe(&obs, t(9));
        assert_eq!(third, vec![LogLine::new(Level::INFO, "synchronized with peers after 4s".into())]);
    }

    /// Quorum lost after having it: WARN, ERROR after a minute repeated every
    /// five, INFO with the refused-writes count when it returns.
    #[test]
    fn quorum_loss_escalates_and_reports_refusals() {
        let mut log = StateLog::default();
        let mut obs = Observation {
            peers: vec![peer("b", "http://b:9000", true), peer("c", "http://c:9000", true)],
            ..Default::default()
        };
        log.observe(&obs, t(0));

        obs.no_quorum = Some((1, 2));
        obs.peers = vec![peer("b", "http://b:9000", false), peer("c", "http://c:9000", false)];
        obs.writes_refused_no_quorum = 10;
        let lost = log.observe(&obs, t(10));
        assert!(lost.contains(&LogLine::new(
            Level::WARN,
            "cluster write quorum lost: 1 of 2 required node(s) eligible; writes are refused with \
             503 until it returns"
                .into()
        )));

        obs.writes_refused_no_quorum = 25;
        assert!(log.observe(&obs, t(40)).iter().all(|l| l.level != Level::ERROR));
        let error = log.observe(&obs, t(70));
        assert!(error.contains(&LogLine::new(
            Level::ERROR,
            "no cluster write quorum for 1m 0s: 1 of 2 required node(s) eligible; missing: \
             http://b:9000, http://c:9000; 15 write(s) refused so far"
                .into()
        )));
        assert!(log.observe(&obs, t(200)).iter().all(|l| l.level != Level::ERROR), "not before 5m");
        assert!(log.observe(&obs, t(370)).iter().any(|l| l.level == Level::ERROR), "repeated");

        obs.no_quorum = None;
        obs.writes_refused_no_quorum = 40;
        obs.peers = vec![peer("b", "http://b:9000", true), peer("c", "http://c:9000", false)];
        let back = log.observe(&obs, t(400));
        assert!(back.contains(&LogLine::new(
            Level::INFO,
            "cluster write quorum back after 6m 30s; 30 write(s) refused meanwhile".into()
        )));
        assert!(back.contains(&LogLine::new(
            Level::INFO,
            "peer b at http://b:9000 is back after 6m 30s".into()
        )));
    }

    /// A peer down: WARN at once, ERROR only after five minutes, then every five.
    #[test]
    fn peer_down_escalates_after_five_minutes() {
        let mut log = StateLog::default();
        let mut obs = Observation { peers: vec![peer("b", "http://b:9000", true)], ..Default::default() };
        log.observe(&obs, t(0));
        obs.peers = vec![peer("b", "http://b:9000", false)];
        assert_eq!(
            log.observe(&obs, t(1)),
            vec![LogLine::new(Level::WARN, "peer b at http://b:9000 is unreachable".into())]
        );
        assert!(log.observe(&obs, t(299)).is_empty());
        assert_eq!(
            log.observe(&obs, t(301)),
            vec![LogLine::new(Level::ERROR, "peer b at http://b:9000 unreachable for 5m 0s".into())]
        );
        assert!(log.observe(&obs, t(500)).is_empty());
        assert_eq!(levels(&log.observe(&obs, t(601))), vec![Level::ERROR]);
    }

    /// A seed never reached since startup: INFO, then ERROR after five minutes.
    #[test]
    fn unreached_seed_becomes_an_error_if_it_never_answers() {
        let mut log = StateLog::default();
        let obs = Observation { unreached: vec!["http://c:9000".into()], ..Default::default() };
        assert_eq!(levels(&log.observe(&obs, t(0))), vec![Level::INFO]);
        assert!(log.observe(&obs, t(120)).is_empty());
        assert_eq!(
            log.observe(&obs, t(300)),
            vec![LogLine::new(
                Level::ERROR,
                "peer at http://c:9000 not reached since startup (5m 0s)".into()
            )]
        );
    }

    /// A long first pass warns (never errors) after five minutes.
    #[test]
    fn long_sync_warns_without_erroring() {
        let mut log = StateLog::default();
        let obs = Observation { syncing: true, ..Default::default() };
        assert_eq!(levels(&log.observe(&obs, t(0))), vec![Level::INFO]);
        assert!(log.observe(&obs, t(299)).is_empty());
        assert_eq!(levels(&log.observe(&obs, t(300))), vec![Level::WARN]);
        assert_eq!(levels(&log.observe(&obs, t(600))), vec![Level::WARN]);
    }

    /// 507 refusals: ERROR at once, then at most every five minutes with the
    /// count accumulated meanwhile, also when the refusals have stopped.
    #[test]
    fn space_refusals_are_rate_limited_errors_with_counts() {
        let mut log = StateLog::default();
        let mut obs = Observation {
            writes_refused_no_space: 1,
            last_space_refusal: Some((100, 10)),
            ..Default::default()
        };
        assert_eq!(
            log.observe(&obs, t(0)),
            vec![LogLine::new(
                Level::ERROR,
                "1 write(s) refused with 507: a node in the cluster lacks the disk space; last: \
                 100 byte(s) needed, 10 free on the fullest node"
                    .into()
            )]
        );
        obs.writes_refused_no_space = 4;
        assert!(log.observe(&obs, t(60)).is_empty());
        let again = log.observe(&obs, t(300));
        assert_eq!(levels(&again), vec![Level::ERROR]);
        assert!(again[0].message.starts_with("3 write(s) refused with 507"));
        assert!(log.observe(&obs, t(900)).is_empty(), "nothing new: nothing logged");
    }

    #[test]
    fn durations_are_human() {
        assert_eq!(human(Duration::from_secs(42)), "42s");
        assert_eq!(human(Duration::from_secs(303)), "5m 3s");
        assert_eq!(human(Duration::from_secs(7500)), "2h 5m");
    }
}
