import { api } from '../api.js';

// ==================== MAINTENANCE — SHARED ====================

// Job types presented in the launch panel. Structured as a table so M2/M3/M4
// can append more types (compact, fsck, rebalance, …) with their own param
// schema without touching the view logic. `paramsKind` selects which small
// params form the launch panel renders; 'noop' is the only one for now.
const JOB_TYPES = [
  { id: 'noop', label: 'No-op (test)', paramsKind: 'noop' },
  { id: 'encrypt', label: 'Encrypt objects (SSE-S3)', paramsKind: 'reencrypt' },
  { id: 'decrypt', label: 'Decrypt objects', paramsKind: 'reencrypt' },
];

// Status pill styling — mirrors the STATUS_META idiom used by the replication
// journal (badge + dot + pulse), extended to the maintenance status set.
const STATUS_META = {
  pending:   { badge: 'bg-yellow-500/20 text-yellow-400',  dot: 'bg-yellow-400', pulse: false },
  running:   { badge: 'bg-cyan-500/20 text-cyan-300',      dot: 'bg-cyan-400',   pulse: true  },
  paused:    { badge: 'bg-amber-500/20 text-amber-400',    dot: 'bg-amber-400',  pulse: false },
  completed: { badge: 'bg-green-500/20 text-green-400',    dot: 'bg-green-400',  pulse: false },
  failed:    { badge: 'bg-red-500/20 text-red-400',        dot: 'bg-red-400',    pulse: false },
  cancelled: { badge: 'bg-slate-500/20 text-slate-300',    dot: 'bg-slate-400',  pulse: false },
};

// Mode chip styling. Maintenance mode is the consequential one (it drains S3
// on the node), so it carries the louder colour.
const MODE_META = {
  live:        { badge: 'bg-emerald-500/15 text-emerald-400 border-emerald-500/30', label: 'Live' },
  maintenance: { badge: 'bg-orange-500/15 text-orange-400 border-orange-500/30',    label: 'Maintenance' },
};

function jobTypeMeta(id) {
  return JOB_TYPES.find(t => t.id === id) || { id, label: id, paramsKind: 'none' };
}

// A job is "active" for the purposes of governing controls when it is still
// in a non-terminal state.
const ACTIVE_STATUSES = new Set(['pending', 'running', 'paused']);

