import { api } from '../api.js';

const ALL_OPERATIONS = [
  { op: 'ListBuckets', cat: 'Bucket' }, { op: 'CreateBucket', cat: 'Bucket' }, { op: 'DeleteBucket', cat: 'Bucket' },
  { op: 'HeadBucket', cat: 'Bucket' }, { op: 'GetBucketLocation', cat: 'Bucket' },
  { op: 'GetBucketVersioning', cat: 'Bucket' }, { op: 'PutBucketVersioning', cat: 'Bucket' },
  { op: 'GetBucketEncryption', cat: 'Bucket' }, { op: 'PutBucketEncryption', cat: 'Bucket' }, { op: 'DeleteBucketEncryption', cat: 'Bucket' },
  { op: 'PutObject', cat: 'Object' }, { op: 'GetObject', cat: 'Object' }, { op: 'HeadObject', cat: 'Object' },
  { op: 'DeleteObject', cat: 'Object' }, { op: 'DeleteObjects', cat: 'Object' }, { op: 'CopyObject', cat: 'Object' },
  { op: 'ListObjectsV1', cat: 'Listing' }, { op: 'ListObjectsV2', cat: 'Listing' },
  { op: 'CreateMultipartUpload', cat: 'Multipart' }, { op: 'UploadPart', cat: 'Multipart' },
  { op: 'CompleteMultipartUpload', cat: 'Multipart' }, { op: 'AbortMultipartUpload', cat: 'Multipart' },
  { op: 'ListMultipartUploads', cat: 'Multipart' },
  { op: 'Admin::Health', cat: 'Admin' }, { op: 'Admin::Info', cat: 'Admin' }, { op: 'Admin::Stats', cat: 'Admin' },
  { op: 'Admin::Me', cat: 'Admin' }, { op: 'Admin::Metrics', cat: 'Admin' }, { op: 'Admin::ListAudit', cat: 'Admin' },
  { op: 'Admin::AuditStats', cat: 'Admin' }, { op: 'Admin::MetricsHistory', cat: 'Admin' },
  { op: 'Admin::ListSettings', cat: 'Admin' }, { op: 'Admin::UpdateSetting', cat: 'Admin' },
  { op: 'Admin::DeleteSetting', cat: 'Admin' }, { op: 'Admin::Presign', cat: 'Admin' }, { op: 'Admin::Archive', cat: 'Admin' },
  { op: 'Admin::CreateUser', cat: 'Users' }, { op: 'Admin::ListUsers', cat: 'Users' },
  { op: 'Admin::GetUser', cat: 'Users' }, { op: 'Admin::UpdateUser', cat: 'Users' }, { op: 'Admin::DeleteUser', cat: 'Users' },
  { op: 'Admin::CreateTeam', cat: 'Teams' }, { op: 'Admin::ListTeams', cat: 'Teams' },
  { op: 'Admin::GetTeam', cat: 'Teams' }, { op: 'Admin::UpdateTeam', cat: 'Teams' }, { op: 'Admin::DeleteTeam', cat: 'Teams' },
  { op: 'Admin::CreateGrant', cat: 'Grants' }, { op: 'Admin::ListGrants', cat: 'Grants' },
  { op: 'Admin::GetGrant', cat: 'Grants' }, { op: 'Admin::UpdateGrant', cat: 'Grants' }, { op: 'Admin::DeleteGrant', cat: 'Grants' },
  { op: 'Admin::CreateCredential', cat: 'Creds' }, { op: 'Admin::ListCredentials', cat: 'Creds' },
  { op: 'Admin::UpdateCredential', cat: 'Creds' }, { op: 'Admin::DeleteCredential', cat: 'Creds' },
];

// ==================== AUDIT LOG VIEW ====================
export function auditView() {
  return {
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

    // Operation tag filter (persisted in sessionStorage)
    opTags: JSON.parse(sessionStorage.getItem('audit_op_tags') || '[]'),
    opMode: sessionStorage.getItem('audit_op_mode') || 'include',
    opOpen: false,
    opQuery: '',
    opFocused: false,
    opShowAC: false,
    opHighlight: 0,

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

    // Operation tag autocomplete
    get opFiltered() {
      const q = this.opQuery.toLowerCase();
      const tagSet = new Set(this.opTags);
      return ALL_OPERATIONS.filter(o => !tagSet.has(o.op) && (!q || o.op.toLowerCase().includes(q))).slice(0, 12);
    },

    opHighlightText(text) {
      if (!this.opQuery) return text;
      const idx = text.toLowerCase().indexOf(this.opQuery.toLowerCase());
      if (idx === -1) return text;
      return text.slice(0, idx) + '<span class="text-vault-accent">' + text.slice(idx, idx + this.opQuery.length) + '</span>' + text.slice(idx + this.opQuery.length);
    },

    _saveOpFilter() {
      sessionStorage.setItem('audit_op_tags', JSON.stringify(this.opTags));
      sessionStorage.setItem('audit_op_mode', this.opMode);
    },

    addOpTag(op) {
      if (!this.opTags.includes(op)) this.opTags.push(op);
      this.opQuery = ''; this.opHighlight = 0; this.page = 0; this._saveOpFilter();
    },
    removeOpTag(op) { this.opTags = this.opTags.filter(t => t !== op); this.page = 0; this._saveOpFilter(); },
    addOpHighlighted() {
      if (this.opFiltered.length > 0) this.addOpTag((this.opFiltered[this.opHighlight] || this.opFiltered[0]).op);
    },
    onOpBackspace() { if (this.opQuery === '' && this.opTags.length > 0) { this.opTags.pop(); this.page = 0; this._saveOpFilter(); } },
    moveOpHighlight(dir) { this.opHighlight = Math.max(0, Math.min(this.opFiltered.length - 1, this.opHighlight + dir)); },
    toggleOpMode() { this.opMode = this.opMode === 'include' ? 'exclude' : 'include'; this.page = 0; this._saveOpFilter(); },
    clearOpTags() { this.opTags = []; this.opQuery = ''; this.page = 0; this._saveOpFilter(); },

    get hasAnyFilter() {
      return this.opTags.length > 0 || this.selectedBuckets.size > 0 || this.selectedUsers.size > 0 || this.selectedStatuses.size > 0 || this.filterKey || this.filterFrom || this.filterTo;
    },

    clearAllFilters() {
      this.opTags = []; this.opQuery = '';
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
        const data = await api.adminGet('/audit?' + params.toString());
        this._rawEntries = data.entries || [];
        this._rawTotal = data.total || 0;
      } catch (e) { console.error('Failed to load audit log:', e); }
      this.loading = false;
    },

    init() {
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
