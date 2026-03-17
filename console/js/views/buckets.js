import { api } from '../api.js';

// ==================== BUCKETS VIEW ====================
export function bucketsView() {
  return {
    buckets: [],
    loading: true,
    showCreateModal: false,
    newBucketName: '',
    creating: false,
    createError: '',

    async load() {
      this.loading = true;
      try {
        this.buckets = await api.s3ListBuckets();
        // Load encryption status for each bucket in parallel
        await Promise.all(this.buckets.map(async (b) => {
          const enc = await api.s3GetBucketEncryption(b.name);
          b.encrypted = !!(enc && enc.algorithm);
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

      this.loading = false;
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
