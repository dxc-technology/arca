import { api } from '../api.js';
import { icons } from '../app.js';

// ==================== BUCKETS VIEW ====================
export function bucketsView() {
  return {
    buckets: [],
    loading: true,
    showCreateModal: false,
    newBucketName: '',
    creating: false,
    createError: '',
    icons,
    searchQuery: '',

    get filteredBuckets() {
      const q = this.searchQuery.toLowerCase().trim();
      if (!q) return this.buckets;
      return this.buckets.filter(b => b.name.toLowerCase().includes(q));
    },

    async load() {
      this.loading = true;
      try {
        this.buckets = await api.s3ListBuckets();
        // Load encryption and versioning status for each bucket in parallel
        await Promise.all(this.buckets.map(async (b) => {
          const [enc, vResp, lockResp] = await Promise.all([
            api.s3GetBucketEncryption(b.name),
            api.s3GetBucketVersioning(b.name).catch(() => null),
            api.s3GetObjectLockConfiguration(b.name).catch(() => null),
          ]);
          b.encrypted = !!(enc && enc.algorithm);
          if (vResp && vResp.ok) {
            const xml = await vResp.text();
            const m = xml.match(/<Status>(.*?)<\/Status>/);
            b.versioned = m ? m[1] : false;
          } else {
            b.versioned = false;
          }
          b.locked = !!(lockResp && lockResp.ok);
        }));
      } catch {}
      this.loading = false;
    },

    openBucket(name) {
      this.$root.__x_app = this;
      window.location.hash = '#/buckets/' + encodeURIComponent(name);
    },

    async createBucket() {
      this.creating = true;
      this.createError = '';
      try {
        const resp = await api.s3CreateBucket(this.newBucketName);
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.showCreateModal = false;
        this.newBucketName = '';
        await this.load();
      } catch (e) { this.createError = e.message; }
      this.creating = false;
    },
  };
}

