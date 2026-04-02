import { api } from '../api.js';

// ==================== NOTIFICATIONS VIEW ====================
// Global notification event log viewer (admin only).
export function notificationsView() {
  return {
    events: [],
    total: 0,
    loading: true,
    page: 0,
    limit: 50,
    autoRefresh: null,
    selectedEvent: null,

    // Filters
    filterBucket: '',
    filterEventName: '',
    filterStatus: '',

    async load() {
      this.loading = true;
      await this.fetchEvents();
      this.loading = false;
    },

    async fetchEvents() {
      try {
        const params = new URLSearchParams();
        if (this.filterBucket) params.set('bucket', this.filterBucket);
        if (this.filterEventName) params.set('event_name', this.filterEventName);
        if (this.filterStatus) params.set('delivery_status', this.filterStatus);
        params.set('offset', String(this.page * this.limit));
        params.set('limit', String(this.limit));

        const data = await api.adminGet('/notifications/events?' + params.toString());
        this.events = data.entries || [];
        this.total = data.total || 0;
      } catch (e) {
        console.error('Failed to load notification events:', e);
        this.events = [];
        this.total = 0;
      }
    },

    async applyFilter() {
      this.page = 0;
      await this.fetchEvents();
    },

    async clearFilters() {
      this.filterBucket = '';
      this.filterEventName = '';
      this.filterStatus = '';
      this.page = 0;
      await this.fetchEvents();
    },

    async prevPage() {
      if (this.page > 0) {
        this.page--;
        await this.fetchEvents();
      }
    },

    async nextPage() {
      if ((this.page + 1) * this.limit < this.total) {
        this.page++;
        await this.fetchEvents();
      }
    },

    get totalPages() {
      return Math.max(1, Math.ceil(this.total / this.limit));
    },

    toggleAutoRefresh() {
      if (this.autoRefresh) {
        clearInterval(this.autoRefresh);
        this.autoRefresh = null;
      } else {
        this.autoRefresh = setInterval(() => this.fetchEvents(), 5000);
      }
    },

    selectEvent(ev) {
      this.selectedEvent = ev;
    },

    closeDetail() {
      this.selectedEvent = null;
    },

    statusBadgeClass(status) {
      switch (status) {
        case 'delivered': return 'bg-green-500/20 text-green-400';
        case 'failed': return 'bg-red-500/20 text-red-400';
        case 'pending': return 'bg-yellow-500/20 text-yellow-400';
        default: return 'bg-gray-500/20 text-gray-400';
      }
    },

    formatTime(ts) {
      if (!ts) return '\u2014';
      const d = new Date(ts);
      return d.toLocaleString();
    },

    truncate(s, max = 40) {
      if (!s) return '';
      return s.length > max ? s.slice(0, max) + '\u2026' : s;
    },

    destroy() {
      if (this.autoRefresh) {
        clearInterval(this.autoRefresh);
        this.autoRefresh = null;
      }
    },
  };
}

