import { api } from '../api.js';
import { nodeSelectorMixin } from '../node-selector.js?v=node-views-2';

// Operations grouped by category
const OP_CATEGORIES = {
  Bucket: ['ListBuckets','CreateBucket','DeleteBucket','HeadBucket','GetBucketLocation','GetBucketVersioning','PutBucketVersioning','GetBucketEncryption','PutBucketEncryption','DeleteBucketEncryption','GetBucketTagging','PutBucketTagging','DeleteBucketTagging'],
  Object: ['PutObject','GetObject','HeadObject','DeleteObject','DeleteObjects','CopyObject','GetObjectTagging','PutObjectTagging','DeleteObjectTagging'],
  Listing: ['ListObjectsV1','ListObjectsV2'],
  Multipart: ['CreateMultipartUpload','UploadPart','CompleteMultipartUpload','AbortMultipartUpload','ListMultipartUploads'],
  Admin: ['Admin::Health','Admin::Info','Admin::Stats','Admin::Me','Admin::Metrics','Admin::ListAudit','Admin::AuditStats','Admin::MetricsHistory','Admin::ListSettings','Admin::UpdateSetting','Admin::DeleteSetting','Admin::Presign','Admin::Archive'],
  Users: ['Admin::CreateUser','Admin::ListUsers','Admin::GetUser','Admin::UpdateUser','Admin::DeleteUser'],
  Teams: ['Admin::CreateTeam','Admin::ListTeams','Admin::GetTeam','Admin::UpdateTeam','Admin::DeleteTeam'],
  Grants: ['Admin::CreateGrant','Admin::ListGrants','Admin::GetGrant','Admin::UpdateGrant','Admin::DeleteGrant'],
  Creds: ['Admin::CreateCredential','Admin::ListCredentials','Admin::UpdateCredential','Admin::DeleteCredential'],
};
const OP_CAT_NAMES = Object.keys(OP_CATEGORIES);
const ALL_OP_NAMES = Object.values(OP_CATEGORIES).flat();
const S3_CATS = ['Bucket','Object','Listing','Multipart'];
const ADMIN_CATS = ['Admin','Users','Teams','Grants','Creds'];

// Smart presets
const OP_PRESETS = {
  'S3 Read': ALL_OP_NAMES.filter(o => /^(Get|Head|List)/.test(o) && S3_CATS.some(c => OP_CATEGORIES[c].includes(o))),
  'S3 Write': ALL_OP_NAMES.filter(o => /^(Put|Create|Delete|Copy|Upload|Complete|Abort)/.test(o) && S3_CATS.some(c => OP_CATEGORIES[c].includes(o))),
  'All S3': S3_CATS.flatMap(c => OP_CATEGORIES[c]),
  'All Admin': ADMIN_CATS.flatMap(c => OP_CATEGORIES[c]),
  'Data Changes': ALL_OP_NAMES.filter(o => /^(Put|Create|Delete|Copy|Upload|Complete|Admin::(Create|Update|Delete))/.test(o)),
};

