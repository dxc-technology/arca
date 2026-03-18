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
    effectiveGrants: [],
    loading: true,
    activeTab: 'credentials',

    // Edit fields
    editDescription: '',
    saving: false,
    saveError: '',
    saveSuccess: false,

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

    // Teams shuttle
    teamsAll: [],
    teamsAssignedIds: [],
    teamsSelLeft: [],
    teamsSelRight: [],
    teamsLastLeft: null,
    teamsLastRight: null,
    teamsDragData: null,
    teamsMoving: false,

    // Grants shuttle
    grantsAll: [],
    grantsAssignedIds: [],
    grantsSelLeft: [],
    grantsSelRight: [],
    grantsLastLeft: null,
    grantsLastRight: null,
    grantsDragData: null,
    grantsMoving: false,

    async load() {
      const hash = window.location.hash || '';
      const match = hash.match(/^#\/users\/([^/?]+)/);
      this.userId = match ? decodeURIComponent(match[1]) : '';
      if (!this.userId) { this.loading = false; return; }
      this.loading = true;
      try {
        const [user, credentials, grants, teams, effective, allTeams, allGrants] = await Promise.allSettled([
          api.adminGet('/users/' + this.userId),
          api.adminGet('/users/' + this.userId + '/credentials'),
          api.adminGet('/users/' + this.userId + '/grants'),
          api.adminGet('/users/' + this.userId + '/teams'),
          api.adminGet('/users/' + this.userId + '/effective-grants'),
          api.adminGet('/teams'),
          api.adminGet('/grants'),
        ]);
        this.user = user.status === 'fulfilled' ? user.value : null;
        this.editDescription = this.user?.description || '';
        this.credentials = credentials.status === 'fulfilled' ? credentials.value : [];
        this.effectiveGrants = effective.status === 'fulfilled' ? effective.value : [];
        // Teams shuttle
        this.teamsAll = allTeams.status === 'fulfilled' ? allTeams.value : [];
        const teamsList = teams.status === 'fulfilled' ? teams.value : [];
        this.teamsAssignedIds = teamsList.map(t => t.team_id);
        // Grants shuttle
        this.grantsAll = allGrants.status === 'fulfilled' ? allGrants.value : [];
        const grantsList = grants.status === 'fulfilled' ? grants.value : [];
        this.grantsAssignedIds = grantsList.map(g => g.grant_id);
      } catch {
        this.user = null;
      }
      this.loading = false;
    },

    // -- Save description --

    async save() {
      this.saving = true;
      this.saveError = '';
      this.saveSuccess = false;
      try {
        const resp = await api.adminPut('/users/' + this.userId, {
          description: this.editDescription,
        });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.saveSuccess = true;
        setTimeout(() => { this.saveSuccess = false; }, 2000);
        // Reload to get canonical state
        const user = await api.adminGet('/users/' + this.userId);
        this.user = user;
        this.editDescription = user.description || '';
      } catch (e) {
        this.saveError = e.message;
      }
      this.saving = false;
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

    async toggleCredActive(credId, active) {
      try {
        const resp = await api.adminPut('/credentials/' + credId, { active });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        // Reload credentials list
        const creds = await api.adminGet('/users/' + this.userId + '/credentials');
        this.credentials = creds;
      } catch (e) {
        alert(e.message);
      }
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

    // -- Teams shuttle --

    get teamsAvailable() {
      const ids = new Set(this.teamsAssignedIds);
      return this.teamsAll.filter(t => !ids.has(t.team_id));
    },

    get teamsAssigned() {
      const ids = new Set(this.teamsAssignedIds);
      return this.teamsAll.filter(t => ids.has(t.team_id));
    },

    teamsToggle(side, id, event) {
      const sel = side === 'left' ? this.teamsSelLeft : this.teamsSelRight;
      const items = side === 'left' ? this.teamsAvailable : this.teamsAssigned;
      const lastKey = side === 'left' ? 'teamsLastLeft' : 'teamsLastRight';
      const ids = items.map(i => i.team_id);

      if (event.shiftKey && this[lastKey] !== null) {
        const start = ids.indexOf(this[lastKey]);
        const end = ids.indexOf(id);
        if (start >= 0 && end >= 0) {
          const range = ids.slice(Math.min(start, end), Math.max(start, end) + 1);
          const merged = [...new Set([...sel, ...range])];
          sel.length = 0; sel.push(...merged);
        }
      } else if (event.ctrlKey || event.metaKey) {
        const idx = sel.indexOf(id);
        if (idx >= 0) sel.splice(idx, 1);
        else sel.push(id);
      } else {
        sel.length = 0; sel.push(id);
      }
      this[lastKey] = id;
    },

    async teamsMoveRight() {
      this.teamsMoving = true;
      for (const tid of this.teamsSelLeft) {
        try {
          await api.adminPut('/teams/' + tid + '/members/' + this.userId);
          this.teamsAssignedIds.push(tid);
        } catch {}
      }
      this.teamsSelLeft = [];
      this.teamsMoving = false;
    },

    async teamsMoveLeft() {
      this.teamsMoving = true;
      for (const tid of this.teamsSelRight) {
        try {
          await api.adminDelete('/teams/' + tid + '/members/' + this.userId);
          this.teamsAssignedIds = this.teamsAssignedIds.filter(id => id !== tid);
        } catch {}
      }
      this.teamsSelRight = [];
      this.teamsMoving = false;
    },

    async teamsMoveAllRight() {
      this.teamsMoving = true;
      for (const t of this.teamsAvailable) {
        try {
          await api.adminPut('/teams/' + t.team_id + '/members/' + this.userId);
          this.teamsAssignedIds.push(t.team_id);
        } catch {}
      }
      this.teamsSelLeft = [];
      this.teamsMoving = false;
    },

    async teamsMoveAllLeft() {
      this.teamsMoving = true;
      for (const t of this.teamsAssigned) {
        try {
          await api.adminDelete('/teams/' + t.team_id + '/members/' + this.userId);
        } catch {}
      }
      this.teamsAssignedIds = [];
      this.teamsSelRight = [];
      this.teamsMoving = false;
    },

    teamsDblClick(side, id) {
      if (side === 'left') {
        this.teamsSelLeft = [id];
        this.teamsMoveRight();
      } else {
        this.teamsSelRight = [id];
        this.teamsMoveLeft();
      }
    },

    teamsDragStart(side, id, event) {
      this.teamsDragData = { side, id };
      event.dataTransfer.effectAllowed = 'move';
    },

    teamsDrop(targetSide, event) {
      event.preventDefault();
      if (!this.teamsDragData) return;
      const { side, id } = this.teamsDragData;
      if (side !== targetSide) {
        if (side === 'left') { this.teamsSelLeft = [id]; this.teamsMoveRight(); }
        else { this.teamsSelRight = [id]; this.teamsMoveLeft(); }
      }
      this.teamsDragData = null;
    },

    // -- Grants shuttle --

    get grantsAvailable() {
      const ids = new Set(this.grantsAssignedIds);
      return this.grantsAll.filter(g => !ids.has(g.grant_id));
    },

    get grantsAssigned() {
      const ids = new Set(this.grantsAssignedIds);
      return this.grantsAll.filter(g => ids.has(g.grant_id));
    },

    grantsToggle(side, id, event) {
      const sel = side === 'left' ? this.grantsSelLeft : this.grantsSelRight;
      const items = side === 'left' ? this.grantsAvailable : this.grantsAssigned;
      const lastKey = side === 'left' ? 'grantsLastLeft' : 'grantsLastRight';
      const ids = items.map(i => i.grant_id);

      if (event.shiftKey && this[lastKey] !== null) {
        const start = ids.indexOf(this[lastKey]);
        const end = ids.indexOf(id);
        if (start >= 0 && end >= 0) {
          const range = ids.slice(Math.min(start, end), Math.max(start, end) + 1);
          const merged = [...new Set([...sel, ...range])];
          sel.length = 0; sel.push(...merged);
        }
      } else if (event.ctrlKey || event.metaKey) {
        const idx = sel.indexOf(id);
        if (idx >= 0) sel.splice(idx, 1);
        else sel.push(id);
      } else {
        sel.length = 0; sel.push(id);
      }
      this[lastKey] = id;
    },

    async grantsMoveRight() {
      this.grantsMoving = true;
      for (const gid of this.grantsSelLeft) {
        try {
          await api.adminPut('/users/' + this.userId + '/grants/' + gid);
          this.grantsAssignedIds.push(gid);
        } catch {}
      }
      this.grantsSelLeft = [];
      this.grantsMoving = false;
    },

    async grantsMoveLeft() {
      this.grantsMoving = true;
      for (const gid of this.grantsSelRight) {
        try {
          await api.adminDelete('/users/' + this.userId + '/grants/' + gid);
          this.grantsAssignedIds = this.grantsAssignedIds.filter(id => id !== gid);
        } catch {}
      }
      this.grantsSelRight = [];
      this.grantsMoving = false;
    },

    async grantsMoveAllRight() {
      this.grantsMoving = true;
      for (const g of this.grantsAvailable) {
        try {
          await api.adminPut('/users/' + this.userId + '/grants/' + g.grant_id);
          this.grantsAssignedIds.push(g.grant_id);
        } catch {}
      }
      this.grantsSelLeft = [];
      this.grantsMoving = false;
    },

    async grantsMoveAllLeft() {
      this.grantsMoving = true;
      for (const g of this.grantsAssigned) {
        try {
          await api.adminDelete('/users/' + this.userId + '/grants/' + g.grant_id);
        } catch {}
      }
      this.grantsAssignedIds = [];
      this.grantsSelRight = [];
      this.grantsMoving = false;
    },

    grantsDblClick(side, id) {
      if (side === 'left') {
        this.grantsSelLeft = [id];
        this.grantsMoveRight();
      } else {
        this.grantsSelRight = [id];
        this.grantsMoveLeft();
      }
    },

    grantsDragStart(side, id, event) {
      this.grantsDragData = { side, id };
      event.dataTransfer.effectAllowed = 'move';
    },

    grantsDrop(targetSide, event) {
      event.preventDefault();
      if (!this.grantsDragData) return;
      const { side, id } = this.grantsDragData;
      if (side !== targetSide) {
        if (side === 'left') { this.grantsSelLeft = [id]; this.grantsMoveRight(); }
        else { this.grantsSelRight = [id]; this.grantsMoveLeft(); }
      }
      this.grantsDragData = null;
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
