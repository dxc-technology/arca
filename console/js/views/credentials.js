import { api } from '../api.js';

// ==================== CREDENTIALS VIEW ====================
export function credentialsView() {
  return {
    credentials: [],
    loading: true,
    showCreateModal: false,
    showDeleteModal: false,
    newCredDescription: '',
    newCredAdmin: false,
    newCredential: null,
    deleteCredId: '',
    creating: false,
    createError: '',
    deleteError: '',
    searchQuery: '',

    get filteredCredentials() {
      const q = this.searchQuery.toLowerCase().trim();
      if (!q) return this.credentials;
      return this.credentials.filter(c =>
        c.access_key_id.toLowerCase().includes(q) ||
        (c.description || '').toLowerCase().includes(q)
      );
    },

    async load() {
      this.loading = true;
      try { this.credentials = await api.adminGet('/credentials'); } catch {}
      this.loading = false;
    },

    async createCredential() {
      this.creating = true;
      this.createError = '';
      try {
        const resp = await api.adminPost('/credentials', { description: this.newCredDescription, admin: this.newCredAdmin });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        this.newCredential = await resp.json();
        this.newCredDescription = '';
        this.newCredAdmin = false;
      } catch (e) { this.createError = e.message; }
      this.creating = false;
    },

    confirmDeleteCred(id) {
      this.deleteCredId = id;
      this.deleteError = '';
      this.showDeleteModal = true;
    },

    async deleteCred() {
      this.deleteError = '';
      try {
        const resp = await api.adminDelete('/credentials/' + this.deleteCredId);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        this.showDeleteModal = false;
        await this.load();
      } catch (e) { this.deleteError = e.message; }
    },

    copyToClipboard(text) {
      navigator.clipboard.writeText(text);
    },
  };
}
