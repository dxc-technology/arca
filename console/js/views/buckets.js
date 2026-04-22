import { api } from '../api.js';
import { icons } from '../app.js';

// ==================== BUCKETS VIEW ====================
export function bucketsView() {
  return {
    buckets: [],
    loading: true,
    showCreateModal: false,
    newBucketName: '',
    newBucketObjectLock: false,
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
        // Load encryption, versioning, lock, and compression status in parallel.
        await Promise.all(this.buckets.map(async (b) => {
          const [enc, vResp, lockResp, comp] = await Promise.all([
            api.s3GetBucketEncryption(b.name),
            api.s3GetBucketVersioning(b.name).catch(() => null),
            api.s3GetObjectLockConfiguration(b.name).catch(() => null),
            api.s3GetBucketCompression(b.name).catch(() => null),
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
          b.compressed = comp ? (comp.algorithm || 'auto') : false;
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
        const resp = await api.s3CreateBucket(this.newBucketName, { objectLock: this.newBucketObjectLock });
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.showCreateModal = false;
        this.newBucketName = '';
        this.newBucketObjectLock = false;
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
    // Compression state (presence of config = enabled for this bucket).
    compressionEnabled: false,
    compressionAlgorithm: 'auto',
    compressionLevel: null,
    compressionSaving: false,
    compressionError: '',
    // Per-algorithm level metadata. Keys are algorithm names; `min`/`max`
    // are inclusive bounds; `def` is the level used if the user hasn't
    // picked one; `levels: false` means the algorithm has no tunable level.
    // The template reads this map directly (no getter indirection — some
    // Alpine expression-scope setups choke on custom getters defined
    // alongside many other state properties).
    compressionLevels: {
      auto:   { levels: false },
      zstd:   { min: 1, max: 22, def: 3 },
      lz4:    { levels: false },
      snappy: { levels: false },
      gzip:   { min: 0, max: 9,  def: 6 },
      brotli: { min: 0, max: 11, def: 4 },
      xz:     { min: 0, max: 9,  def: 6 },
    },
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
    // Irreversible-action confirmation modals. Versioning (once ENABLED) and
    // Object Lock cannot be turned off afterwards, so the first enable is
    // gated by a typed-name confirmation (same pattern as delete-bucket).
    showEnableVersioningModal: false,
    enableVersioningConfirmName: '',
    showEnableObjectLockModal: false,
    enableObjectLockConfirmName: '',
    pendingLockMode: '',
    pendingLockDays: null,
    // Lifecycle state — modal-based editor to match the notification /
    // replication / etc. pattern.
    lifecycleRules: [],
    lifecycleLoading: false,
    lifecycleError: '',
    lifecycleSaving: false,
    showLifecycleModal: false,
    lifecycleModalMode: 'add',          // 'add' | 'edit'
    editingLifecycleIndex: -1,           // -1 when adding
    lifecycleForm: {
      id: '',
      status: 'Enabled',
      prefix: '',
      expirationDays: '',
      noncurrentDays: '',
      abortUploadDays: '',
    },
    lifecycleFormError: '',

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

      // Fetch bucket compression config (presence = enabled).
      try {
        const comp = await api.s3GetBucketCompression(this.bucketName);
        if (comp) {
          this.compressionEnabled = true;
          this.compressionAlgorithm = comp.algorithm || 'auto';
          this.compressionLevel = comp.level;
        }
      } catch {}

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

    // Gate Object Lock enable behind a typed confirmation: once enabled it
    // cannot be disabled and WORM retention becomes enforceable on new objects.
    requestEnableObjectLock(mode, days) {
      this.pendingLockMode = mode || '';
      this.pendingLockDays = days || null;
      this.enableObjectLockConfirmName = '';
      this.objectLockError = '';
      this.showEnableObjectLockModal = true;
    },

    async confirmEnableObjectLock() {
      this.showEnableObjectLockModal = false;
      await this.enableObjectLock(this.pendingLockMode, this.pendingLockDays);
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

    openLifecycleAddModal() {
      this.lifecycleModalMode = 'add';
      this.editingLifecycleIndex = -1;
      this.lifecycleForm = {
        id: 'rule-' + Math.random().toString(36).slice(2, 8),
        status: 'Enabled',
        prefix: '',
        expirationDays: '',
        noncurrentDays: '',
        abortUploadDays: '',
      };
      this.lifecycleFormError = '';
      this.showLifecycleModal = true;
    },

    openLifecycleEditModal(index) {
      const r = this.lifecycleRules[index];
      this.lifecycleModalMode = 'edit';
      this.editingLifecycleIndex = index;
      this.lifecycleForm = {
        id: r.id || '',
        status: r.status || 'Enabled',
        prefix: r.prefix || '',
        expirationDays: r.expirationDays || '',
        noncurrentDays: r.noncurrentDays || '',
        abortUploadDays: r.abortUploadDays || '',
      };
      this.lifecycleFormError = '';
      this.showLifecycleModal = true;
    },

    closeLifecycleModal() {
      this.showLifecycleModal = false;
      this.editingLifecycleIndex = -1;
      this.lifecycleFormError = '';
    },

    async saveLifecycleFromModal() {
      const f = this.lifecycleForm;
      if (!f.expirationDays && !f.noncurrentDays && !f.abortUploadDays) {
        this.lifecycleFormError =
          'At least one action is required (expiration, noncurrent version, or abort multipart).';
        return;
      }
      const rule = {
        id: f.id || 'rule-' + Date.now(),
        status: f.status || 'Enabled',
        prefix: f.prefix || '',
        expirationDays: f.expirationDays || '',
        noncurrentDays: f.noncurrentDays || '',
        abortUploadDays: f.abortUploadDays || '',
      };
      if (this.editingLifecycleIndex >= 0) {
        this.lifecycleRules[this.editingLifecycleIndex] = rule;
      } else {
        this.lifecycleRules.push(rule);
      }
      this.closeLifecycleModal();
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

    async deleteAllLifecycleRules() {
      this.lifecycleRules = [];
      await this._persistRules();
    },

    // Gate the toggle: the Disabled -> Enabled transition is irreversible
    // (AWS S3 versioning can only be Suspended afterwards, never turned off),
    // so we ask for typed confirmation. Suspended <-> Enabled is reversible
    // and goes through directly.
    requestToggleVersioning() {
      if (!this.versioningStatus) {
        this.enableVersioningConfirmName = '';
        this.versioningError = '';
        this.showEnableVersioningModal = true;
        return;
      }
      this.toggleVersioning();
    },

    async confirmEnableVersioning() {
      this.showEnableVersioningModal = false;
      await this.toggleVersioning();
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

    /**
     * Primary toggle: if OFF, enable with the current algorithm/level; if ON,
     * delete the per-bucket config. Mirrors toggleVersioning's UX.
     */
    async toggleCompression() {
      if (!this.compressionEnabled) {
        await this._putCompression(this.compressionAlgorithm || 'auto',
          this.compressionSupportsLevel ? this.compressionLevel : null);
      } else {
        await this._deleteCompression();
      }
    },

    /**
     * Live-update: user switched algorithm or level while the toggle is ON.
     * For level, we re-PUT with the new value; for algorithm we also reset the
     * level to that algorithm's default (or null when the algorithm has none).
     */
    async updateCompressionAlgorithm() {
      if (!this.compressionEnabled) return;
      const meta = this.compressionLevels[this.compressionAlgorithm] || {};
      const newLevel = meta.levels === false ? null : (meta.def ?? 3);
      this.compressionLevel = newLevel;
      await this._putCompression(this.compressionAlgorithm, newLevel);
    },

    async updateCompressionLevel() {
      if (!this.compressionEnabled) return;
      await this._putCompression(this.compressionAlgorithm, this.compressionLevel);
    },

    async _putCompression(algorithm, level) {
      this.compressionSaving = true;
      this.compressionError = '';
      try {
        const resp = await api.s3PutBucketCompression(this.bucketName, {
          algorithm,
          level: this.compressionLevels[algorithm]?.levels === false ? null : level,
        });
        if (!resp.ok) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.compressionEnabled = true;
        this.compressionAlgorithm = algorithm;
        this.compressionLevel = this.compressionLevels[algorithm]?.levels === false
          ? null
          : (level ?? null);
      } catch (e) {
        this.compressionError = e.message;
      }
      this.compressionSaving = false;
    },

    async _deleteCompression() {
      this.compressionSaving = true;
      this.compressionError = '';
      try {
        const resp = await api.s3DeleteBucketCompression(this.bucketName);
        if (!resp.ok && resp.status !== 204) {
          const text = await resp.text();
          throw new Error(text.match(/<Message>(.*?)<\/Message>/)?.[1] || `Error ${resp.status}`);
        }
        this.compressionEnabled = false;
        this.compressionAlgorithm = 'auto';
        this.compressionLevel = null;
      } catch (e) {
        this.compressionError = e.message;
      }
      this.compressionSaving = false;
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
