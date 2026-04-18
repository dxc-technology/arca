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

// ==================== CONNECTOR TYPES ====================
// Connector type definitions for notification destinations.
// All 13 connectors are implemented and selectable.
const CONNECTOR_TYPES = [
  { id: 'webhook', name: 'Webhook', category: 'Functions', color: '#8b5cf6',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M12 21a9.004 9.004 0 0 0 8.716-6.747M12 21a9.004 9.004 0 0 1-8.716-6.747M12 21c2.485 0 4.5-4.03 4.5-9S14.485 3 12 3m0 18c-2.485 0-4.5-4.03-4.5-9S9.515 3 12 3m0 0a8.997 8.997 0 0 1 7.843 4.582M12 3a8.997 8.997 0 0 0-7.843 4.582m15.686 0A11.953 11.953 0 0 1 12 10.5c-2.998 0-5.74-1.1-7.843-2.918m15.686 0A8.959 8.959 0 0 1 21 12c0 .778-.099 1.533-.284 2.253m0 0A17.919 17.919 0 0 1 12 16.5a17.92 17.92 0 0 1-8.716-2.247m0 0A9 9 0 0 1 3 12c0-1.47.353-2.856.978-4.082"/></svg>` },
  { id: 'kafka', name: 'Kafka', category: 'Queue', color: '#e0e0e0',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M3.75 6.75h16.5M3.75 12h16.5m-16.5 5.25H12"/></svg>` },
  { id: 'amqp', name: 'AMQP', category: 'Queue', color: '#ff6600',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M7.5 21 3 16.5m0 0L7.5 12M3 16.5h13.5m0-13.5L21 7.5m0 0L16.5 12M21 7.5H7.5"/></svg>` },
  { id: 'redis', name: 'Redis', category: 'Queue', color: '#dc382d',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M20.25 6.375c0 2.278-3.694 4.125-8.25 4.125S3.75 8.653 3.75 6.375m16.5 0c0-2.278-3.694-4.125-8.25-4.125S3.75 4.097 3.75 6.375m16.5 0v11.25c0 2.278-3.694 4.125-8.25 4.125s-8.25-1.847-8.25-4.125V6.375m16.5 0v3.75m-16.5-3.75v3.75m16.5 0v3.75C20.25 16.153 16.556 18 12 18s-8.25-1.847-8.25-4.125v-3.75m16.5 0c0 2.278-3.694 4.125-8.25 4.125s-8.25-1.847-8.25-4.125"/></svg>` },
  { id: 'nats', name: 'NATS', category: 'Queue', color: '#27aae1',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="m3.75 13.5 10.5-11.25L12 10.5h8.25L9.75 21.75 12 13.5H3.75Z"/></svg>` },
  { id: 'mqtt', name: 'MQTT', category: 'Queue', color: '#e5347e',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M9.348 14.652a3.75 3.75 0 0 1 0-5.304m5.304 0a3.75 3.75 0 0 1 0 5.304m-7.425 2.121a6.75 6.75 0 0 1 0-9.546m9.546 0a6.75 6.75 0 0 1 0 9.546M5.106 18.894c-3.808-3.807-3.808-9.98 0-13.788m13.788 0c3.808 3.807 3.808 9.98 0 13.788M12 12h.008v.008H12V12Zm.375 0a.375.375 0 1 1-.75 0 .375.375 0 0 1 .75 0Z"/></svg>` },
  { id: 'postgresql', name: 'PostgreSQL', category: 'Database', color: '#336791',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><circle cx="12" cy="12" r="9" /><path d="M8 12h8M8 8h8M8 16h5"/></svg>` },
  { id: 'mysql', name: 'MySQL', category: 'Database', color: '#00758f',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M20.25 6.375c0 2.278-3.694 4.125-8.25 4.125S3.75 8.653 3.75 6.375m16.5 0c0-2.278-3.694-4.125-8.25-4.125S3.75 4.097 3.75 6.375m16.5 0v11.25c0 2.278-3.694 4.125-8.25 4.125s-8.25-1.847-8.25-4.125V6.375"/></svg>` },
  { id: 'mongodb', name: 'MongoDB', category: 'Database', color: '#47a248',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M15 11.25c0 3.314-1.343 6-3 6s-3-2.686-3-6c0-3.314 1.343-6 3-6s3 2.686 3 6Z"/><path stroke-linecap="round" d="M12 17.25v3"/></svg>` },
  { id: 'elasticsearch', name: 'Elasticsearch', category: 'Database', color: '#fed10a',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="m21 21-5.197-5.197m0 0A7.5 7.5 0 1 0 5.196 5.196a7.5 7.5 0 0 0 10.607 10.607Z"/></svg>` },
  { id: 'grpc', name: 'gRPC', category: 'Functions', color: '#244c5a',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M17.25 6.75 22.5 12l-5.25 5.25m-10.5 0L1.5 12l5.25-5.25m7.5-3-4.5 16.5"/></svg>` },
  { id: 'smtp', name: 'SMTP', category: 'Protocol', color: '#0ea5e9',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M21.75 6.75v10.5a2.25 2.25 0 0 1-2.25 2.25h-15a2.25 2.25 0 0 1-2.25-2.25V6.75m19.5 0A2.25 2.25 0 0 0 19.5 4.5h-15a2.25 2.25 0 0 0-2.25 2.25m19.5 0v.243a2.25 2.25 0 0 1-1.07 1.916l-7.5 4.615a2.25 2.25 0 0 1-2.36 0L3.32 8.91a2.25 2.25 0 0 1-1.07-1.916V6.75"/></svg>` },
  { id: 'syslog', name: 'Syslog', category: 'Protocol', color: '#059669',
    icon: `<svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M12 7.5h1.5m-1.5 3h1.5m-7.5 3h7.5m-7.5 3h7.5m3-9h3.375c.621 0 1.125.504 1.125 1.125V18a2.25 2.25 0 0 1-2.25 2.25M16.5 7.5V18a2.25 2.25 0 0 0 2.25 2.25M16.5 7.5V4.875c0-.621-.504-1.125-1.125-1.125H4.125C3.504 3.75 3 4.254 3 4.875V18a2.25 2.25 0 0 0 2.25 2.25h13.5M6 7.5h3v3H6v-3Z"/></svg>` },
];

// Group connectors by category for the modal grid.
function connectorsByCategory() {
  const groups = {};
  for (const c of CONNECTOR_TYPES) {
    if (!groups[c.category]) groups[c.category] = [];
    groups[c.category].push(c);
  }
  return groups;
}

// ==================== BUCKET NOTIFICATION SETTINGS ====================
// Per-bucket notification configuration editor with modal dialog.
export function bucketNotificationEditor() {
  return {
    configs: [],
    loading: false,
    saving: false,
    error: '',
    showModal: false,
    modalMode: 'add',   // 'add' or 'edit'
    editingIndex: -1,
    editForm: {
      id: '', arn: '', events: ['s3:ObjectCreated:*'], type: 'TopicConfiguration',
      prefix: '', suffix: '', connector_type: 'webhook', auth_token: '',
      channel: '', password: '', subject: '', token: '', user: '', topic: '',
      table: '', schema: '', database: '', collection: '',
      smtp_to: '', smtp_from: '', smtp_starttls: '',
      grpc_ca_certificate: '', grpc_domain_name: '', grpc_insecure: '',
    },

    connectorTypes: CONNECTOR_TYPES,
    connectorGroups: connectorsByCategory(),

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

          // Arca extensions: connector type and properties
          const connectorTypeEl = el.getElementsByTagName('ConnectorType')[0]?.textContent;
          const connector_type = connectorTypeEl || 'webhook';
          const properties = {};
          const propEls = el.getElementsByTagName('Property');
          for (const prop of propEls) {
            const pName = prop.getElementsByTagName('Name')[0]?.textContent || '';
            const pValue = prop.getElementsByTagName('Value')[0]?.textContent || '';
            if (pName) properties[pName] = pValue;
          }

          configs.push({
            id, arn, events, type: t.type, prefix, suffix, enabled,
            connector_type, properties,
          });
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
        // Arca extensions
        if (cfg.connector_type && cfg.connector_type !== 'webhook') {
          xml += `    <ConnectorType>${this.escapeXml(cfg.connector_type)}</ConnectorType>\n`;
        }
        const props = cfg.properties || {};
        for (const [k, v] of Object.entries(props)) {
          if (v) xml += `    <Property><Name>${this.escapeXml(k)}</Name><Value>${this.escapeXml(v)}</Value></Property>\n`;
        }
        xml += `  </${wrapTag}>\n`;
      }
      xml += '</NotificationConfiguration>';
      return xml;
    },

    escapeXml(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;'); },

    // Modal management
    openAddModal() {
      this.editingIndex = -1;
      this.modalMode = 'add';
      this.editForm = {
        id: '', arn: '', events: ['s3:ObjectCreated:*'], type: 'TopicConfiguration',
        prefix: '', suffix: '', connector_type: 'webhook', auth_token: '',
        channel: '', password: '', subject: '', token: '', user: '', topic: '',
        table: '', schema: '', database: '', collection: '',
        exchange: '', routing_key: '', durable: '', index: '',
        facility: '', severity: '', app_name: '',
        sasl_username: '', sasl_password: '', security_protocol: '',
        smtp_to: '', smtp_from: '', smtp_starttls: '',
        grpc_ca_certificate: '', grpc_domain_name: '', grpc_insecure: '',
      };
      this.error = '';
      this.showModal = true;
    },

    openEditModal(idx) {
      const cfg = this.configs[idx];
      this.editingIndex = idx;
      this.modalMode = 'edit';
      const props = cfg.properties || {};
      this.editForm = {
        id: cfg.id, arn: cfg.arn, events: [...cfg.events], type: cfg.type,
        prefix: cfg.prefix, suffix: cfg.suffix,
        connector_type: cfg.connector_type || 'webhook',
        auth_token: props.auth_token || '',
        channel: props.channel || '',
        password: props.password || '',
        subject: props.subject || '',
        token: props.token || '',
        user: props.user || props.username || '',
        topic: props.topic || '',
        table: props.table || '',
        schema: props.schema || '',
        database: props.database || '',
        collection: props.collection || '',
        exchange: props.exchange || '',
        routing_key: props.routing_key || '',
        durable: props.durable || '',
        index: props.index || '',
        facility: props.facility || '',
        severity: props.severity || '',
        app_name: props.app_name || '',
        sasl_username: props.sasl_username || '',
        sasl_password: props.sasl_password || '',
        security_protocol: props.security_protocol || '',
        smtp_to: props.to || '',
        smtp_from: props.from || '',
        smtp_starttls: props.starttls || '',
        grpc_ca_certificate: props.ca_certificate || '',
        grpc_domain_name: props.domain_name || '',
        grpc_insecure: props.insecure || '',
      };
      this.error = '';
      this.showModal = true;
    },

    closeModal() {
      this.showModal = false;
      this.editingIndex = -1;
      this.error = '';
    },

    selectConnectorType(id) {
      const ct = CONNECTOR_TYPES.find(c => c.id === id);
      if (!ct) return;
      this.editForm.connector_type = id;
      // Set S3 destination type to match the connector's category
      if (ct.category === 'Queue') this.editForm.type = 'QueueConfiguration';
      else if (ct.category === 'Functions') this.editForm.type = 'TopicConfiguration';
      else if (ct.category === 'Database') this.editForm.type = 'TopicConfiguration';
      else if (ct.category === 'Protocol') this.editForm.type = 'CloudFunctionConfiguration';
    },

    toggleEvent(ev) {
      const idx = this.editForm.events.indexOf(ev);
      if (idx >= 0) this.editForm.events.splice(idx, 1); else this.editForm.events.push(ev);
    },

    async toggleEnabled(idx, bucket) {
      this.configs[idx].enabled = !this.configs[idx].enabled;
      await this.saveNotifications(bucket);
    },

    async saveFromModal(bucket) {
      if (!this.editForm.arn) { this.error = 'Destination URL is required'; return; }
      if (this.editForm.events.length === 0) { this.error = 'At least one event is required'; return; }
      this.error = '';

      const properties = {};
      if (this.editForm.auth_token) properties.auth_token = this.editForm.auth_token;
      if (this.editForm.channel) properties.channel = this.editForm.channel;
      if (this.editForm.password) properties.password = this.editForm.password;
      if (this.editForm.subject) properties.subject = this.editForm.subject;
      if (this.editForm.token) properties.token = this.editForm.token;
      if (this.editForm.user) {
        // SMTP uses `username` for PLAIN auth; other connectors use `user`.
        if (this.editForm.connector_type === 'smtp') properties.username = this.editForm.user;
        else properties.user = this.editForm.user;
      }
      if (this.editForm.topic) properties.topic = this.editForm.topic;
      if (this.editForm.table) properties.table = this.editForm.table;
      if (this.editForm.schema) properties.schema = this.editForm.schema;
      if (this.editForm.database) properties.database = this.editForm.database;
      if (this.editForm.collection) properties.collection = this.editForm.collection;
      if (this.editForm.exchange) properties.exchange = this.editForm.exchange;
      if (this.editForm.routing_key) properties.routing_key = this.editForm.routing_key;
      if (this.editForm.durable) properties.durable = this.editForm.durable;
      if (this.editForm.index) properties.index = this.editForm.index;
      if (this.editForm.facility) properties.facility = this.editForm.facility;
      if (this.editForm.severity) properties.severity = this.editForm.severity;
      if (this.editForm.app_name) properties.app_name = this.editForm.app_name;
      if (this.editForm.sasl_username) properties.sasl_username = this.editForm.sasl_username;
      if (this.editForm.sasl_password) properties.sasl_password = this.editForm.sasl_password;
      if (this.editForm.security_protocol) properties.security_protocol = this.editForm.security_protocol;
      if (this.editForm.smtp_to) properties.to = this.editForm.smtp_to;
      if (this.editForm.smtp_from) properties.from = this.editForm.smtp_from;
      if (this.editForm.smtp_starttls) properties.starttls = this.editForm.smtp_starttls;
      if (this.editForm.grpc_ca_certificate) properties.ca_certificate = this.editForm.grpc_ca_certificate;
      if (this.editForm.grpc_domain_name) properties.domain_name = this.editForm.grpc_domain_name;
      if (this.editForm.grpc_insecure) properties.insecure = this.editForm.grpc_insecure;

      const cfg = {
        id: this.editForm.id || crypto.randomUUID(),
        arn: this.editForm.arn,
        events: [...this.editForm.events],
        type: this.editForm.type,
        prefix: this.editForm.prefix,
        suffix: this.editForm.suffix,
        enabled: true,
        connector_type: this.editForm.connector_type,
        properties,
      };

      if (this.editingIndex >= 0) {
        cfg.enabled = this.configs[this.editingIndex].enabled;
        this.configs[this.editingIndex] = cfg;
      } else {
        this.configs.push(cfg);
      }

      this.closeModal();
      await this.saveNotifications(bucket);
    },

    async removeNotification(idx, bucket) {
      this.configs.splice(idx, 1);
      await this.saveNotifications(bucket);
    },

    async deleteAllNotifications(bucket) {
      this.configs = [];
      await this.saveNotifications(bucket);
    },

    async saveNotifications(bucket) {
      this.saving = true; this.error = '';
      try {
        const xml = this.buildNotificationXml();
        const resp = await api.s3PutBucketNotification(bucket, xml);
        if (!resp.ok) { const text = await resp.text(); this.error = text || 'Failed to save notification configuration'; }
      } catch (e) { this.error = e.message || 'Failed to save'; }
      this.saving = false;
    },

    async testConnector(url, connectorType, properties) {
      if (!url) return;
      try {
        const data = await api.adminPost('/notifications/test-connector', {
          connector_type: connectorType || 'webhook',
          url,
          properties: properties || {},
        });
        if (data.success) alert('Test successful (' + data.status + ')');
        else alert('Test failed: ' + (data.error || data.status));
      } catch (e) { alert('Test failed: ' + e.message); }
    },

    connectorIcon(typeId) {
      const ct = CONNECTOR_TYPES.find(c => c.id === typeId);
      return ct ? ct.icon : '';
    },

    connectorName(typeId) {
      const ct = CONNECTOR_TYPES.find(c => c.id === typeId);
      return ct ? ct.name : typeId;
    },

    shortEvent(ev) { return ev.replace('s3:', ''); },
  };
}