// ==================== MAINTENANCE VIEW ====================
// Layout mirrors the replication view's idioms: a header with a live
// auto-refresh badge + manual reload, glass cards, a status-pill table, and a
// side detail panel. The hero is the active-job card with its progress bar.
export function maintenanceView() {
  return {
    // Server state
    active: null,          // the in-flight job, or null
    jobs: [],              // history, newest first
    logs: [],              // logs for the active (or selected) job
    loading: true,
    autoRefresh: null,

    // Launch form
    launch: {
      type: 'noop',
      mode: 'live',
      // noop params. A non-zero default delay makes the test job run long
      // enough to actually observe + pause/cancel it (with delay 0 a small
      // noop completes in well under one worker tick).
      n: 100,
      delayMs: 200,
      // re-encryption params (encrypt / decrypt). All optional: a blank
      // bucket scans every bucket, a blank prefix scans every key, and the
      // throttle only applies in live mode (0 = unlimited).
      bucket: '',
      prefix: '',
      rateBytesPerSec: 0,
    },
    launching: false,
    launchError: '',

    // Per-job action state (keyed by job id) so buttons can show a spinner
    // without freezing the whole card.
    acting: {},

    // Detail panel — a history row clicked open. Holds its own job + logs so it
    // doesn't fight the active-job auto-refresh.
    selectedJob: null,
    selectedLogs: [],
    detailLoading: false,

    // History pagination (client-side, like the audit log) + clear-history modal.
    historyPage: 0,
    historyPageSize: 20,
    showClearModal: false,
    clearing: false,

    jobTypes: JOB_TYPES,

    // ---- lifecycle ----
    init() {
      this.load();
      // Faster cadence than the journal views (2s): an active job's progress
      // bar and throughput are the whole point of this page.
      this.autoRefresh = setInterval(() => this.tick(), 2000);
    },
    destroy() { if (this.autoRefresh) clearInterval(this.autoRefresh); },

    // The interval handler. Always refresh the active job + history while the
    // view is open; refresh the open detail panel only while its job is active
    // (a terminal job won't change).
    async tick() {
      await this.load();
      if (this.selectedJob && this.isActive(this.selectedJob)) {
        await this.refreshSelected();
      }
    },

    async load() {
      try {
        const data = await api.adminGet('/maintenance/jobs?limit=200');
        // Merge the active job into the existing object reference rather than
        // replacing it, so the active-job card + progress bar update in place
        // instead of being torn down and rebuilt on every 2s poll (flicker).
        this.active = mergeInto(this.active, data.active || null);
        this.jobs = data.jobs || [];
        // Clamp the history page if rows were removed (e.g. after a clear).
        const maxPage = Math.max(0, this.historyTotalPages - 1);
        if (this.historyPage > maxPage) this.historyPage = maxPage;
        // Keep the logs panel tracking the active job. When no job is active we
        // leave the last logs in place until the user opens a detail panel.
        if (this.active) {
          await this.loadActiveLogs(this.active.id);
        }
      } catch (e) {
        console.error('Failed to load maintenance jobs:', e);
      }
      this.loading = false;
    },

    async loadActiveLogs(id) {
      try {
        const data = await api.adminGet('/maintenance/jobs/' + encodeURIComponent(id));
        // Newest first.
        this.logs = (data.logs || []).slice().reverse();
      } catch {
        this.logs = [];
      }
    },

    // ---- launch ----
    get jobActive() { return this.active != null; },

    get launchParamsKind() { return jobTypeMeta(this.launch.type).paramsKind; },

    async startJob() {
      if (this.jobActive) return;
      this.launchError = '';

      const meta = jobTypeMeta(this.launch.type);
      const params = {};
      if (meta.paramsKind === 'noop') {
        const n = Number(this.launch.n);
        if (!Number.isInteger(n) || n < 1) {
          this.launchError = 'Steps must be a whole number of 1 or more.';
          return;
        }
        params.n = n;
        const delay = Number(this.launch.delayMs);
        if (delay) {
          if (!Number.isInteger(delay) || delay < 0) {
            this.launchError = 'Delay must be 0 or a positive number of milliseconds.';
            return;
          }
          params.delay_ms = delay;
        }
      } else if (meta.paramsKind === 'reencrypt') {
        // bucket / prefix are optional scoping filters — omit when blank so
        // the job scans all buckets / all keys.
        const bucket = (this.launch.bucket || '').trim();
        if (bucket) params.bucket = bucket;
        const prefix = (this.launch.prefix || '').trim();
        if (prefix) params.prefix = prefix;
        // Throttle only matters in live mode (copy-on-write). Omit when blank
        // or zero (= unlimited).
        if (this.launch.mode === 'live') {
          const rate = Number(this.launch.rateBytesPerSec);
          if (rate) {
            if (!Number.isInteger(rate) || rate < 0) {
              this.launchError = 'Throttle must be 0 or a positive number of bytes per second.';
              return;
            }
            params.rate_bytes_per_sec = rate;
          }
        }
      }

      this.launching = true;
      try {
        const resp = await api.adminPost('/maintenance/jobs', {
          type: this.launch.type,
          mode: this.launch.mode,
          params,
        });
        if (!resp.ok) {
          let body = {};
          try { body = await resp.json(); } catch {}
          if (resp.status === 409) {
            this.launchError = body.message || 'A job is already running. Only one job runs at a time.';
          } else if (resp.status === 400) {
            this.launchError = body.message || body.error || 'Invalid job request.';
          } else {
            this.launchError = body.message || body.error || `Could not start job (HTTP ${resp.status}).`;
          }
          return;
        }
        await this.load();
      } catch (e) {
        this.launchError = e.message || 'Could not start job.';
      } finally {
        this.launching = false;
      }
    },

    // ---- governing actions (pause / resume / cancel) ----
    async _act(id, verb, method) {
      this.acting = { ...this.acting, [id]: true };
      try {
        const path = '/maintenance/jobs/' + encodeURIComponent(id) +
          (verb === 'cancel' ? '' : '/' + verb);
        // Guard against a request that never settles so the action buttons can
        // never get permanently wedged (the finally below always re-enables them).
        const reqP = method === 'DELETE'
          ? api.adminDelete(path)
          : api.adminPost(path);
        const resp = await Promise.race([
          reqP,
          new Promise((_, reject) =>
            setTimeout(() => reject(new Error('request timed out')), 15000)),
        ]);
        if (!resp.ok) {
          let body = {};
          try { body = await resp.json(); } catch {}
          throw new Error(body.message || body.error || `HTTP ${resp.status}`);
        }
        await this.load();
        if (this.selectedJob?.id === id) await this.refreshSelected();
      } catch (e) {
        this.$dispatch('show-toast', { message: capitalize(verb) + ' failed: ' + e.message, type: 'error' });
      } finally {
        this.acting = { ...this.acting, [id]: false };
      }
    },

    pauseJob(id) { return this._act(id, 'pause', 'POST'); },
    resumeJob(id) { return this._act(id, 'resume', 'POST'); },
    cancelJob(id) { return this._act(id, 'cancel', 'DELETE'); },

    // ---- detail panel ----
    async selectJob(job) {
      if (this.selectedJob?.id === job.id) { this.selectedJob = null; return; }
      this.selectedJob = job;
      this.detailLoading = true;
      await this.refreshSelected();
      this.detailLoading = false;
    },

    async refreshSelected() {
      if (!this.selectedJob) return;
      try {
        const data = await api.adminGet('/maintenance/jobs/' + encodeURIComponent(this.selectedJob.id));
        // Merge in place (stable reference) so the open detail pane updates its
        // progress bar smoothly instead of flickering on each poll.
        this.selectedJob = mergeInto(this.selectedJob, data.job || this.selectedJob);
        this.selectedLogs = (data.logs || []).slice().reverse();
      } catch (e) {
        console.error('Failed to load job detail:', e);
        this.selectedLogs = [];
      }
    },

    closeDetail() { this.selectedJob = null; this.selectedLogs = []; },

    // ---- history pagination (client-side, mirrors the audit log) ----
    get historyTotalPages() {
      return Math.max(1, Math.ceil(this.jobs.length / this.historyPageSize));
    },
    get pagedJobs() {
      const start = this.historyPage * this.historyPageSize;
      return this.jobs.slice(start, start + this.historyPageSize);
    },
    get historyHasPrev() { return this.historyPage > 0; },
    get historyHasNext() { return this.historyPage < this.historyTotalPages - 1; },
    prevHistoryPage() { if (this.historyHasPrev) this.historyPage--; },
    nextHistoryPage() { if (this.historyHasNext) this.historyPage++; },

    // ---- clear history ----
    async clearHistory() {
      this.clearing = true;
      try {
        const resp = await api.adminDelete('/maintenance/jobs');
        if (!resp.ok) {
          let body = {};
          try { body = await resp.json(); } catch {}
          throw new Error(body.message || body.error || `HTTP ${resp.status}`);
        }
        this.showClearModal = false;
        this.historyPage = 0;
        // If the open detail panel pointed at a now-deleted terminal job, close it.
        if (this.selectedJob && !this.isActive(this.selectedJob)) this.closeDetail();
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Clear failed: ' + e.message, type: 'error' });
      } finally {
        this.clearing = false;
      }
    },

    // ---- computed display helpers ----
    isActive(job) { return job ? ACTIVE_STATUSES.has(job.status) : false; },

    canPause(job) { return job && (job.status === 'running' || job.status === 'pending'); },
    canResume(job) { return job && job.status === 'paused'; },
    canCancel(job) { return job && ACTIVE_STATUSES.has(job.status); },

    progressPct(job) {
      if (!job || !job.total || job.total <= 0) return 0;
      return Math.min(100, (job.done / job.total) * 100);
    },

    progressLabel(job) {
      if (!job) return '—';
      const total = job.total || 0;
      const done = job.done || 0;
      return done + ' / ' + total;
    },

    // Throughput as steps/s, formatted compactly.
    rateLabel(job) {
      if (!job || !job.rate) return '—';
      const r = job.rate;
      const fixed = r >= 100 ? Math.round(r) : r.toFixed(1);
      return fixed + '/s';
    },

    // ETA = remaining work / current rate. Only meaningful while a job is
    // running with a positive rate and a known total.
    etaLabel(job) {
      if (!job || !job.rate || job.rate <= 0 || !job.total || job.total <= 0) return '—';
      const remaining = Math.max(0, job.total - (job.done || 0));
      if (remaining === 0) return '0s';
      const secs = remaining / job.rate;
      return formatDuration(secs);
    },

    statusBadgeClass(status) { return STATUS_META[status]?.badge || 'bg-gray-500/20 text-gray-400'; },
    statusDotClass(status) { return STATUS_META[status]?.dot || 'bg-gray-400'; },
    statusShouldPulse(status) { return STATUS_META[status]?.pulse === true; },

    modeBadgeClass(mode) { return MODE_META[mode]?.badge || 'bg-slate-500/15 text-slate-300 border-slate-500/30'; },
    modeLabel(mode) { return MODE_META[mode]?.label || mode; },

    jobTypeLabel(id) { return jobTypeMeta(id).label; },

    logLevelClass(level) {
      const l = (level || '').toLowerCase();
      if (l === 'error') return 'text-red-400';
      if (l === 'warn' || l === 'warning') return 'text-amber-400';
      return 'text-vault-muted';
    },

    formatTime(ts) { return ts ? new Date(ts).toLocaleString() : '—'; },
  };
}

// ---- module-local utilities ----
function capitalize(s) { return s ? s.charAt(0).toUpperCase() + s.slice(1) : s; }

// Merge `next` into the existing `prev` object IN PLACE when they refer to the
// same job (same id), preserving the object reference so Alpine updates bound
// DOM in place rather than tearing it down (avoids progress-bar flicker on the
// 2s poll). Returns the object to assign back: `prev` mutated, or `next` when
// the identity changed or either side is null.
function mergeInto(prev, next) {
  if (!next) return null;
  if (prev && prev.id === next.id) {
    Object.assign(prev, next);
    return prev;
  }
  return next;
}

function formatDuration(seconds) {
  if (!isFinite(seconds) || seconds < 0) return '—';
  const s = Math.round(seconds);
  if (s < 60) return s + 's';
  const m = Math.floor(s / 60);
  const rem = s % 60;
  if (m < 60) return rem ? `${m}m ${rem}s` : `${m}m`;
  const h = Math.floor(m / 60);
  const remM = m % 60;
  return remM ? `${h}h ${remM}m` : `${h}h`;
}
