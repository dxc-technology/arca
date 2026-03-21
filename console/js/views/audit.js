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
    filterBucket: '',
    filterUser: '',
    autoRefresh: null,
    selectedEntry: null,

    // Operation tag filter (persisted in sessionStorage)
    opTags: JSON.parse(sessionStorage.getItem('audit_op_tags') || '[]'),
    opMode: sessionStorage.getItem('audit_op_mode') || 'include',
    opQuery: '',
    opFocused: false,
    opShowAC: false,
    opHighlight: 0,

    // All entries after applying operation filter (client-side)
    get allFiltered() {
      if (this.opTags.length === 0) return this._rawEntries;
      const tagSet = new Set(this.opTags);
      if (this.opMode === 'include') {
        return this._rawEntries.filter(e => tagSet.has(e.operation));
      }
      return this._rawEntries.filter(e => !tagSet.has(e.operation));
    },

    // Current page slice
    get filteredEntries() {
      const start = this.page * this.limit;
      return this.allFiltered.slice(start, start + this.limit);
    },

    get total() {
      return this.allFiltered.length;
    },

    get currentPage() { return this.page + 1; },
    get totalPages() { return Math.max(1, Math.ceil(this.total / this.limit)); },

    get opFiltered() {
      const q = this.opQuery.toLowerCase();
      const tagSet = new Set(this.opTags);
      return ALL_OPERATIONS
        .filter(o => !tagSet.has(o.op) && (!q || o.op.toLowerCase().includes(q)))
        .slice(0, 12);
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
      this.opQuery = '';
      this.opHighlight = 0;
      this.page = 0;
      this._saveOpFilter();
    },

    removeOpTag(op) {
      this.opTags = this.opTags.filter(t => t !== op);
      this.page = 0;
      this._saveOpFilter();
    },

    addOpHighlighted() {
      if (this.opFiltered.length > 0) {
        const item = this.opFiltered[this.opHighlight] || this.opFiltered[0];
        this.addOpTag(item.op);
      }
    },

    onOpBackspace() {
      if (this.opQuery === '' && this.opTags.length > 0) {
        this.opTags.pop();
        this.page = 0;
        this._saveOpFilter();
      }
    },

    moveOpHighlight(dir) {
      this.opHighlight = Math.max(0, Math.min(this.opFiltered.length - 1, this.opHighlight + dir));
    },

    toggleOpMode() {
      this.opMode = this.opMode === 'include' ? 'exclude' : 'include';
      this.page = 0;
      this._saveOpFilter();
    },

    clearOpTags() {
      this.opTags = [];
      this.opQuery = '';
      this.page = 0;
      this._saveOpFilter();
    },

    async load() {
      this.loading = true;
      this.limit = Number(this.limit) || 50;
      sessionStorage.setItem('audit_page_size', this.limit);
      try {
        const params = new URLSearchParams();
        // Fetch a large batch; operation filtering + pagination happen client-side
        params.set('limit', '1000');
        params.set('offset', '0');
        if (this.filterBucket) params.set('bucket', this.filterBucket);
        if (this.filterUser) params.set('user_id', this.filterUser);
        const data = await api.adminGet('/audit?' + params.toString());
        this._rawEntries = data.entries || [];
        this._rawTotal = data.total || 0;
      } catch (e) {
        console.error('Failed to load audit log:', e);
      }
      this.loading = false;
    },

    init() {
      this.load();
      this.autoRefresh = setInterval(() => this.load(), 30000);
    },

    destroy() {
      if (this.autoRefresh) clearInterval(this.autoRefresh);
    },

    applyFilters() {
      this.page = 0;
      this.load();
    },

    clearFilters() {
      this.filterBucket = '';
      this.filterUser = '';
      this.opTags = [];
      this.opQuery = '';
      this.page = 0;
      this.load();
    },

    get hasAnyFilter() {
      return this.filterBucket || this.filterUser || this.opTags.length > 0;
    },

    firstPage() { this.page = 0; },
    prevPage() { if (this.page > 0) this.page--; },
    nextPage() { if (this.currentPage < this.totalPages) this.page++; },
    lastPage() { this.page = this.totalPages - 1; },

    selectEntry(entry) {
      this.selectedEntry = this.selectedEntry?.id === entry.id ? null : entry;
    },

    statusClass(status) {
      if (status >= 200 && status < 300) return 'text-green-400';
      if (status >= 400 && status < 500) return 'text-amber-400';
      return 'text-red-400';
    },

    formatTime(ts) {
      if (!ts) return '';
      return new Date(ts).toLocaleString();
    },
  };
}
