import { api } from '../api.js';

// ==================== REPLICATION — SHARED ====================

const EVENT_TYPES = [
  { id: 'put', label: 'Put', chip: 'bg-cyan-500/15 text-cyan-400 border-cyan-500/30' },
  { id: 'delete_marker', label: 'Delete marker', chip: 'bg-amber-500/15 text-amber-400 border-amber-500/30' },
  { id: 'tag', label: 'Tag', chip: 'bg-slate-500/15 text-slate-300 border-slate-500/30' },
];

const STATUS_META = {
  pending:    { badge: 'bg-yellow-500/20 text-yellow-400', dot: 'bg-yellow-400', pulse: false },
  in_flight:  { badge: 'bg-cyan-500/20 text-cyan-300',     dot: 'bg-cyan-400',   pulse: true  },
  completed:  { badge: 'bg-green-500/20 text-green-400',   dot: 'bg-green-400',  pulse: false },
  failed:     { badge: 'bg-red-500/20 text-red-400',       dot: 'bg-red-400',    pulse: false },
};

function eventTypeMeta(id) {
  return EVENT_TYPES.find(e => e.id === id) || { id, label: id, chip: 'bg-slate-500/15 text-slate-300 border-slate-500/30' };
}

// ==================== GLOBAL JOURNAL VIEW ====================
// Mirrors the audit + notification-events pattern: inline header filters,
// side detail panel, auto-refresh, pagination.
export function replicationView() {
  return {
    _rawEntries: [],
    _rawTotal: 0,

    loading: true,
    page: 0,
    limit: Number(sessionStorage.getItem('repl_page_size')) || 50,
    autoRefresh: null,
    selectedEntry: null,
    retrying: {},
    showClearModal: false,
    clearConfirmText: '',
    clearing: false,

    // Column filters (persisted in sessionStorage)
    filterFrom: sessionStorage.getItem('repl_f_from') || '',
    filterTo: sessionStorage.getItem('repl_f_to') || '',
    timeOpen: false,
    selectedBuckets: new Set(JSON.parse(sessionStorage.getItem('repl_f_buckets') || '[]')),
    bucketOpen: false,
    bucketSearch: '',
    filterKey: sessionStorage.getItem('repl_f_key') || '',
    keyEditing: false,
    filterRule: sessionStorage.getItem('repl_f_rule') || '',
    ruleEditing: false,
    selectedTypes: new Set(JSON.parse(sessionStorage.getItem('repl_f_types') || '[]')),
    typeOpen: false,
    selectedStatuses: new Set(JSON.parse(sessionStorage.getItem('repl_f_statuses') || '[]')),
    statusOpen: false,
    filterDest: sessionStorage.getItem('repl_f_dest') || '',
    destEditing: false,

    eventTypeOptions: EVENT_TYPES,

    get bucketValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const b = e.bucket || '(none)';
        counts[b] = (counts[b] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },
    get typeValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const t = e.event_type || '(none)';
        counts[t] = (counts[t] || 0) + 1;
      }
      return Object.entries(counts).sort((a, b) => a[0].localeCompare(b[0])).map(([name, count]) => ({ name, count }));
    },
    get statusValues() {
      const counts = {};
      for (const e of this._rawEntries) {
        const s = e.status || '(none)';
        counts[s] = (counts[s] || 0) + 1;
      }
      const order = { pending: 0, in_flight: 1, completed: 2, failed: 3 };
      return Object.entries(counts)
        .sort((a, b) => (order[a[0]] ?? 99) - (order[b[0]] ?? 99))
        .map(([name, count]) => ({ name, count }));
    },

    toggleSet(set, val) { if (set.has(val)) set.delete(val); else set.add(val); },

    _saveFilters() {
      sessionStorage.setItem('repl_f_from', this.filterFrom);
      sessionStorage.setItem('repl_f_to', this.filterTo);
      sessionStorage.setItem('repl_f_buckets', JSON.stringify([...this.selectedBuckets]));
      sessionStorage.setItem('repl_f_key', this.filterKey);
      sessionStorage.setItem('repl_f_rule', this.filterRule);
      sessionStorage.setItem('repl_f_types', JSON.stringify([...this.selectedTypes]));
      sessionStorage.setItem('repl_f_statuses', JSON.stringify([...this.selectedStatuses]));
      sessionStorage.setItem('repl_f_dest', this.filterDest);
    },

    get allFiltered() {
      let entries = this._rawEntries;
      if (this.filterFrom) {
        const from = new Date(this.filterFrom).getTime();
        entries = entries.filter(e => new Date(e.created_at).getTime() >= from);
      }
      if (this.filterTo) {
        const to = new Date(this.filterTo).getTime();
        entries = entries.filter(e => new Date(e.created_at).getTime() <= to);
      }
      if (this.selectedBuckets.size > 0) {
        entries = entries.filter(e => this.selectedBuckets.has(e.bucket || '(none)'));
      }
      if (this.filterKey) {
        const q = this.filterKey.toLowerCase();
        entries = entries.filter(e => (e.key || '').toLowerCase().includes(q));
      }
      if (this.filterRule) {
        const q = this.filterRule.toLowerCase();
        entries = entries.filter(e => (e.rule_id || '').toLowerCase().includes(q));
      }
      if (this.selectedTypes.size > 0) {
        entries = entries.filter(e => this.selectedTypes.has(e.event_type));
      }
      if (this.selectedStatuses.size > 0) {
        entries = entries.filter(e => this.selectedStatuses.has(e.status));
      }
      if (this.filterDest) {
        const q = this.filterDest.toLowerCase();
        entries = entries.filter(e =>
          (e.destination_endpoint || '').toLowerCase().includes(q) ||
          (e.destination_bucket || '').toLowerCase().includes(q)
        );
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
      return this.filterFrom || this.filterTo ||
        this.selectedBuckets.size > 0 || this.filterKey || this.filterRule ||
        this.selectedTypes.size > 0 || this.selectedStatuses.size > 0 || this.filterDest;
    },

    clearAllFilters() {
      this.filterFrom = ''; this.filterTo = '';
      this.selectedBuckets = new Set();
      this.filterKey = ''; this.filterRule = '';
      this.selectedTypes = new Set();
      this.selectedStatuses = new Set();
      this.filterDest = '';
      this.page = 0;
      this._saveFilters();
      this.load();
    },

    timeLabel() {
      const fmt = (s) => new Date(s).toLocaleDateString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
      const f = this.filterFrom ? fmt(this.filterFrom) : '';
      const t = this.filterTo ? fmt(this.filterTo) : '';
      if (f && t) return f + ' \u2014 ' + t;
      if (f) return 'From ' + f;
      if (t) return 'Until ' + t;
      return '';
    },

    async load() {
      this.loading = true;
      this.limit = Number(this.limit) || 50;
      sessionStorage.setItem('repl_page_size', this.limit);
      try {
        const params = new URLSearchParams();
        params.set('limit', '1000');
        params.set('offset', '0');
        const data = await api.adminGet('/replication/journal?' + params.toString());
        this._rawEntries = data.entries || [];
        this._rawTotal = data.total || 0;
      } catch (e) {
        console.error('Failed to load replication journal:', e);
        this._rawEntries = [];
        this._rawTotal = 0;
      }
      this.loading = false;
    },

    init() {
      this.load();
      this.autoRefresh = setInterval(() => this.load(), 30000);
      this.$watch('selectedBuckets', () => this._saveFilters());
      this.$watch('selectedTypes', () => this._saveFilters());
      this.$watch('selectedStatuses', () => this._saveFilters());
      this.$watch('filterKey', () => this._saveFilters());
      this.$watch('filterRule', () => this._saveFilters());
      this.$watch('filterDest', () => this._saveFilters());
      this.$watch('filterFrom', () => this._saveFilters());
      this.$watch('filterTo', () => this._saveFilters());
    },
    destroy() { if (this.autoRefresh) clearInterval(this.autoRefresh); },

    firstPage() { this.page = 0; },
    prevPage() { if (this.page > 0) this.page--; },
    nextPage() { if (this.currentPage < this.totalPages) this.page++; },
    lastPage() { this.page = this.totalPages - 1; },

    selectEntry(entry) { this.selectedEntry = this.selectedEntry?.id === entry.id ? null : entry; },

    async retryEntry(id) {
      this.retrying = { ...this.retrying, [id]: true };
      try {
        const resp = await api.adminPost('/replication/retry/' + encodeURIComponent(id));
        if (!resp.ok) throw new Error('retry failed: ' + resp.status);
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Retry failed: ' + e.message, type: 'error' });
      }
      this.retrying = { ...this.retrying, [id]: false };
    },

    async clearAllJournal() {
      this.clearing = true;
      try {
        const resp = await api.adminDelete('/replication/journal', { confirm: 'CLEAR JOURNAL' });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || `Error ${resp.status}`);
        }
        this.showClearModal = false;
        this.clearConfirmText = '';
        this.page = 0;
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Clear failed: ' + e.message, type: 'error' });
      }
      this.clearing = false;
    },

    statusBadgeClass(status) { return STATUS_META[status]?.badge || 'bg-gray-500/20 text-gray-400'; },
    statusDotClass(status) { return STATUS_META[status]?.dot || 'bg-gray-400'; },
    statusShouldPulse(status) { return STATUS_META[status]?.pulse === true; },
    eventTypeLabel(id) { return eventTypeMeta(id).label; },
    eventTypeChip(id) { return eventTypeMeta(id).chip; },
    formatTime(ts) { return ts ? new Date(ts).toLocaleString() : ''; },

    shortDestination(entry) {
      try {
        const u = new URL(entry.destination_endpoint);
        return `${entry.destination_bucket}@${u.host}`;
      } catch {
        return `${entry.destination_bucket || '?'}@${entry.destination_endpoint || '?'}`;
      }
    },
  };
}

