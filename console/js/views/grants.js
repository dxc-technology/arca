import { api } from '../api.js';

const TEMPLATE_FULL_ACCESS = JSON.stringify({
  Version: '2012-10-17',
  Statement: [{
    Effect: 'Allow',
    Action: ['*'],
    Resource: ['*'],
  }],
}, null, 2);

const TEMPLATE_S3_FULL_ACCESS = JSON.stringify({
  Version: '2012-10-17',
  Statement: [{
    Effect: 'Allow',
    Action: ['s3:*'],
    Resource: ['*'],
  }],
}, null, 2);

const TEMPLATE_S3_READ_ONLY = JSON.stringify({
  Version: '2012-10-17',
  Statement: [{
    Effect: 'Allow',
    Action: [
      's3:GetObject',
      's3:ListBucket',
      's3:ListAllMyBuckets',
      's3:GetBucketLocation',
      's3:GetBucketEncryption',
    ],
    Resource: ['*'],
  }],
}, null, 2);

export function grantsView() {
  return {
    grants: [],
    loading: true,
    showCreateModal: false,
    newName: '',
    newDescription: '',
    newDocument: TEMPLATE_S3_FULL_ACCESS,
    creating: false,
    createError: '',
    showDeleteModal: false,
    deleteGrant: null,
    deleteError: '',
    searchQuery: '',

    get filteredGrants() {
      const q = this.searchQuery.toLowerCase().trim();
      if (!q) return this.grants;
      return this.grants.filter(g =>
        g.name.toLowerCase().includes(q) ||
        (g.description || '').toLowerCase().includes(q)
      );
    },

    async load() {
      this.loading = true;
      try {
        this.grants = await api.adminGet('/grants');
      } catch {
        this.grants = [];
      }
      this.loading = false;
    },

    openGrant(grantId) {
      window.location.hash = '#/grants/' + encodeURIComponent(grantId);
    },

    isBuiltIn(grant) {
      return grant.grant_id && grant.grant_id.startsWith('grant-');
    },

    applyTemplate(template) {
      if (template === 'full-access') this.newDocument = TEMPLATE_FULL_ACCESS;
      else if (template === 's3-full-access') this.newDocument = TEMPLATE_S3_FULL_ACCESS;
      else if (template === 's3-read-only') this.newDocument = TEMPLATE_S3_READ_ONLY;
    },

    formatNewDocument() {
      try {
        const parsed = JSON.parse(this.newDocument);
        this.newDocument = JSON.stringify(parsed, null, 2);
        this.createError = '';
      } catch (e) {
        this.createError = 'Invalid JSON: ' + e.message;
      }
    },

    async createGrant() {
      this.creating = true;
      this.createError = '';
      try {
        let document;
        try {
          document = JSON.parse(this.newDocument);
        } catch (e) {
          throw new Error('Invalid JSON: ' + e.message);
        }
        const resp = await api.adminPost('/grants', {
          name: this.newName,
          description: this.newDescription,
          document,
        });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        this.showCreateModal = false;
        this.newName = '';
        this.newDescription = '';
        this.newDocument = TEMPLATE_S3_FULL_ACCESS;
        await this.load();
      } catch (e) {
        this.createError = e.message;
      }
      this.creating = false;
    },

    confirmDelete(grant) {
      this.deleteGrant = grant;
      this.deleteError = '';
      this.showDeleteModal = true;
    },

    async performDelete() {
      this.deleteError = '';
      try {
        const resp = await api.adminDelete('/grants/' + this.deleteGrant.grant_id);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        this.showDeleteModal = false;
        this.deleteGrant = null;
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

export function grantDetailView() {
  return {
    grantId: '',
    grant: null,
    attachedUsers: [],
    attachedTeams: [],
    loading: true,
    editName: '',
    editDescription: '',
    editDocument: '',
    saving: false,
    saveError: '',
    saveSuccess: false,

    // Delete
    showDeleteModal: false,
    deleteError: '',

    async load() {
      const hash = window.location.hash || '';
      const match = hash.match(/^#\/grants\/([^/?]+)/);
      this.grantId = match ? decodeURIComponent(match[1]) : '';
      if (!this.grantId) { this.loading = false; return; }
      this.loading = true;
      try {
        const [grant, users, teams] = await Promise.allSettled([
          api.adminGet('/grants/' + this.grantId),
          api.adminGet('/users'),
          api.adminGet('/teams'),
        ]);

        if (grant.status === 'fulfilled') {
          this.grant = grant.value;
          this.editName = this.grant.name || '';
          this.editDescription = this.grant.description || '';
          this.editDocument = this.grant.document
            ? (typeof this.grant.document === 'string'
              ? this.grant.document
              : JSON.stringify(this.grant.document, null, 2))
            : '';
        }

        // Find users and teams that have this grant attached
        if (users.status === 'fulfilled') {
          await this.loadAttachedUsers(users.value);
        }
        if (teams.status === 'fulfilled') {
          await this.loadAttachedTeams(teams.value);
        }
      } catch {
        this.grant = null;
      }
      this.loading = false;
    },

    async loadAttachedUsers(allUsers) {
      const attached = [];
      await Promise.all(allUsers.map(async (user) => {
        try {
          const grants = await api.adminGet('/users/' + user.user_id + '/grants');
          if (grants.some(g => g.grant_id === this.grantId)) {
            attached.push(user);
          }
        } catch {}
      }));
      this.attachedUsers = attached;
    },

    async loadAttachedTeams(allTeams) {
      const attached = [];
      await Promise.all(allTeams.map(async (team) => {
        try {
          const grants = await api.adminGet('/teams/' + team.team_id + '/grants');
          if (grants.some(g => g.grant_id === this.grantId)) {
            attached.push(team);
          }
        } catch {}
      }));
      this.attachedTeams = attached;
    },

    isBuiltIn() {
      return this.grant && this.grant.grant_id && this.grant.grant_id.startsWith('grant-');
    },

    formatDocument() {
      try {
        const parsed = JSON.parse(this.editDocument);
        this.editDocument = JSON.stringify(parsed, null, 2);
        this.saveError = '';
      } catch (e) {
        this.saveError = 'Invalid JSON: ' + e.message;
      }
    },

    applyTemplate(template) {
      if (template === 'full-access') this.editDocument = TEMPLATE_FULL_ACCESS;
      else if (template === 's3-full-access') this.editDocument = TEMPLATE_S3_FULL_ACCESS;
      else if (template === 's3-read-only') this.editDocument = TEMPLATE_S3_READ_ONLY;
    },

    get parsedStatements() {
      try {
        const doc = typeof this.editDocument === 'string'
          ? JSON.parse(this.editDocument)
          : this.editDocument;
        if (!doc || !doc.Statement) return [];
        return doc.Statement.map(s => ({
          effect: s.Effect || 'Allow',
          actions: Array.isArray(s.Action) ? s.Action : [s.Action || '*'],
          resources: Array.isArray(s.Resource) ? s.Resource : [s.Resource || '*'],
        }));
      } catch {
        return [];
      }
    },

    async saveField(data) {
      try {
        const resp = await api.adminPut('/grants/' + this.grantId, data);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        const grant = await api.adminGet('/grants/' + this.grantId);
        this.grant = grant;
        this.editName = grant.name || '';
        this.editDescription = grant.description || '';
      } catch (e) {
        alert(e.message);
        this.editName = this.grant?.name || '';
        this.editDescription = this.grant?.description || '';
      }
    },

    async save() {
      this.saving = true;
      this.saveError = '';
      this.saveSuccess = false;
      try {
        let document;
        try {
          document = JSON.parse(this.editDocument);
        } catch (e) {
          throw new Error('Invalid JSON: ' + e.message);
        }
        const resp = await api.adminPut('/grants/' + this.grantId, {
          name: this.editName,
          description: this.editDescription,
          document,
        });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        this.saveSuccess = true;
        setTimeout(() => { this.saveSuccess = false; }, 2000);
        // Reload to get the canonical state
        const grant = await api.adminGet('/grants/' + this.grantId);
        this.grant = grant;
        this.editName = grant.name || '';
        this.editDescription = grant.description || '';
        this.editDocument = grant.document
          ? (typeof grant.document === 'string'
            ? grant.document
            : JSON.stringify(grant.document, null, 2))
          : '';
      } catch (e) {
        this.saveError = e.message;
      }
      this.saving = false;
    },

    confirmDelete() {
      this.deleteError = '';
      this.showDeleteModal = true;
    },

    async performDelete() {
      this.deleteError = '';
      try {
        const resp = await api.adminDelete('/grants/' + this.grantId);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || body.error || `Error ${resp.status}`);
        }
        window.location.hash = '#/grants';
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