// ==================== AUDIT LOG VIEW ====================
export function auditView() {
  return {
    // Cluster node selector (R8): the audit log is node-local.
    ...nodeSelectorMixin('audit'),

    // Raw data from server
    _rawEntries: [],
    _rawTotal: 0,

    loading: true,
    page: 0,
    limit: Number(sessionStorage.getItem('audit_page_size')) || 50,
    autoRefresh: null,
    selectedEntry: null,
    showClearModal: false,
    clearConfirmText: '',
    clearing: false,

    // Column header filters (persisted in sessionStorage)
    selectedBuckets: new Set(JSON.parse(sessionStorage.getItem('audit_f_buckets') || '[]')),
    bucketOpen: false,
    bucketSearch: '',
    selectedUsers: new Set(JSON.parse(sessionStorage.getItem('audit_f_users') || '[]')),
    userOpen: false,
    userSearch: '',
    filterKey: sessionStorage.getItem('audit_f_key') || '',
    keyEditing: false,
    filterFrom: sessionStorage.getItem('audit_f_from') || '',
    filterTo: sessionStorage.getItem('audit_f_to') || '',
    timeOpen: false,
    selectedStatuses: new Set(JSON.parse(sessionStorage.getItem('audit_f_statuses') || '[]')),
    statusOpen: false,
    statusSearch: '',

    // Operation filter (persisted in sessionStorage)
    opTags: JSON.parse(sessionStorage.getItem('audit_op_tags') || '[]'),
    opMode: sessionStorage.getItem('audit_op_mode') || 'include',
    opOpen: false,
    opExpandedCat: null,
    opCategories: OP_CATEGORIES,
    opCatNames: OP_CAT_NAMES,
    opPresets: OP_PRESETS,

    get opActivePreset() {
      const tagSet = new Set(this.opTags);
      for (const [name, ops] of Object.entries(OP_PRESETS)) {
        if (ops.length === tagSet.size && ops.every(o => tagSet.has(o))) return name;
      }
      return null;
    },

    // Distinct values for bucket/user dropdowns (computed from raw entries)
    get bucketValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const b = e.bucket || '(none)';
        counts[b] = (counts[b] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },

    get userValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const u = e.user_id || '(none)';
        counts[u] = (counts[u] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },

    get statusValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const s = String(e.http_status);
        counts[s] = (counts[s] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => Number(a[0]) - Number(b[0])).map(([name, count]) => ({ name, count }));
    },

    toggleSet(set, val) {
      if (set.has(val)) set.delete(val); else set.add(val);
    },

    _saveColumnFilters() {
      sessionStorage.setItem('audit_f_buckets', JSON.stringify([...this.selectedBuckets]));
      sessionStorage.setItem('audit_f_users', JSON.stringify([...this.selectedUsers]));
      sessionStorage.setItem('audit_f_statuses', JSON.stringify([...this.selectedStatuses]));
      sessionStorage.setItem('audit_f_key', this.filterKey);
      sessionStorage.setItem('audit_f_from', this.filterFrom);
      sessionStorage.setItem('audit_f_to', this.filterTo);
    },

    // All entries after applying ALL client-side filters (operation tags + column headers)
    get allFiltered() {
      let entries = this._rawEntries;

      // Operation tag filter
      if (this.opTags.length > 0) {
        const tagSet = new Set(this.opTags);
        entries = this.opMode === 'include'
          ? entries.filter(e => tagSet.has(e.operation))
          : entries.filter(e => !tagSet.has(e.operation));
      }

      // Column header filters
      if (this.selectedBuckets.size > 0) {
        entries = entries.filter(e => this.selectedBuckets.has(e.bucket || '(none)'));
      }
      if (this.selectedUsers.size > 0) {
        entries = entries.filter(e => this.selectedUsers.has(e.user_id || '(none)'));
      }
      if (this.filterKey) {
        const k = this.filterKey.toLowerCase();
        entries = entries.filter(e => (e.key || '').toLowerCase().includes(k));
      }
      if (this.selectedStatuses.size > 0) {
        entries = entries.filter(e => this.selectedStatuses.has(String(e.http_status)));
      }

      return entries;
    },

    // Current page slice
    get filteredEntries() {
      const start = this.page * this.limit;
      return this.allFiltered.slice(start, start + this.limit);
    },

    get total() { return this.allFiltered.length; },
    get currentPage() { return this.page + 1; },
    get totalPages() { return Math.max(1, Math.ceil(this.total / this.limit)); },

    // Time label for the header badge
    timeLabel() {
      const fmt = (s) => new Date(s).toLocaleDateString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
      const f = this.filterFrom ? fmt(this.filterFrom) : '';
      const t = this.filterTo ? fmt(this.filterTo) : '';
      if (f && t) return f + ' \u2014 ' + t;
      if (f) return 'From ' + f;
      if (t) return 'Until ' + t;
      return '';
    },

    // Operation filter helpers
    _saveOpFilter() {
      sessionStorage.setItem('audit_op_tags', JSON.stringify(this.opTags));
      sessionStorage.setItem('audit_op_mode', this.opMode);
    },

    toggleOpMode() { this.opMode = this.opMode === 'include' ? 'exclude' : 'include'; this.page = 0; this._saveOpFilter(); },
    clearOpTags() { this.opTags = []; this.page = 0; this._saveOpFilter(); },
    selectAllOps() { this.opTags = [...ALL_OP_NAMES]; this.page = 0; this._saveOpFilter(); },

    applyOpPreset(name) {
      this.opTags = [...OP_PRESETS[name]];
      this.page = 0; this._saveOpFilter();
    },

    toggleOpCat(cat) {
      const ops = OP_CATEGORIES[cat];
      const tagSet = new Set(this.opTags);
      const allSelected = ops.every(o => tagSet.has(o));
      if (allSelected) {
        this.opTags = this.opTags.filter(t => !ops.includes(t));
      } else {
        const merged = new Set(this.opTags);
        ops.forEach(o => merged.add(o));
        this.opTags = [...merged];
      }
      this.page = 0; this._saveOpFilter();
    },

    toggleSingleOp(op) {
      const idx = this.opTags.indexOf(op);
      if (idx >= 0) this.opTags.splice(idx, 1); else this.opTags.push(op);
      this.page = 0; this._saveOpFilter();
    },

    opCatSelected(cat) {
      const ops = OP_CATEGORIES[cat];
      const tagSet = new Set(this.opTags);
      return ops.filter(o => tagSet.has(o)).length;
    },

    opCatClass(cat) {
      const sel = this.opCatSelected(cat);
      if (sel === 0) return '';
      return sel === OP_CATEGORIES[cat].length ? 'selected' : 'partial';
    },

    get hasAnyFilter() {
      return this.opTags.length > 0 || this.selectedBuckets.size > 0 || this.selectedUsers.size > 0 || this.selectedStatuses.size > 0 || this.filterKey || this.filterFrom || this.filterTo;
    },

    clearAllFilters() {
      this.opTags = [];
      this.selectedBuckets = new Set(); this.selectedUsers = new Set(); this.selectedStatuses = new Set();
      this.filterKey = ''; this.filterFrom = ''; this.filterTo = '';
      this.page = 0; this._saveOpFilter(); this._saveColumnFilters(); this.load();
    },

    async load() {
      this.loading = true;
      this.limit = Number(this.limit) || 50;
      sessionStorage.setItem('audit_page_size', this.limit);
      try {
        const params = new URLSearchParams();
        params.set('limit', '1000');
        params.set('offset', '0');
        // Date range is sent server-side for efficiency
        if (this.filterFrom) params.set('from', new Date(this.filterFrom).toISOString());
        if (this.filterTo) params.set('to', new Date(this.filterTo).toISOString());
        const data = await api.adminGet('/audit?' + params.toString() + this.nodeQuery());
        this._rawEntries = data.entries || [];
        this._rawTotal = data.total || 0;
        this.captureNodeMeta(data);
      } catch (e) {
        console.error('Failed to load audit log:', e);
        // Stale rows from another node would be misleading: clear, and say
        // which node failed (a proxied peer can be down or ineligible).
        this._rawEntries = [];
        this._rawTotal = 0;
        this.captureNodeMeta({});
        if (this.selectedNode) {
          this.$dispatch('show-toast', { message: this.nodeErrorMessage(e), type: 'error' });
        }
      }
      this.loading = false;
    },

    init() {
      this.loadNodes();
      this.load();
      this.autoRefresh = setInterval(() => this.load(), 30000);
      // Auto-persist column filters whenever they change
      this.$watch('selectedBuckets', () => this._saveColumnFilters());
      this.$watch('selectedUsers', () => this._saveColumnFilters());
      this.$watch('filterKey', () => this._saveColumnFilters());
      this.$watch('filterFrom', () => this._saveColumnFilters());
      this.$watch('filterTo', () => this._saveColumnFilters());
      this.$watch('selectedStatuses', () => this._saveColumnFilters());
    },
    destroy() { if (this.autoRefresh) clearInterval(this.autoRefresh); },

    firstPage() { this.page = 0; },
    prevPage() { if (this.page > 0) this.page--; },
    nextPage() { if (this.currentPage < this.totalPages) this.page++; },
    lastPage() { this.page = this.totalPages - 1; },

    selectEntry(entry) { this.selectedEntry = this.selectedEntry?.id === entry.id ? null : entry; },

    async clearAllAudit() {
      this.clearing = true;
      try {
        const resp = await api.adminDelete('/audit', { confirm: 'CLEAR AUDIT LOG' });
        if (!resp.ok) { const body = await resp.json(); throw new Error(body.message || `Error ${resp.status}`); }
        this.showClearModal = false; this.clearConfirmText = ''; this.page = 0; await this.load();
      } catch (e) { this.$dispatch('show-toast', { message: 'Clear failed: ' + e.message, type: 'error' }); }
      this.clearing = false;
    },

    statusClass(status) {
      if (status >= 200 && status < 300) return 'text-green-400';
      if (status >= 400 && status < 500) return 'text-amber-400';
      return 'text-red-400';
    },
    formatTime(ts) { return ts ? new Date(ts).toLocaleString() : ''; },
  };
}
