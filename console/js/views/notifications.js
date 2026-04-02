import { api } from '../api.js';

// ==================== NOTIFICATIONS VIEW ====================
// Global notification event log viewer (admin only).
// Replicates the audit log view pattern exactly: inline column header filters,
// side panel detail, pagination bar, clear-all with confirmation.
export function notificationsView() {
  return {
    _rawEntries: [],
    _rawTotal: 0,

    loading: true,
    page: 0,
    limit: Number(sessionStorage.getItem('notif_page_size')) || 50,
    autoRefresh: null,
    selectedEntry: null,
    showClearModal: false,
    clearConfirmText: '',
    clearing: false,

    // Column header filters (persisted in sessionStorage) — same types as audit
    filterFrom: sessionStorage.getItem('notif_f_from') || '',
    filterTo: sessionStorage.getItem('notif_f_to') || '',
    timeOpen: false,
    selectedEvents: new Set(JSON.parse(sessionStorage.getItem('notif_f_events') || '[]')),
    eventOpen: false,
    selectedBuckets: new Set(JSON.parse(sessionStorage.getItem('notif_f_buckets') || '[]')),
    bucketOpen: false,
    bucketSearch: '',
    filterKey: sessionStorage.getItem('notif_f_key') || '',
    keyEditing: false,
    filterDestination: sessionStorage.getItem('notif_f_dest') || '',
    destEditing: false,
    selectedStatuses: new Set(JSON.parse(sessionStorage.getItem('notif_f_statuses') || '[]')),
    statusOpen: false,

    // Computed distinct values for dropdown filters
    get eventValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const ev = e.event_name || '(none)';
        counts[ev] = (counts[ev] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },
    get bucketValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const b = e.bucket || '(none)';
        counts[b] = (counts[b] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },
    get statusValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const s = e.delivery_status || '(none)';
        counts[s] = (counts[s] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },

    toggleSet(set, val) {
      if (set.has(val)) set.delete(val); else set.add(val);
    },

    _saveFilters() {
      sessionStorage.setItem('notif_f_from', this.filterFrom);
      sessionStorage.setItem('notif_f_to', this.filterTo);
      sessionStorage.setItem('notif_f_buckets', JSON.stringify([...this.selectedBuckets]));
      sessionStorage.setItem('notif_f_key', this.filterKey);
      sessionStorage.setItem('notif_f_dest', this.filterDestination);
      sessionStorage.setItem('notif_f_events', JSON.stringify([...this.selectedEvents]));
      sessionStorage.setItem('notif_f_statuses', JSON.stringify([...this.selectedStatuses]));
    },

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

    get allFiltered() {
      let entries = this._rawEntries;
      if (this.selectedEvents.size > 0) {
        entries = entries.filter(e => this.selectedEvents.has(e.event_name));
      }
      if (this.selectedBuckets.size > 0) {
        entries = entries.filter(e => this.selectedBuckets.has(e.bucket || '(none)'));
      }
      if (this.filterKey) {
        const q = this.filterKey.toLowerCase();
        entries = entries.filter(e => (e.key || '').toLowerCase().includes(q));
      }
      if (this.filterDestination) {
        const q = this.filterDestination.toLowerCase();
        entries = entries.filter(e => (e.destination_url || '').toLowerCase().includes(q));
      }
      if (this.selectedStatuses.size > 0) {
        entries = entries.filter(e => this.selectedStatuses.has(e.delivery_status));
      }
      return entries;
    },

    get pagedEntries() {
      const start = this.page * this.limit;
      return this.allFiltered.slice(start, start + this.limit);
    },

    get total() { return this.allFiltered.length; },
    get currentPage() { return this.page + 1; },
    get totalPages() { return Math.max(1, Math.ceil(this.total / this.limit)); },

    get hasAnyFilter() {
      return this.filterFrom || this.filterTo || this.selectedEvents.size > 0 || this.selectedBuckets.size > 0 || this.filterKey || this.filterDestination || this.selectedStatuses.size > 0;
    },

    clearAllFilters() {
      this.filterFrom = ''; this.filterTo = '';
      this.selectedEvents = new Set();
      this.selectedBuckets = new Set();
      this.filterKey = '';
      this.filterDestination = '';
      this.selectedStatuses = new Set();
      this.page = 0;
      this._saveFilters();
      this.load();
    },

    async load() {
      this.loading = true;
      this.limit = Number(this.limit) || 50;
      sessionStorage.setItem('notif_page_size', this.limit);
      try {
        const params = new URLSearchParams();
        params.set('limit', '1000');
        params.set('offset', '0');
        const data = await api.adminGet('/notifications/events?' + params.toString());
        this._rawEntries = data.entries || [];
        this._rawTotal = data.total || 0;
      } catch (e) {
        console.error('Failed to load notification events:', e);
        this._rawEntries = [];
        this._rawTotal = 0;
      }
      this.loading = false;
    },

    init() {
      this.load();
      this.autoRefresh = setInterval(() => this.load(), 30000);
      this.$watch('selectedBuckets', () => this._saveFilters());
      this.$watch('selectedEvents', () => this._saveFilters());
      this.$watch('selectedStatuses', () => this._saveFilters());
      this.$watch('filterKey', () => this._saveFilters());
      this.$watch('filterDestination', () => this._saveFilters());
      this.$watch('filterFrom', () => this._saveFilters());
      this.$watch('filterTo', () => this._saveFilters());
    },
    destroy() { if (this.autoRefresh) clearInterval(this.autoRefresh); },

    firstPage() { this.page = 0; },
    prevPage() { if (this.page > 0) this.page--; },
    nextPage() { if (this.currentPage < this.totalPages) this.page++; },
    lastPage() { this.page = this.totalPages - 1; },

    selectEntry(entry) { this.selectedEntry = this.selectedEntry?.id === entry.id ? null : entry; },

    async clearAllEvents() {
      this.clearing = true;
      try {
        const resp = await api.adminDelete('/notifications/events', { confirm: 'CLEAR EVENTS' });
        if (!resp.ok) { const body = await resp.json(); throw new Error(body.message || `Error ${resp.status}`); }
        this.showClearModal = false; this.clearConfirmText = ''; this.page = 0; await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Clear failed: ' + e.message, type: 'error' });
      }
      this.clearing = false;
    },

    statusClass(status) {
      switch (status) {
        case 'delivered': return 'text-green-400';
        case 'failed': return 'text-red-400';
        case 'pending': return 'text-amber-400';
        default: return 'text-vault-muted';
      }
    },
    statusBadgeClass(status) {
      switch (status) {
        case 'delivered': return 'bg-green-500/20 text-green-400';
        case 'failed': return 'bg-red-500/20 text-red-400';
        case 'pending': return 'bg-yellow-500/20 text-yellow-400';
        default: return 'bg-gray-500/20 text-gray-400';
      }
    },
    formatTime(ts) { return ts ? new Date(ts).toLocaleString() : ''; },
  };
}

