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
    members: [],
    grants: [],
    loading: true,
    activeTab: 'members',

    // Add member
    showAddMemberModal: false,
    allUsers: [],
    selectedUserId: '',
    addMemberError: '',

    // Remove member
    showRemoveMemberModal: false,
    removeMemberUser: null,
    removeMemberError: '',

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
      const match = hash.match(/^#\/teams\/([^/?]+)/);
      this.teamId = match ? decodeURIComponent(match[1]) : '';
      if (!this.teamId) { this.loading = false; return; }
      this.loading = true;
      try {
        const [team, members, grants] = await Promise.allSettled([
          api.adminGet('/teams/' + this.teamId),
          api.adminGet('/teams/' + this.teamId + '/members'),
          api.adminGet('/teams/' + this.teamId + '/grants'),
        ]);
        this.team = team.status === 'fulfilled' ? team.value : null;
        this.members = members.status === 'fulfilled' ? members.value : [];
        this.grants = grants.status === 'fulfilled' ? grants.value : [];
      } catch {
        this.team = null;
      }
      this.loading = false;
    },

    // -- Members --

    async openAddMember() {
      this.addMemberError = '';
      this.selectedUserId = '';
      try {
        this.allUsers = await api.adminGet('/users');
      } catch {
        this.allUsers = [];
      }
      this.showAddMemberModal = true;
    },

    get availableUsers() {
      const memberIds = new Set(this.members.map(m => m.user_id));
      return this.allUsers.filter(u => !memberIds.has(u.user_id));
    },

    async addMember() {
      this.addMemberError = '';
      if (!this.selectedUserId) return;
      try {
        const resp = await api.adminPut('/teams/' + this.teamId + '/members/' + this.selectedUserId);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showAddMemberModal = false;
        await this.reloadMembers();
      } catch (e) {
        this.addMemberError = e.message;
      }
    },

    confirmRemoveMember(member) {
      this.removeMemberUser = member;
      this.removeMemberError = '';
      this.showRemoveMemberModal = true;
    },

    async performRemoveMember() {
      this.removeMemberError = '';
      try {
        const resp = await api.adminDelete('/teams/' + this.teamId + '/members/' + this.removeMemberUser.user_id);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.error || body.message || `Error ${resp.status}`);
        }
        this.showRemoveMemberModal = false;
        this.removeMemberUser = null;
        await this.reloadMembers();
      } catch (e) {
        this.removeMemberError = e.message;
      }
    },

    async reloadMembers() {
      try {
        this.members = await api.adminGet('/teams/' + this.teamId + '/members');
      } catch {}
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
        const resp = await api.adminPut('/teams/' + this.teamId + '/grants/' + this.selectedGrantId);
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
        const resp = await api.adminDelete('/teams/' + this.teamId + '/grants/' + this.detachGrantId);
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
        this.grants = await api.adminGet('/teams/' + this.teamId + '/grants');
      } catch {}
    },

    // -- Helpers --

    formatDate(dateStr) {
      if (!dateStr) return '';
      return new Date(dateStr).toLocaleDateString();
    },
  };
}