// ==================== PER-BUCKET REPLICATION CARD ====================
export function bucketReplicationEditor() {
  return {
    rules: [],
    // Reactively mirrors the parent bucketSettingsView.versioningStatus via x-effect
    // in index.html. `null` = parent still loading (don't show the banner yet);
    // `true` = versioning Enabled; `false` = versioning Disabled or Suspended.
    versioningEnabled: null,
    // Named distinctly from the parent's `loading` so the x-effect on this card can
    // reference parent.loading unambiguously via scope walk-up.
    rulesLoading: false,
    saving: false,
    error: '',
    showModal: false,
    modalMode: 'add',
    editingIndex: -1,

    editForm: {
      id: '',
      status: 'Enabled',
      priority: 1,
      prefix: '',
      // Array of { key, value } — at serialize time any empty-key entries are dropped.
      tags: [],
      destBucket: '',
      destEndpoint: '',
      destRegion: 'us-east-1',
      credentialRef: '',
      newCredentialMode: false,
      newAccessKey: '',
      newSecretKey: '',
      deleteMarkers: true,
    },

    // Connection-test state (reset on every modal open).
    testing: false,
    testResult: null,  // null | { success: bool, message: string }

    get knownCredentialRefs() {
      const set = new Set();
      for (const r of this.rules) if (r.credentialRef) set.add(r.credentialRef);
      return [...set].sort();
    },

    async load(bucket) {
      this.rulesLoading = true;
      this.error = '';
      try {
        // versioningEnabled is driven reactively from the parent bucketSettingsView's
        // versioningStatus via x-effect on the card (see index.html). No local fetch
        // here, otherwise the flag would go stale if the user toggled versioning in
        // the Versioning card without refreshing the page.

        const resp = await api.s3GetBucketReplication(bucket);
        if (resp.ok) {
          const xml = await resp.text();
          this.rules = this.parseReplicationXml(xml);
        } else {
          this.rules = [];
        }
      } catch {
        this.rules = [];
      }
      this.rulesLoading = false;
    },

    parseReplicationXml(xml) {
      const rules = [];
      const doc = new DOMParser().parseFromString(xml, 'text/xml');
      const ruleEls = doc.getElementsByTagName('Rule');
      for (const el of ruleEls) {
        const id = el.getElementsByTagName('ID')[0]?.textContent || '';
        const status = el.getElementsByTagName('Status')[0]?.textContent || 'Enabled';
        const priority = parseInt(el.getElementsByTagName('Priority')[0]?.textContent || '1', 10);

        // Filter: may be <Prefix> | <Tag> | <And><Prefix>...</Prefix><Tag>...</Tag>...</And>
        // Server-side (ReplicationFilter in arca-core) supports all three shapes.
        const filterEl = el.getElementsByTagName('Filter')[0];
        let prefix = '';
        let tags = [];
        if (filterEl) {
          const andEl = filterEl.getElementsByTagName('And')[0];
          if (andEl) {
            prefix = andEl.getElementsByTagName('Prefix')[0]?.textContent || '';
            for (const t of andEl.getElementsByTagName('Tag')) {
              tags.push({
                key: t.getElementsByTagName('Key')[0]?.textContent || '',
                value: t.getElementsByTagName('Value')[0]?.textContent || '',
              });
            }
          } else {
            // Direct child of <Filter>: either <Prefix> or a single <Tag>.
            // Only look at direct children so we don't accidentally pick up
            // tags nested in an <And> we already handled above.
            for (const child of filterEl.children) {
              if (child.tagName === 'Prefix') prefix = child.textContent || '';
              else if (child.tagName === 'Tag') {
                tags.push({
                  key: child.getElementsByTagName('Key')[0]?.textContent || '',
                  value: child.getElementsByTagName('Value')[0]?.textContent || '',
                });
              }
            }
          }
        }

        const destEl = el.getElementsByTagName('Destination')[0];
        const destBucket = destEl?.getElementsByTagName('Bucket')[0]?.textContent || '';
        const destEndpoint = destEl?.getElementsByTagName('Endpoint')[0]?.textContent || '';
        const destRegion = destEl?.getElementsByTagName('Region')[0]?.textContent || 'us-east-1';
        const credentialRef = destEl?.getElementsByTagName('CredentialRef')[0]?.textContent || '';
        const dmrEl = el.getElementsByTagName('DeleteMarkerReplication')[0];
        const dmrStatus = dmrEl?.getElementsByTagName('Status')[0]?.textContent || 'Enabled';
        rules.push({
          id, status, priority, prefix, tags,
          destBucket, destEndpoint, destRegion, credentialRef,
          deleteMarkers: dmrStatus === 'Enabled',
        });
      }
      return rules;
    },

    /**
     * Serialize the <Filter> block per AWS ReplicationConfiguration shape:
     *   - prefix only               → <Prefix>...</Prefix>
     *   - single tag, no prefix     → <Tag><Key/><Value/></Tag>
     *   - prefix + any tags, or ≥2  → <And><Prefix/><Tag/>...</And>
     *   - otherwise                 → <Prefix></Prefix>  (matches everything)
     */
    _renderFilterXml(prefix, tags) {
      const p = prefix || '';
      const ts = (tags || []).filter(t => t && t.key);
      if (!p && ts.length === 0) return '      <Prefix></Prefix>\n';
      if (ts.length === 0) return `      <Prefix>${this.escapeXml(p)}</Prefix>\n`;
      if (!p && ts.length === 1) {
        const t = ts[0];
        return `      <Tag><Key>${this.escapeXml(t.key)}</Key><Value>${this.escapeXml(t.value || '')}</Value></Tag>\n`;
      }
      let out = '      <And>\n';
      if (p) out += `        <Prefix>${this.escapeXml(p)}</Prefix>\n`;
      for (const t of ts) {
        out += `        <Tag><Key>${this.escapeXml(t.key)}</Key><Value>${this.escapeXml(t.value || '')}</Value></Tag>\n`;
      }
      out += '      </And>\n';
      return out;
    },

    buildReplicationXml() {
      let xml = '<?xml version="1.0" encoding="UTF-8"?>\n<ReplicationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">\n';
      xml += '  <Role></Role>\n';
      for (const r of this.rules) {
        xml += '  <Rule>\n';
        xml += `    <ID>${this.escapeXml(r.id)}</ID>\n`;
        xml += `    <Status>${this.escapeXml(r.status)}</Status>\n`;
        xml += `    <Priority>${r.priority | 0}</Priority>\n`;
        xml += '    <Filter>\n';
        xml += this._renderFilterXml(r.prefix, r.tags);
        xml += '    </Filter>\n';
        xml += '    <Destination>\n';
        xml += `      <Bucket>${this.escapeXml(r.destBucket)}</Bucket>\n`;
        xml += `      <Endpoint>${this.escapeXml(r.destEndpoint)}</Endpoint>\n`;
        xml += `      <Region>${this.escapeXml(r.destRegion || 'us-east-1')}</Region>\n`;
        xml += `      <CredentialRef>${this.escapeXml(r.credentialRef)}</CredentialRef>\n`;
        xml += '    </Destination>\n';
        xml += `    <DeleteMarkerReplication><Status>${r.deleteMarkers ? 'Enabled' : 'Disabled'}</Status></DeleteMarkerReplication>\n`;
        xml += '  </Rule>\n';
      }
      xml += '</ReplicationConfiguration>';
      return xml;
    },

    escapeXml(s) {
      return String(s ?? '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
    },

    openAddModal() {
      this.modalMode = 'add';
      this.editingIndex = -1;
      this.editForm = {
        id: 'rule-' + Math.random().toString(36).slice(2, 8),
        status: 'Enabled',
        priority: (this.rules.length + 1),
        prefix: '',
        tags: [],
        destBucket: '',
        destEndpoint: '',
        destRegion: 'us-east-1',
        credentialRef: this.knownCredentialRefs[0] || '',
        newCredentialMode: this.knownCredentialRefs.length === 0,
        newAccessKey: '',
        newSecretKey: '',
        deleteMarkers: true,
      };
      this.error = '';
      this.testResult = null;
      this.testing = false;
      this.showModal = true;
    },

    openEditModal(idx) {
      const r = this.rules[idx];
      this.modalMode = 'edit';
      this.editingIndex = idx;
      this.editForm = {
        id: r.id,
        status: r.status,
        priority: r.priority,
        prefix: r.prefix,
        tags: (r.tags || []).map(t => ({ key: t.key, value: t.value })),
        destBucket: r.destBucket,
        destEndpoint: r.destEndpoint,
        destRegion: r.destRegion || 'us-east-1',
        credentialRef: r.credentialRef,
        newCredentialMode: false,
        newAccessKey: '',
        newSecretKey: '',
        deleteMarkers: !!r.deleteMarkers,
      };
      this.error = '';
      this.testResult = null;
      this.testing = false;
      this.showModal = true;
    },

    addTagRow() {
      this.editForm.tags.push({ key: '', value: '' });
    },

    removeTagRow(idx) {
      this.editForm.tags.splice(idx, 1);
    },

    closeModal() { this.showModal = false; this.editingIndex = -1; this.error = ''; },

    pickCredentialRef(name) {
      this.editForm.credentialRef = name;
      this.editForm.newCredentialMode = false;
    },

    startNewCredential() {
      this.editForm.newCredentialMode = true;
      this.editForm.credentialRef = '';
      this.editForm.newAccessKey = '';
      this.editForm.newSecretKey = '';
    },

    async saveFromModal(bucket) {
      if (this.versioningEnabled === false) {
        this.error = 'Versioning must be Enabled on this bucket before adding replication rules.';
        return;
      }
      const f = this.editForm;
      if (!f.id) { this.error = 'Rule ID is required'; return; }
      if (!f.destBucket) { this.error = 'Destination bucket is required'; return; }
      if (!f.destEndpoint) { this.error = 'Destination endpoint URL is required'; return; }
      try { new URL(f.destEndpoint); } catch { this.error = 'Destination endpoint must be a valid URL'; return; }

      let credRef = f.credentialRef;
      if (f.newCredentialMode) {
        if (!credRef) { this.error = 'New credential name is required'; return; }
        if (!/^[A-Za-z0-9_.-]+$/.test(credRef)) {
          this.error = 'Credential name: letters, digits, dot, dash, underscore only'; return;
        }
        if (!f.newAccessKey || !f.newSecretKey) {
          this.error = 'Access key ID and secret are required for a new credential'; return;
        }
      }
      if (!credRef) { this.error = 'Select or create a destination credential'; return; }

      // Drop empty-key tag rows (UI scaffolding) and check for duplicate keys.
      const tagRows = (f.tags || []).filter(t => t && t.key);
      const seen = new Set();
      for (const t of tagRows) {
        if (seen.has(t.key)) {
          this.error = `Duplicate tag key '${t.key}' — each tag in a filter must have a unique key`;
          return;
        }
        seen.add(t.key);
      }

      this.error = '';
      this.saving = true;
      try {
        if (f.newCredentialMode) {
          const resp = await api.adminPost('/replication/credentials/' + encodeURIComponent(credRef), {
            access_key_id: f.newAccessKey,
            secret_access_key: f.newSecretKey,
          });
          if (!resp.ok) {
            const text = await resp.text();
            throw new Error('Could not save credential: ' + text);
          }
        }

        const newRule = {
          id: f.id,
          status: f.status,
          priority: Number(f.priority) || 1,
          prefix: f.prefix,
          tags: tagRows.map(t => ({ key: t.key, value: t.value || '' })),
          destBucket: f.destBucket,
          destEndpoint: f.destEndpoint,
          destRegion: f.destRegion || 'us-east-1',
          credentialRef: credRef,
          deleteMarkers: !!f.deleteMarkers,
        };
        if (this.editingIndex >= 0) this.rules[this.editingIndex] = newRule;
        else this.rules.push(newRule);

        await this.saveRules(bucket);
        this.closeModal();
      } catch (e) {
        this.error = e.message || 'Save failed';
      }
      this.saving = false;
    },

    async toggleEnabled(idx, bucket) {
      this.rules[idx].status = this.rules[idx].status === 'Enabled' ? 'Disabled' : 'Enabled';
      await this.saveRules(bucket);
    },

    async removeRule(idx, bucket) {
      this.rules.splice(idx, 1);
      if (this.rules.length === 0) {
        await api.s3DeleteBucketReplication(bucket);
      } else {
        await this.saveRules(bucket);
      }
    },

    async deleteAllRules(bucket) {
      this.saving = true; this.error = '';
      try {
        const resp = await api.s3DeleteBucketReplication(bucket);
        if (!resp.ok && resp.status !== 204 && resp.status !== 404) {
          const text = await resp.text();
          throw new Error(text || ('delete failed: ' + resp.status));
        }
        this.rules = [];
      } catch (e) {
        this.error = e.message || 'Delete failed';
      }
      this.saving = false;
    },

    async saveRules(bucket) {
      this.saving = true; this.error = '';
      try {
        const xml = this.buildReplicationXml();
        const resp = await api.s3PutBucketReplication(bucket, xml);
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text || ('save failed: ' + resp.status));
        }
      } catch (e) {
        this.error = e.message || 'Save failed';
      }
      this.saving = false;
    },

    scrollToVersioning() {
      const el = document.getElementById('versioning-card');
      if (el) el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    },

    // Connection test — POST /admin/replication/test-destination with the
    // current modal form values. The server does a signed HEAD on the
    // destination bucket and returns success/status; we render the result
    // inline under the Destination section.
    async testDestination() {
      const f = this.editForm;
      this.testResult = null;

      if (!f.destEndpoint) {
        this.testResult = { success: false, message: 'Destination endpoint is required' };
        return;
      }
      if (!f.destBucket) {
        this.testResult = { success: false, message: 'Destination bucket is required' };
        return;
      }

      // Build the request body: either inline AK/SK (new-credential mode) or
      // a reference to a stored credential.
      const payload = {
        endpoint: f.destEndpoint,
        bucket: f.destBucket,
        region: f.destRegion || 'us-east-1',
      };
      if (f.newCredentialMode) {
        if (!f.newAccessKey || !f.newSecretKey) {
          this.testResult = {
            success: false,
            message: 'Access key ID and secret are required to test a new credential',
          };
          return;
        }
        payload.access_key_id = f.newAccessKey;
        payload.secret_access_key = f.newSecretKey;
      } else {
        if (!f.credentialRef) {
          this.testResult = {
            success: false,
            message: 'Select or create a destination credential first',
          };
          return;
        }
        payload.credential_ref = f.credentialRef;
      }

      this.testing = true;
      try {
        const resp = await api.adminPost('/replication/test-destination', payload);
        if (!resp.ok) {
          const txt = await resp.text();
          throw new Error(`${resp.status}: ${txt}`);
        }
        const data = await resp.json();
        this.testResult = {
          success: !!data.success,
          message: data.status || (data.success ? 'Connected' : 'Connection failed'),
          detail: data.error || data.response_body || null,
          // Authoritative server identity via the destination's `Server` header.
          // "Arca" -> loop-prevention contract applies; anything else (MinIO,
          // AmazonS3, nginx, …) -> don't imply it does.
          server: data.server || null,
          isArca: !!data.is_arca,
        };
      } catch (e) {
        this.testResult = {
          success: false,
          message: 'Test failed',
          detail: e.message,
          server: null,
          isArca: false,
        };
      }
      this.testing = false;
    },

    shortDestination(r) {
      try {
        const u = new URL(r.destEndpoint);
        return `${r.destBucket}@${u.host}`;
      } catch {
        return `${r.destBucket || '?'}@${r.destEndpoint || '?'}`;
      }
    },

    statusChipClass(status) {
      return status === 'Enabled'
        ? 'bg-emerald-500/15 text-emerald-400 hover:bg-emerald-500/25'
        : 'bg-vault-border/30 text-vault-muted hover:bg-vault-border/50';
    },
  };
}