// ==================== BUCKET NOTIFICATION SETTINGS ====================
// Per-bucket notification configuration editor (lifecycle-style inline forms).
export function bucketNotificationEditor() {
  return {
    configs: [],
    loading: false,
    saving: false,
    error: '',
    showAddForm: false,
    editingIndex: -1,
    editForm: { id: '', arn: '', events: ['s3:ObjectCreated:*'], type: 'TopicConfiguration', prefix: '', suffix: '' },

    eventOptions: [
      's3:ObjectCreated:*',
      's3:ObjectCreated:Put',
      's3:ObjectCreated:Post',
      's3:ObjectCreated:Copy',
      's3:ObjectCreated:CompleteMultipartUpload',
      's3:ObjectRemoved:*',
      's3:ObjectRemoved:Delete',
      's3:ObjectRemoved:DeleteMarkerCreated',
    ],

    async loadNotifications(bucket) {
      this.loading = true;
      this.error = '';
      try {
        const resp = await api.s3GetBucketNotification(bucket);
        if (resp.ok) {
          const xml = await resp.text();
          this.configs = this.parseNotificationXml(xml);
        } else {
          this.configs = [];
        }
      } catch {
        this.configs = [];
      }
      this.loading = false;
    },

    parseNotificationXml(xml) {
      const configs = [];
      const types = [
        { tag: 'TopicConfiguration', arnTag: 'Topic', type: 'TopicConfiguration' },
        { tag: 'QueueConfiguration', arnTag: 'Queue', type: 'QueueConfiguration' },
        { tag: 'CloudFunctionConfiguration', arnTag: 'CloudFunction', type: 'CloudFunctionConfiguration' },
      ];
      const parser = new DOMParser();
      const doc = parser.parseFromString(xml, 'text/xml');
      for (const t of types) {
        const elems = doc.getElementsByTagName(t.tag);
        for (const el of elems) {
          const id = el.getElementsByTagName('Id')[0]?.textContent || '';
          const arn = el.getElementsByTagName(t.arnTag)[0]?.textContent || '';
          const events = [];
          for (const ev of el.getElementsByTagName('Event')) events.push(ev.textContent);
          let prefix = '', suffix = '';
          const rules = el.getElementsByTagName('FilterRule');
          for (const rule of rules) {
            const name = rule.getElementsByTagName('Name')[0]?.textContent || '';
            const value = rule.getElementsByTagName('Value')[0]?.textContent || '';
            if (name.toLowerCase() === 'prefix') prefix = value;
            if (name.toLowerCase() === 'suffix') suffix = value;
          }
          const enabledEl = el.getElementsByTagName('Enabled')[0]?.textContent;
          const enabled = enabledEl !== 'false';
          configs.push({ id, arn, events, type: t.type, prefix, suffix, enabled });
        }
      }
      return configs;
    },

    buildNotificationXml() {
      let xml = '<?xml version="1.0" encoding="UTF-8"?>\n<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">\n';
      for (const cfg of this.configs) {
        const arnTagMap = { TopicConfiguration: 'Topic', QueueConfiguration: 'Queue', CloudFunctionConfiguration: 'CloudFunction' };
        const wrapTag = cfg.type || 'TopicConfiguration';
        const arnTag = arnTagMap[wrapTag] || 'Topic';
        xml += `  <${wrapTag}>\n    <Id>${this.escapeXml(cfg.id)}</Id>\n    <${arnTag}>${this.escapeXml(cfg.arn)}</${arnTag}>\n`;
        for (const ev of cfg.events) xml += `    <Event>${this.escapeXml(ev)}</Event>\n`;
        if (cfg.prefix || cfg.suffix) {
          xml += '    <Filter><S3Key>\n';
          if (cfg.prefix) xml += `      <FilterRule><Name>prefix</Name><Value>${this.escapeXml(cfg.prefix)}</Value></FilterRule>\n`;
          if (cfg.suffix) xml += `      <FilterRule><Name>suffix</Name><Value>${this.escapeXml(cfg.suffix)}</Value></FilterRule>\n`;
          xml += '    </S3Key></Filter>\n';
        }
        if (!cfg.enabled) xml += '    <Enabled>false</Enabled>\n';
        xml += `  </${wrapTag}>\n`;
      }
      xml += '</NotificationConfiguration>';
      return xml;
    },

    escapeXml(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;'); },

    initNewWebhook() {
      this.editingIndex = -1;
      this.editForm = { id: '', arn: '', events: ['s3:ObjectCreated:*'], type: 'TopicConfiguration', prefix: '', suffix: '' };
      this.showAddForm = true;
    },

    startEditWebhook(idx) {
      const cfg = this.configs[idx];
      this.editingIndex = idx;
      this.editForm = { id: cfg.id, arn: cfg.arn, events: [...cfg.events], type: cfg.type, prefix: cfg.prefix, suffix: cfg.suffix };
      this.showAddForm = false;
    },

    cancelEdit() { this.editingIndex = -1; this.showAddForm = false; },

    toggleEvent(ev) {
      const idx = this.editForm.events.indexOf(ev);
      if (idx >= 0) this.editForm.events.splice(idx, 1); else this.editForm.events.push(ev);
    },

    async toggleWebhookEnabled(idx, bucket) {
      this.configs[idx].enabled = !this.configs[idx].enabled;
      await this.saveNotifications(bucket);
    },

    async saveEditWebhook(bucket) {
      if (!this.editForm.arn) { this.error = 'Destination URL is required'; return; }
      if (this.editForm.events.length === 0) { this.error = 'At least one event is required'; return; }
      this.error = '';
      const cfg = {
        id: this.editForm.id || crypto.randomUUID(), arn: this.editForm.arn,
        events: [...this.editForm.events], type: this.editForm.type,
        prefix: this.editForm.prefix, suffix: this.editForm.suffix, enabled: true,
      };
      if (this.editingIndex >= 0) { cfg.enabled = this.configs[this.editingIndex].enabled; this.configs[this.editingIndex] = cfg; }
      else this.configs.push(cfg);
      this.editingIndex = -1; this.showAddForm = false;
      await this.saveNotifications(bucket);
    },

    async removeWebhook(idx, bucket) { this.configs.splice(idx, 1); await this.saveNotifications(bucket); },
    async deleteAllWebhooks(bucket) { this.configs = []; await this.saveNotifications(bucket); },

    async saveNotifications(bucket) {
      this.saving = true; this.error = '';
      try {
        const xml = this.buildNotificationXml();
        const resp = await api.s3PutBucketNotification(bucket, xml);
        if (!resp.ok) { const text = await resp.text(); this.error = text || 'Failed to save notification configuration'; }
      } catch (e) { this.error = e.message || 'Failed to save'; }
      this.saving = false;
    },

    async testWebhook(url) {
      if (!url) return;
      try {
        const data = await api.adminPost('/notifications/test-webhook', { url });
        if (data.success) alert('Webhook test successful (HTTP ' + data.status + ')');
        else alert('Webhook test failed: ' + (data.error || 'HTTP ' + data.status));
      } catch (e) { alert('Webhook test failed: ' + e.message); }
    },

    shortEvent(ev) { return ev.replace('s3:', ''); },
  };
}