// ==================== BUCKET NOTIFICATION SETTINGS ====================
// Per-bucket notification configuration editor (used in bucket-settings view).
export function bucketNotificationEditor() {
  return {
    configs: [],
    loading: false,
    saving: false,
    error: '',
    showAddModal: false,
    editIndex: -1,

    // New/edit form
    form: { id: '', arn: '', events: [], type: 'TopicConfiguration', prefix: '', suffix: '' },

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
          for (const ev of el.getElementsByTagName('Event')) {
            events.push(ev.textContent);
          }
          let prefix = '', suffix = '';
          const rules = el.getElementsByTagName('FilterRule');
          for (const rule of rules) {
            const name = rule.getElementsByTagName('Name')[0]?.textContent || '';
            const value = rule.getElementsByTagName('Value')[0]?.textContent || '';
            if (name.toLowerCase() === 'prefix') prefix = value;
            if (name.toLowerCase() === 'suffix') suffix = value;
          }
          configs.push({ id, arn, events, type: t.type, prefix, suffix });
        }
      }
      return configs;
    },

    buildNotificationXml() {
      let xml = '<?xml version="1.0" encoding="UTF-8"?>\n';
      xml += '<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">\n';

      for (const cfg of this.configs) {
        const arnTagMap = {
          TopicConfiguration: 'Topic',
          QueueConfiguration: 'Queue',
          CloudFunctionConfiguration: 'CloudFunction',
        };
        const wrapTag = cfg.type || 'TopicConfiguration';
        const arnTag = arnTagMap[wrapTag] || 'Topic';

        xml += `  <${wrapTag}>\n`;
        xml += `    <Id>${this.escapeXml(cfg.id)}</Id>\n`;
        xml += `    <${arnTag}>${this.escapeXml(cfg.arn)}</${arnTag}>\n`;
        for (const ev of cfg.events) {
          xml += `    <Event>${this.escapeXml(ev)}</Event>\n`;
        }
        if (cfg.prefix || cfg.suffix) {
          xml += '    <Filter><S3Key>\n';
          if (cfg.prefix) {
            xml += `      <FilterRule><Name>prefix</Name><Value>${this.escapeXml(cfg.prefix)}</Value></FilterRule>\n`;
          }
          if (cfg.suffix) {
            xml += `      <FilterRule><Name>suffix</Name><Value>${this.escapeXml(cfg.suffix)}</Value></FilterRule>\n`;
          }
          xml += '    </S3Key></Filter>\n';
        }
        xml += `  </${wrapTag}>\n`;
      }

      xml += '</NotificationConfiguration>';
      return xml;
    },

    escapeXml(s) {
      return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
    },

    openAdd() {
      this.editIndex = -1;
      this.form = { id: '', arn: '', events: ['s3:ObjectCreated:*'], type: 'TopicConfiguration', prefix: '', suffix: '' };
      this.showAddModal = true;
    },

    openEdit(idx) {
      this.editIndex = idx;
      const cfg = this.configs[idx];
      this.form = { id: cfg.id, arn: cfg.arn, events: [...cfg.events], type: cfg.type, prefix: cfg.prefix, suffix: cfg.suffix };
      this.showAddModal = true;
    },

    closeModal() {
      this.showAddModal = false;
      this.editIndex = -1;
    },

    toggleEvent(ev) {
      const idx = this.form.events.indexOf(ev);
      if (idx >= 0) {
        this.form.events.splice(idx, 1);
      } else {
        this.form.events.push(ev);
      }
    },

    saveForm(bucket) {
      if (!this.form.arn) { this.error = 'Destination URL is required'; return; }
      if (this.form.events.length === 0) { this.error = 'At least one event type is required'; return; }
      this.error = '';

      const cfg = {
        id: this.form.id || crypto.randomUUID(),
        arn: this.form.arn,
        events: [...this.form.events],
        type: this.form.type,
        prefix: this.form.prefix,
        suffix: this.form.suffix,
      };

      if (this.editIndex >= 0) {
        this.configs[this.editIndex] = cfg;
      } else {
        this.configs.push(cfg);
      }

      this.closeModal();
      this.saveNotifications(bucket);
    },

    removeConfig(idx, bucket) {
      this.configs.splice(idx, 1);
      this.saveNotifications(bucket);
    },

    async saveNotifications(bucket) {
      this.saving = true;
      this.error = '';
      try {
        const xml = this.buildNotificationXml();
        const resp = await api.s3PutBucketNotification(bucket, xml);
        if (!resp.ok) {
          const text = await resp.text();
          this.error = text || 'Failed to save notification configuration';
        }
      } catch (e) {
        this.error = e.message || 'Failed to save';
      }
      this.saving = false;
    },

    async testWebhook(url) {
      if (!url) return;
      try {
        const data = await api.adminPost('/notifications/test-webhook', { url });
        if (data.success) {
          this.error = '';
          alert('Webhook test successful (HTTP ' + data.status + ')');
        } else {
          alert('Webhook test failed: ' + (data.error || 'HTTP ' + data.status));
        }
      } catch (e) {
        alert('Webhook test failed: ' + e.message);
      }
    },
  };
}