// ==================== BUCKET SETTINGS VIEW ====================
export function bucketSettingsView() {
  return {
    bucketName: '',
    loading: true,
    adminInfoAvailable: false,
    serverEncryptionEnabled: false,
    kmsProvider: null,
    encryptionActive: false,
    encryptionOverride: false,
    encryptionSaving: false,
    encryptionError: '',
    versioningStatus: null,
    versioningSaving: false,
    versioningError: '',
    showDeleteModal: false,
    deleteBucketConfirmName: '',
    deleteError: '',
    deleting: false,
    // Object Lock state
    objectLockEnabled: false,
    objectLockMode: null,
    objectLockDays: null,
    objectLockSaving: false,
    objectLockError: '',
    // Lifecycle state
    lifecycleRules: [],
    lifecycleLoading: false,
    lifecycleError: '',
    lifecycleSaving: false,
    showAddRule: false,
    editingIndex: -1,
    editRule: null,
    newRule: null,

    async load() {
      const hash = window.location.hash || '';
      const match = hash.match(/^#\/buckets\/([^/?]+)\/settings$/);
      this.bucketName = match ? decodeURIComponent(match[1]) : '';
      if (!this.bucketName) { this.loading = false; return; }
      this.loading = true;

      // Fetch admin info (best-effort — non-admin users get 403)
      try {
        const info = await api.adminGet('/info');
        this.adminInfoAvailable = true;
        this.serverEncryptionEnabled = !!info.encryption_enabled;
        this.kmsProvider = info.kms_provider || null;
      } catch {
        this.adminInfoAvailable = false;
        this.serverEncryptionEnabled = false;
      }

      // Fetch bucket encryption config
      const enc = await api.s3GetBucketEncryption(this.bucketName);
      if (enc && enc.algorithm === 'AES256') {
        this.encryptionActive = true;
        // If server default is off but bucket has encryption, it must be a per-bucket override
        // If server default is on, it could be inherited — only mark override if we can confirm
        this.encryptionOverride = this.adminInfoAvailable ? !this.serverEncryptionEnabled : true;
      } else {
        this.encryptionActive = false;
        this.encryptionOverride = false;
      }

      // Fetch bucket versioning config
      try {
        const vResp = await api.s3GetBucketVersioning(this.bucketName);
        if (vResp.ok) {
          const vXml = await vResp.text();
          const statusMatch = vXml.match(/<Status>(.*?)<\/Status>/);
          this.versioningStatus = statusMatch ? statusMatch[1] : null;
        }
      } catch {}

      // Fetch Object Lock config
      try {
        const lockResp = await api.s3GetObjectLockConfiguration(this.bucketName);
        if (lockResp.ok) {
          this.objectLockEnabled = true;
          const xml = await lockResp.text();
          const modeMatch = xml.match(/<Mode>(.*?)<\/Mode>/);
          const daysMatch = xml.match(/<Days>(.*?)<\/Days>/);
          this.objectLockMode = modeMatch ? modeMatch[1] : null;
          this.objectLockDays = daysMatch ? parseInt(daysMatch[1], 10) : null;
        }
      } catch {}

      // Fetch lifecycle rules
      await this.loadLifecycleRules();

      this.loading = false;
    },

    async enableObjectLock(mode, days) {
      this.objectLockSaving = true;
      this.objectLockError = '';
      try {
        let ruleXml = '';
        if (mode && days) {
          ruleXml = `<Rule><DefaultRetention><Mode>${mode}</Mode><Days>${days}</Days></DefaultRetention></Rule>`;
        }
        const xml = `<ObjectLockConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><ObjectLockEnabled>Enabled</ObjectLockEnabled>${ruleXml}</ObjectLockConfiguration>`;
        const resp = await api.s3PutObjectLockConfiguration(this.bucketName, xml);
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.objectLockEnabled = true;
        this.objectLockMode = mode || null;
        this.objectLockDays = days || null;
        // Object Lock auto-enables versioning
        this.versioningStatus = 'Enabled';
      } catch (e) {
        this.objectLockError = e.message;
      }
      this.objectLockSaving = false;
    },

    async loadLifecycleRules() {
      this.lifecycleLoading = true;
      this.lifecycleError = '';
      try {
        const resp = await api.s3GetBucketLifecycle(this.bucketName);
        if (resp.ok) {
          const xml = await resp.text();
          this.lifecycleRules = parseLifecycleXml(xml);
        } else {
          this.lifecycleRules = [];
        }
      } catch {
        this.lifecycleRules = [];
      }
      this.lifecycleLoading = false;
    },

    /** Persist current rules array to the server. */
    async _persistRules() {
      this.lifecycleSaving = true;
      this.lifecycleError = '';
      try {
        if (this.lifecycleRules.length === 0) {
          const resp = await api.s3DeleteBucketLifecycle(this.bucketName);
          if (!resp.ok && resp.status !== 204) throw new Error(`Error ${resp.status}`);
        } else {
          const xml = buildLifecycleXml(this.lifecycleRules);
          const resp = await api.s3PutBucketLifecycle(this.bucketName, xml);
          if (!resp.ok) {
            const text = await resp.text();
            throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
          }
        }
      } catch (e) {
        this.lifecycleError = e.message;
        // Reload to get back to server state on error
        await this.loadLifecycleRules();
      }
      this.lifecycleSaving = false;
    },

    initNewRule() {
      this.editingIndex = -1;
      this.editRule = null;
      this.newRule = {
        id: '',
        status: 'Enabled',
        prefix: '',
        expirationDays: '',
        noncurrentDays: '',
        abortUploadDays: '',
      };
      this.showAddRule = true;
    },

    cancelAddRule() {
      this.showAddRule = false;
      this.newRule = null;
    },

    async addRule() {
      if (!this.newRule) return;
      const rule = { ...this.newRule };
      if (!rule.id) rule.id = 'rule-' + Date.now();
      this.lifecycleRules.push(rule);
      this.showAddRule = false;
      this.newRule = null;
      await this._persistRules();
    },

    async removeRule(index) {
      this.lifecycleRules.splice(index, 1);
      await this._persistRules();
    },

    async toggleRuleStatus(index) {
      const rule = this.lifecycleRules[index];
      rule.status = rule.status === 'Enabled' ? 'Disabled' : 'Enabled';
      await this._persistRules();
    },

    startEditRule(index) {
      this.showAddRule = false;
      this.newRule = null;
      this.editingIndex = index;
      this.editRule = { ...this.lifecycleRules[index] };
    },

    cancelEditRule() {
      this.editingIndex = -1;
      this.editRule = null;
    },

    async saveEditRule() {
      if (!this.editRule || this.editingIndex < 0) return;
      this.lifecycleRules[this.editingIndex] = { ...this.editRule };
      this.editingIndex = -1;
      this.editRule = null;
      await this._persistRules();
    },

    async deleteAllLifecycleRules() {
      this.lifecycleRules = [];
      await this._persistRules();
    },

    async toggleVersioning() {
      // Cycle: Disabled -> Enabled, Enabled -> Suspended, Suspended -> Enabled
      const newStatus = this.versioningStatus === 'Enabled' ? 'Suspended' : 'Enabled';
      this.versioningSaving = true;
      this.versioningError = '';
      try {
        const resp = await api.s3PutBucketVersioning(this.bucketName, newStatus);
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.versioningStatus = newStatus;
      } catch (e) {
        this.versioningError = e.message;
      }
      this.versioningSaving = false;
    },

    async toggleEncryption() {
      this.encryptionSaving = true;
      this.encryptionError = '';
      try {
        if (!this.encryptionOverride) {
          // Turn ON per-bucket encryption
          const resp = await api.s3PutBucketEncryption(this.bucketName);
          if (!resp.ok) {
            const text = await resp.text();
            throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
          }
          this.encryptionOverride = true;
          this.encryptionActive = true;
        } else {
          // Turn OFF per-bucket override
          const resp = await api.s3DeleteBucketEncryption(this.bucketName);
          if (!resp.ok && resp.status !== 204) {
            const text = await resp.text();
            throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
          }
          this.encryptionOverride = false;
          // After removing override, effective encryption depends on server default
          this.encryptionActive = this.serverEncryptionEnabled;
        }
      } catch (e) {
        this.encryptionError = e.message;
      }
      this.encryptionSaving = false;
    },

    async deleteBucket() {
      this.deleteError = '';
      this.deleting = true;
      try {
        const resp = await api.s3DeleteBucket(this.bucketName);
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.showDeleteModal = false;
        window.location.hash = '#/buckets';
      } catch (e) { this.deleteError = e.message; }
      this.deleting = false;
    },
  };
}

// ==================== LIFECYCLE HELPERS ====================

/** Parse lifecycle configuration XML into an array of rule objects. */
function parseLifecycleXml(xml) {
  const doc = new DOMParser().parseFromString(xml, 'text/xml');
  const rules = [];
  for (const ruleEl of doc.querySelectorAll('Rule')) {
    const id = ruleEl.querySelector('ID')?.textContent || '';
    const status = ruleEl.querySelector('Status')?.textContent || 'Enabled';

    // Parse filter prefix
    let prefix = '';
    const filterEl = ruleEl.querySelector('Filter');
    if (filterEl) {
      const andEl = filterEl.querySelector('And');
      if (andEl) {
        prefix = andEl.querySelector('Prefix')?.textContent || '';
      } else {
        prefix = filterEl.querySelector('Prefix')?.textContent || '';
      }
    }

    const expirationDays = ruleEl.querySelector('Expiration > Days')?.textContent || '';
    const noncurrentDays = ruleEl.querySelector('NoncurrentVersionExpiration > NoncurrentDays')?.textContent || '';
    const abortUploadDays = ruleEl.querySelector('AbortIncompleteMultipartUpload > DaysAfterInitiation')?.textContent || '';

    rules.push({ id, status, prefix, expirationDays, noncurrentDays, abortUploadDays });
  }
  return rules;
}

/** Build lifecycle configuration XML from an array of rule objects. */
function buildLifecycleXml(rules) {
  let xml = '<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">';
  for (const rule of rules) {
    xml += '<Rule>';
    xml += `<ID>${escapeXml(rule.id)}</ID>`;
    xml += `<Status>${rule.status}</Status>`;
    xml += `<Filter><Prefix>${escapeXml(rule.prefix || '')}</Prefix></Filter>`;
    if (rule.expirationDays) {
      xml += `<Expiration><Days>${parseInt(rule.expirationDays, 10)}</Days></Expiration>`;
    }
    if (rule.noncurrentDays) {
      xml += `<NoncurrentVersionExpiration><NoncurrentDays>${parseInt(rule.noncurrentDays, 10)}</NoncurrentDays></NoncurrentVersionExpiration>`;
    }
    if (rule.abortUploadDays) {
      xml += `<AbortIncompleteMultipartUpload><DaysAfterInitiation>${parseInt(rule.abortUploadDays, 10)}</DaysAfterInitiation></AbortIncompleteMultipartUpload>`;
    }
    xml += '</Rule>';
  }
  xml += '</LifecycleConfiguration>';
  return xml;
}

function escapeXml(str) {
  return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
