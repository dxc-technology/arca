import { api } from '../api.js';

export function usersView() {
  return {
    users: [],
    loading: true,
    showCreateModal: false,
    newUsername: '',
    newDescription: '',
    creating: false,
    createError: '',
    showDeleteModal: false,
    deleteUser: null,
    deleteError: '',

    async load() {
      this.loading = true;
      try {
        this.users = await api.adminGet('/users');
      } catch {
        this.users = [];
      }
      this.loading = false;
    },

    openUser(userId) {
      window.location.hash = '#/users/' + encodeURIComponent(userId);
    },

    async createUser() {
      this.creating = true;
      this.createError = '';
      try {
        const resp = await api.adminPost('/users', {
          username: this.newUsername,
          description: this.newDescription,
        });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showCreateModal = false;
        this.newUsername = '';
        this.newDescription = '';
        await this.load();
      } catch (e) {
        this.createError = e.message;
      }
      this.creating = false;
    },

    confirmDelete(user) {
      this.deleteUser = user;
      this.deleteError = '';
      this.showDeleteModal = true;
    },

    async performDelete() {
      this.deleteError = '';
      try {
        const resp = await api.adminDelete('/users/' + this.deleteUser.user_id);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showDeleteModal = false;
        this.deleteUser = null;
        await this.load();
      } catch (e) {
        this.deleteError = e.message;
      }
    },

    formatDate(dateStr) {
      if (!dateStr) return '';
      return new Date(dateStr).toLocaleDateString();
    },
  };
}

export function userDetailView() {
  return {
    userId: '',
    user: null,
    credentials: [],
    grants: [],
    teams: [],
    effectiveGrants: [],
    loading: true,
    activeTab: 'credentials',

    // Credential creation
    showCreateCredModal: false,
    newCredDescription: '',
    newCredAccessKey: '',
    newCredSecretKey: '',
    creatingCred: false,
    createCredError: '',
    newCredential: null,

    // Credential deletion
    showDeleteCredModal: false,
    deleteCredId: '',
    deleteCredError: '',

    // Grant attachment
    showAttachGrantModal: false,
    allGrants: [],
    selectedGrantId: '',
    attachGrantError: '',

    // Grant detachment
    showDetachGrantModal: false,
    detachGrantId: '',
    detachGrantName: '',
    detachGrantError: '',

    async load() {
      const hash = window.location.hash || '';
      const match = hash.match(/^#\/users\/([^/?]+)/);
      this.userId = match ? decodeURIComponent(match[1]) : '';
      if (!this.userId) { this.loading = false; return; }
      this.loading = true;
      try {
        const [user, credentials, grants, teams, effective] = await Promise.allSettled([
          api.adminGet('/users/' + this.userId),
          api.adminGet('/users/' + this.userId + '/credentials'),
          api.adminGet('/users/' + this.userId + '/grants'),
          api.adminGet('/users/' + this.userId + '/teams'),
          api.adminGet('/users/' + this.userId + '/effective-grants'),
        ]);
        this.user = user.status === 'fulfilled' ? user.value : null;
        this.credentials = credentials.status === 'fulfilled' ? credentials.value : [];
        this.grants = grants.status === 'fulfilled' ? grants.value : [];
        this.teams = teams.status === 'fulfilled' ? teams.value : [];
        this.effectiveGrants = effective.status === 'fulfilled' ? effective.value : [];
      } catch {
        this.user = null;
      }
      this.loading = false;
    },

    // -- Credentials --

    async createCredential() {
      this.creatingCred = true;
      this.createCredError = '';
      try {
        const body = { description: this.newCredDescription };
        if (this.newCredAccessKey.trim()) body.access_key_id = this.newCredAccessKey.trim();
        if (this.newCredSecretKey.trim()) body.secret_access_key = this.newCredSecretKey.trim();
        const resp = await api.adminPost('/users/' + this.userId + '/credentials', body);
        if (!resp.ok) {
          const err = await resp.json();
          throw new Error(err.message || err.error || `Error ${resp.status}`);
        }
        this.newCredential = await resp.json();
        this.newCredDescription = '';
        this.newCredAccessKey = '';
        this.newCredSecretKey = '';
        // Reload credentials list (but keep modal open to show secret)
        const creds = await api.adminGet('/users/' + this.userId + '/credentials');
        this.credentials = creds;
      } catch (e) {
        this.createCredError = e.message;
      }
      this.creatingCred = false;
    },

    closeCreateCredModal() {
      this.showCreateCredModal = false;
      this.newCredential = null;
      this.createCredError = '';
      this.newCredDescription = '';
      this.newCredAccessKey = '';
      this.newCredSecretKey = '';
    },

    confirmDeleteCred(credId) {
      this.deleteCredId = credId;
      this.deleteCredError = '';
      this.showDeleteCredModal = true;
    },

    async performDeleteCred() {
      this.deleteCredError = '';
      try {
        const resp = await api.adminDelete('/credentials/' + this.deleteCredId);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showDeleteCredModal = false;
        const creds = await api.adminGet('/users/' + this.userId + '/credentials');
        this.credentials = creds;
      } catch (e) {
        this.deleteCredError = e.message;
      }
    },

    // -- Grants --

    async openAttachGrant() {
      this.attachGrantError = '';
      this.selectedGrantId = '';
      try {
        this.allGrants = await api.adminGet('/grants');
      } catch {
        this.allGrants = [];
      }
      this.showAttachGrantModal = true;
    },

    get availableGrants() {
      const attached = new Set(this.grants.map(g => g.grant_id));
      return this.allGrants.filter(g => !attached.has(g.grant_id));
    },

    async attachGrant() {
      this.attachGrantError = '';
      if (!this.selectedGrantId) return;
      try {
        const resp = await api.adminPut('/users/' + this.userId + '/grants/' + this.selectedGrantId);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showAttachGrantModal = false;
        await this.reloadGrants();
      } catch (e) {
        this.attachGrantError = e.message;
      }
    },

    confirmDetachGrant(grant) {
      this.detachGrantId = grant.grant_id;
      this.detachGrantName = grant.name;
      this.detachGrantError = '';
      this.showDetachGrantModal = true;
    },

    async performDetachGrant() {
      this.detachGrantError = '';
      try {
        const resp = await api.adminDelete('/users/' + this.userId + '/grants/' + this.detachGrantId);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showDetachGrantModal = false;
        await this.reloadGrants();
      } catch (e) {
        this.detachGrantError = e.message;
      }
    },

    async reloadGrants() {
      try {
        const [grants, effective] = await Promise.all([
          api.adminGet('/users/' + this.userId + '/grants'),
          api.adminGet('/users/' + this.userId + '/effective-grants'),
        ]);
        this.grants = grants;
        this.effectiveGrants = effective;
      } catch {}
    },

    // -- Helpers --

    formatDate(dateStr) {
      if (!dateStr) return '';
      return new Date(dateStr).toLocaleDateString();
    },

    copyToClipboard(text) {
      navigator.clipboard.writeText(text);
    },
  };
}
