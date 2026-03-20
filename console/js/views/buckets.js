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

    async load() {
      this.loading = true;
      try {
        this.buckets = await api.s3ListBuckets();
        // Load encryption and versioning status for each bucket in parallel
        await Promise.all(this.buckets.map(async (b) => {
          const [enc, vResp] = await Promise.all([
            api.s3GetBucketEncryption(b.name),
            api.s3GetBucketVersioning(b.name).catch(() => null),
          ]);
          b.encrypted = !!(enc && enc.algorithm);
          if (vResp && vResp.ok) {
            const xml = await vResp.text();
            const m = xml.match(/<Status>(.*?)<\/Status>/);
            b.versioned = m ? m[1] : false;
          } else {
            b.versioned = false;
          }
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

      this.loading = false;
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
