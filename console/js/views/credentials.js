import { api } from '../api.js';

// ==================== CREDENTIALS VIEW ====================
export function credentialsView() {
  return {
    credentials: [],
    loading: true,
    showCreateModal: false,
    showDeleteModal: false,
    newCredDescription: '',
    newCredential: null,
    deleteCredId: '',
    creating: false,
    createError: '',
    deleteError: '',
    searchQuery: '',
    // Each view is its own x-data scope, so read the signed-in user locally
    // rather than relying on the root scope (same pattern as bucket-detail).
    username: sessionStorage.getItem('arca_username') || '',
    // user_id -> username, so credentials can show the owner by name.
    usernames: {},

    ownerName(cred) {
      return this.usernames[cred.user_id] || cred.user_id;
    },

    get filteredCredentials() {
      const q = this.searchQuery.toLowerCase().trim();
      if (!q) return this.credentials;
      return this.credentials.filter(c =>
        c.access_key_id.toLowerCase().includes(q) ||
        (c.description || '').toLowerCase().includes(q) ||
        this.ownerName(c).toLowerCase().includes(q)
      );
    },

    async load() {
      this.loading = true;
      try { this.credentials = await api.adminGet('/credentials'); } catch {}
      // Credentials carry the owning user_id; resolve names for display.
      try {
        const users = await api.adminGet('/users');
        this.usernames = Object.fromEntries(users.map(u => [u.user_id, u.username]));
      } catch {}
      this.loading = false;
    },

    async createCredential() {
      this.creating = true;
      this.createError = '';
      try {
        const resp = await api.adminPost('/credentials', { description: this.newCredDescription });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        this.newCredential = await resp.json();
        this.newCredDescription = '';
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
