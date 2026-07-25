//! HTTP middleware for the S3 protocol layer.

use chrono::{DateTime, Utc};

/// SigV4 header-auth anti-replay window, in seconds. A signed request is
/// accepted only when its `x-amz-date` lies within this many seconds of the
/// server clock (either direction). Without it, a captured signed request
/// (e.g. one sniffed off a plain-HTTP hop or read from an access log) stays
/// replayable forever. ±15 minutes matches the AWS SigV4 convention and is
/// generous against NTP drift (the HA guide already mandates NTP on cluster
/// nodes).
///
/// Shared by all three header-auth paths: S3 ([`auth`]), admin
/// ([`admin_auth`]) and inter-node ([`cluster_auth`], §3.1).
pub const REPLAY_WINDOW_SECS: i64 = 15 * 60;

/// Returns true when `amz_date` (SigV4 `YYYYMMDDTHHMMSSZ`) falls within
/// [`REPLAY_WINDOW_SECS`] of `now`. Unparseable timestamps are rejected: the
/// signature already covers `x-amz-date`, so a legitimate client that produced
/// a valid signature always sends the canonical basic-ISO8601 format.
pub fn within_replay_window(amz_date: &str, now: DateTime<Utc>) -> bool {
    match chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ") {
        Ok(t) => (now - t.and_utc()).num_seconds().abs() <= REPLAY_WINDOW_SECS,
        Err(_) => false,
    }
}

pub mod admin_auth;
pub mod audit;
pub mod auth;
pub mod cluster_auth;
pub mod identity;
pub mod maintenance_drain;
pub mod normalize;
pub mod rate_limit;
pub mod request_id;
pub mod validate;
pub mod virtual_host;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn replay_window_accepts_fresh_and_boundary_timestamps() {
        let now = at("2026-06-11T12:00:00Z");
        assert!(within_replay_window("20260611T120000Z", now));
        // Exactly ±15 minutes is still inside (<=).
        assert!(within_replay_window("20260611T114500Z", now));
        assert!(within_replay_window("20260611T121500Z", now));
    }

    #[test]
    fn replay_window_rejects_stale_and_future_timestamps() {
        let now = at("2026-06-11T12:00:00Z");
        // One second beyond the window, both directions.
        assert!(!within_replay_window("20260611T114459Z", now));
        assert!(!within_replay_window("20260611T121501Z", now));
        // A captured request replayed the next day.
        assert!(!within_replay_window("20260610T120000Z", now));
    }

    #[test]
    fn replay_window_rejects_malformed_timestamps() {
        let now = at("2026-06-11T12:00:00Z");
        assert!(!within_replay_window("", now));
        assert!(!within_replay_window("not-a-date", now));
        // Missing the trailing Z / wrong shape.
        assert!(!within_replay_window("20260611T120000", now));
        assert!(!within_replay_window("2026-06-11T12:00:00Z", now));
    }

    /// The window is evaluated against the CALLER's `now`, not the wall clock:
    /// the three middlewares pass `Utc::now()`, and a timestamp that is fresh
    /// for one instant must be stale for a later one. This is what makes the
    /// helper testable without freezing time.
    #[test]
    fn replay_window_is_relative_to_the_supplied_now() {
        let amz_date = "20260611T120000Z";
        assert!(within_replay_window(amz_date, at("2026-06-11T12:14:59Z")));
        assert!(!within_replay_window(amz_date, at("2026-06-11T12:15:01Z")));
    }

    /// The window must not be widened by accident: the anti-replay guarantee of
    /// all three header-auth paths is exactly this constant.
    #[test]
    fn replay_window_is_fifteen_minutes() {
        assert_eq!(REPLAY_WINDOW_SECS, 900);
    }
}
