//! In-memory metrics collection for Prometheus exposition.
//!
//! Tracks request counters (by operation + status class) and latency histograms.
//! All structures use atomics for lock-free concurrent updates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Thread-safe metrics registry.
#[derive(Debug)]
pub struct MetricsRegistry {
    /// Request counters keyed by (operation, status_class).
    counters: RwLock<HashMap<(String, String), AtomicU64>>,
    /// Latency histogram buckets keyed by operation.
    histograms: RwLock<HashMap<String, LatencyHistogram>>,
    /// Active HTTP connections gauge.
    pub active_connections: AtomicU64,
}

/// Predefined histogram bucket boundaries in milliseconds.
const BUCKET_BOUNDS_MS: &[u64] = &[1, 5, 10, 25, 50, 100, 250, 500, 1000, 5000];

/// Per-operation latency histogram with fixed buckets.
#[derive(Debug)]
struct LatencyHistogram {
    /// Count of observations in each bucket (le boundary).
    buckets: Vec<AtomicU64>,
    /// Total sum of all observed values in milliseconds.
    sum: AtomicU64,
    /// Total count of all observations.
    count: AtomicU64,
}

impl LatencyHistogram {
    fn new() -> Self {
        Self {
            buckets: BUCKET_BOUNDS_MS.iter().map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, duration_ms: u64) {
        self.sum.fetch_add(duration_ms, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // Find the first bucket whose bound >= duration and increment only that one.
        // The render phase accumulates cumulatively.
        for (i, &bound) in BUCKET_BOUNDS_MS.iter().enumerate() {
            if duration_ms <= bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        // If duration exceeds all bucket bounds, it only appears in +Inf (via count)
    }
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            counters: RwLock::new(HashMap::new()),
            histograms: RwLock::new(HashMap::new()),
            active_connections: AtomicU64::new(0),
        }
    }

    /// Record a completed request.
    pub fn record_request(&self, operation: &str, status: u16, duration_ms: u64) {
        let status_class = match status {
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };

        // Increment counter
        {
            let key = (operation.to_string(), status_class.to_string());
            let counters = self.counters.read().unwrap();
            if let Some(counter) = counters.get(&key) {
                counter.fetch_add(1, Ordering::Relaxed);
            } else {
                drop(counters);
                let mut counters = self.counters.write().unwrap();
                counters
                    .entry(key)
                    .or_insert_with(|| AtomicU64::new(0))
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // Record latency
        {
            let histograms = self.histograms.read().unwrap();
            if let Some(hist) = histograms.get(operation) {
                hist.observe(duration_ms);
            } else {
                drop(histograms);
                let mut histograms = self.histograms.write().unwrap();
                let hist = histograms
                    .entry(operation.to_string())
                    .or_insert_with(LatencyHistogram::new);
                hist.observe(duration_ms);
            }
        }
    }

    /// Render Prometheus text exposition format.
    pub fn render_prometheus(
        &self,
        bucket_count: u64,
        object_count: u64,
        total_size_bytes: u64,
    ) -> String {
        let mut out = String::with_capacity(4096);

        // Request counters
        out.push_str("# HELP arca_requests_total Total number of requests by operation and status class\n");
        out.push_str("# TYPE arca_requests_total counter\n");
        {
            let counters = self.counters.read().unwrap();
            let mut entries: Vec<_> = counters.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for ((op, status), count) in &entries {
                let val = count.load(Ordering::Relaxed);
                out.push_str(&format!(
                    "arca_requests_total{{operation=\"{op}\",status=\"{status}\"}} {val}\n"
                ));
            }
        }

        // Latency histograms
        out.push_str("\n# HELP arca_request_duration_ms Request duration histogram in milliseconds\n");
        out.push_str("# TYPE arca_request_duration_ms histogram\n");
        {
            let histograms = self.histograms.read().unwrap();
            let mut entries: Vec<_> = histograms.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (op, hist) in &entries {
                let mut cumulative = 0u64;
                for (i, &bound) in BUCKET_BOUNDS_MS.iter().enumerate() {
                    cumulative += hist.buckets[i].load(Ordering::Relaxed);
                    out.push_str(&format!(
                        "arca_request_duration_ms_bucket{{operation=\"{op}\",le=\"{bound}\"}} {cumulative}\n"
                    ));
                }
                let total_count = hist.count.load(Ordering::Relaxed);
                out.push_str(&format!(
                    "arca_request_duration_ms_bucket{{operation=\"{op}\",le=\"+Inf\"}} {total_count}\n"
                ));
                let sum = hist.sum.load(Ordering::Relaxed);
                out.push_str(&format!("arca_request_duration_ms_sum{{operation=\"{op}\"}} {sum}\n"));
                out.push_str(&format!("arca_request_duration_ms_count{{operation=\"{op}\"}} {total_count}\n"));
            }
        }

        // Gauges
        let active = self.active_connections.load(Ordering::Relaxed);
        out.push_str("\n# HELP arca_active_connections Current number of active HTTP connections\n");
        out.push_str("# TYPE arca_active_connections gauge\n");
        out.push_str(&format!("arca_active_connections {active}\n"));

        out.push_str("\n# HELP arca_buckets_total Total number of buckets\n");
        out.push_str("# TYPE arca_buckets_total gauge\n");
        out.push_str(&format!("arca_buckets_total {bucket_count}\n"));

        out.push_str("\n# HELP arca_objects_total Total number of objects\n");
        out.push_str("# TYPE arca_objects_total gauge\n");
        out.push_str(&format!("arca_objects_total {object_count}\n"));

        out.push_str("\n# HELP arca_storage_bytes_total Total storage size in bytes\n");
        out.push_str("# TYPE arca_storage_bytes_total gauge\n");
        out.push_str(&format!("arca_storage_bytes_total {total_size_bytes}\n"));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_render() {
        let reg = MetricsRegistry::new();
        reg.record_request("PutObject", 200, 15);
        reg.record_request("PutObject", 200, 25);
        reg.record_request("GetObject", 200, 5);
        reg.record_request("GetObject", 404, 2);

        let output = reg.render_prometheus(3, 100, 1024);
        assert!(output.contains("arca_requests_total{operation=\"PutObject\",status=\"2xx\"} 2"));
        assert!(output.contains("arca_requests_total{operation=\"GetObject\",status=\"2xx\"} 1"));
        assert!(output.contains("arca_requests_total{operation=\"GetObject\",status=\"4xx\"} 1"));
        assert!(output.contains("arca_request_duration_ms_count{operation=\"PutObject\"} 2"));
        assert!(output.contains("arca_request_duration_ms_sum{operation=\"PutObject\"} 40"));
        assert!(output.contains("arca_buckets_total 3"));
        assert!(output.contains("arca_objects_total 100"));
        assert!(output.contains("arca_storage_bytes_total 1024"));
    }

    #[test]
    fn histogram_buckets() {
        let reg = MetricsRegistry::new();
        reg.record_request("Test", 200, 3); // falls in 5ms bucket
        reg.record_request("Test", 200, 50); // falls in 50ms bucket
        reg.record_request("Test", 200, 9999); // falls in +Inf only

        let output = reg.render_prometheus(0, 0, 0);
        // 3ms <= 5, so bucket le=5 should have at least 1
        assert!(output.contains("arca_request_duration_ms_bucket{operation=\"Test\",le=\"5\"} 1"));
        // 50ms <= 50, cumulative includes the 3ms one too
        assert!(output.contains("arca_request_duration_ms_bucket{operation=\"Test\",le=\"50\"} 2"));
        // +Inf has all 3
        assert!(output.contains("arca_request_duration_ms_bucket{operation=\"Test\",le=\"+Inf\"} 3"));
    }

    #[test]
    fn active_connections_gauge() {
        let reg = MetricsRegistry::new();
        assert_eq!(reg.active_connections.load(Ordering::Relaxed), 0);
        reg.active_connections.fetch_add(1, Ordering::Relaxed);
        reg.active_connections.fetch_add(1, Ordering::Relaxed);
        assert_eq!(reg.active_connections.load(Ordering::Relaxed), 2);
        reg.active_connections.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(reg.active_connections.load(Ordering::Relaxed), 1);
    }
}
