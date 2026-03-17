import { api } from '../api.js';

export function teamsView() {
  return {
    teams: [],
    loading: true,
    showCreateModal: false,
    newName: '',
    newDescription: '',
    creating: false,
    createError: '',
    showDeleteModal: false,
    deleteTeam: null,
    deleteError: '',

    async load() {
      this.loading = true;
      try {
        this.teams = await api.adminGet('/teams');
      } catch {
        this.teams = [];
      }
      this.loading = false;
    },

    openTeam(teamId) {
      window.location.hash = '#/teams/' + encodeURIComponent(teamId);
    },

    async createTeam() {
      this.creating = true;
      this.createError = '';
      try {
        const resp = await api.adminPost('/teams', {
          name: this.newName,
          description: this.newDescription,
        });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showCreateModal = false;
        this.newName = '';
        this.newDescription = '';
        await this.load();
      } catch (e) {
        this.createError = e.message;
      }
      this.creating = false;
    },

    confirmDelete(team) {
      this.deleteTeam = team;
      this.deleteError = '';
      this.showDeleteModal = true;
    },

    async performDelete() {
      this.deleteError = '';
      try {
        const resp = await api.adminDelete('/teams/' + this.deleteTeam.team_id);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showDeleteModal = false;
        this.deleteTeam = null;
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

export function teamDetailView() {
  return {
    teamId: '',
    team: null,
    loading: true,
    activeTab: 'members',

    // Members shuttle
    membersAll: [],
    membersAssignedIds: [],
    membersSelLeft: [],
    membersSelRight: [],
    membersLastLeft: null,
    membersLastRight: null,
    membersDragData: null,
    membersMoving: false,

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
      const match = hash.match(/^#\/teams\/([^/?]+)/);
      this.teamId = match ? decodeURIComponent(match[1]) : '';
      if (!this.teamId) { this.loading = false; return; }
      this.loading = true;
      try {
        const [team, members, grants, allUsers, allGrants] = await Promise.allSettled([
          api.adminGet('/teams/' + this.teamId),
          api.adminGet('/teams/' + this.teamId + '/members'),
          api.adminGet('/teams/' + this.teamId + '/grants'),
          api.adminGet('/users'),
          api.adminGet('/grants'),
        ]);
        this.team = team.status === 'fulfilled' ? team.value : null;
        // Members shuttle
        this.membersAll = allUsers.status === 'fulfilled' ? allUsers.value : [];
        const membersList = members.status === 'fulfilled' ? members.value : [];
        this.membersAssignedIds = membersList.map(m => m.user_id);
        // Grants shuttle
        this.grantsAll = allGrants.status === 'fulfilled' ? allGrants.value : [];
        const grantsList = grants.status === 'fulfilled' ? grants.value : [];
        this.grantsAssignedIds = grantsList.map(g => g.grant_id);
      } catch {
        this.team = null;
      }
      this.loading = false;
    },

    // -- Members shuttle --

    get membersAvailable() {
      const ids = new Set(this.membersAssignedIds);
      return this.membersAll.filter(u => !ids.has(u.user_id));
    },

    get membersAssigned() {
      const ids = new Set(this.membersAssignedIds);
      return this.membersAll.filter(u => ids.has(u.user_id));
    },

    membersToggle(side, id, event) {
      const sel = side === 'left' ? this.membersSelLeft : this.membersSelRight;
      const items = side === 'left' ? this.membersAvailable : this.membersAssigned;
      const lastKey = side === 'left' ? 'membersLastLeft' : 'membersLastRight';
      const ids = items.map(i => i.user_id);

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

    async membersMoveRight() {
      this.membersMoving = true;
      for (const uid of this.membersSelLeft) {
        try {
          await api.adminPut('/teams/' + this.teamId + '/members/' + uid);
          this.membersAssignedIds.push(uid);
        } catch {}
      }
      this.membersSelLeft = [];
      this.membersMoving = false;
    },

    async membersMoveLeft() {
      this.membersMoving = true;
      for (const uid of this.membersSelRight) {
        try {
          await api.adminDelete('/teams/' + this.teamId + '/members/' + uid);
          this.membersAssignedIds = this.membersAssignedIds.filter(id => id !== uid);
        } catch {}
      }
      this.membersSelRight = [];
      this.membersMoving = false;
    },

    async membersMoveAllRight() {
      this.membersMoving = true;
      for (const u of this.membersAvailable) {
        try {
          await api.adminPut('/teams/' + this.teamId + '/members/' + u.user_id);
          this.membersAssignedIds.push(u.user_id);
        } catch {}
      }
      this.membersSelLeft = [];
      this.membersMoving = false;
    },

    async membersMoveAllLeft() {
      this.membersMoving = true;
      for (const u of this.membersAssigned) {
        try {
          await api.adminDelete('/teams/' + this.teamId + '/members/' + u.user_id);
        } catch {}
      }
      this.membersAssignedIds = [];
      this.membersSelRight = [];
      this.membersMoving = false;
    },

    membersDblClick(side, id) {
      if (side === 'left') {
        this.membersSelLeft = [id];
        this.membersMoveRight();
      } else {
        this.membersSelRight = [id];
        this.membersMoveLeft();
      }
    },

    membersDragStart(side, id, event) {
      this.membersDragData = { side, id };
      event.dataTransfer.effectAllowed = 'move';
    },

    membersDrop(targetSide, event) {
      event.preventDefault();
      if (!this.membersDragData) return;
      const { side, id } = this.membersDragData;
      if (side !== targetSide) {
        if (side === 'left') { this.membersSelLeft = [id]; this.membersMoveRight(); }
        else { this.membersSelRight = [id]; this.membersMoveLeft(); }
      }
      this.membersDragData = null;
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
          await api.adminPut('/teams/' + this.teamId + '/grants/' + gid);
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
          await api.adminDelete('/teams/' + this.teamId + '/grants/' + gid);
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
          await api.adminPut('/teams/' + this.teamId + '/grants/' + g.grant_id);
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
          await api.adminDelete('/teams/' + this.teamId + '/grants/' + g.grant_id);
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
  };
}
