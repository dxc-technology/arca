import { api } from './api.js';
import { dashboardView } from './views/dashboard.js';
import { bucketsView, bucketSettingsView } from './views/buckets.js';
import { bucketDetailView } from './views/bucket-detail.js';
import { credentialsView } from './views/credentials.js';
import { usersView, userDetailView } from './views/users.js';
import { teamsView, teamDetailView } from './views/teams.js';
import { grantsView, grantDetailView } from './views/grants.js';
import { settingsView } from './views/settings.js';
import { auditView } from './views/audit.js';
import { monitoringView } from './views/monitoring.js';
import { notificationsView, bucketNotificationEditor } from './views/notifications.js';

// ==================== SHARED SVG ICONS ====================
// Centralized SVG strings for consistent use across views.
export const icons = {
  encryptionShield: '<svg class="w-3 h-3 text-green-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M9 12.75 11.25 15 15 9.75m-3-7.036A11.959 11.959 0 0 1 3.598 6 11.99 11.99 0 0 0 3 9.749c0 5.592 3.824 10.29 9 11.623 5.176-1.332 9-6.03 9-11.622 0-1.31-.21-2.571-.598-3.751h-.152c-3.196 0-6.1-1.248-8.25-3.285Z"/></svg>',
  versioningClock: '<svg class="w-3 h-3 text-blue-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M12 6v6h4.5m4.5 0a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z"/></svg>',
  versioningSuspended: '<svg class="w-3 h-3 text-amber-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M12 6v6h4.5m4.5 0a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z"/></svg>',
  objectLock: '<svg class="w-3 h-3 text-orange-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M16.5 10.5V6.75a4.5 4.5 0 1 0-9 0v3.75m-.75 11.25h10.5a2.25 2.25 0 0 0 2.25-2.25v-6.75a2.25 2.25 0 0 0-2.25-2.25H6.75a2.25 2.25 0 0 0-2.25 2.25v6.75a2.25 2.25 0 0 0 2.25 2.25Z"/></svg>',
};

// ==================== SHARED HELPERS ====================
export const ringColors = ['#00d4ff', '#6366f1', '#06b6d4', '#818cf8', '#22d3ee', '#a78bfa', '#67e8f9', '#c4b5fd'];

export function formatBytes(bytes) {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
}

export function formatUptime(seconds) {
  if (!seconds && seconds !== 0) return '\u2014';
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

// ==================== MAIN APP ====================
// Derive a default Arca endpoint from the console URL (same host, port 9000)
function guessArcaEndpoint() {
  const loc = window.location;
  const proto = loc.protocol;  // "http:" or "https:"
  const host = loc.hostname;
  return `${proto}//${host}:9000`;
}

export function app() {
  // Pre-compute injected endpoint before Alpine wraps the data
  const cfg = window.__ARCA_CONFIG__;
  const hasEndpoint = cfg && cfg.endpoint && !cfg.endpoint.startsWith('ARCA_INJECT_') && cfg.endpoint.trim() !== '';
  const hasSession = !!sessionStorage.getItem('arca_access_key');

  return {
    authenticated: hasSession,
    isAdmin: sessionStorage.getItem('arca_is_admin') === 'true',
    view: 'dashboard',
    sidebarOpen: false,
    currentBucket: '',
    currentPrefix: '',
    toast: '',
    toastType: 'success',
    toastTimeout: null,

    // Login
    loginForm: { endpoint: hasEndpoint ? cfg.endpoint : guessArcaEndpoint(), accessKey: '', secretKey: '' },
    loginLoading: false,
    loginError: '',
    showEndpointField: !hasEndpoint,

    init() {
      // Hash router
      this.handleRoute();
      window.addEventListener('hashchange', () => this.handleRoute());
    },

    handleRoute() {
      const hash = window.location.hash || '#/dashboard';
      const settingsMatch = hash.match(/^#\/buckets\/([^/?]+)\/settings$/);
      if (settingsMatch) {
        this.currentBucket = decodeURIComponent(settingsMatch[1]);
        this.view = 'bucket-settings';
      } else if (hash.startsWith('#/buckets/')) {
        this.currentBucket = decodeURIComponent(hash.slice('#/buckets/'.length).split('?')[0]);
        const prefixMatch = hash.match(/[?&]prefix=([^&]*)/);
        this.currentPrefix = prefixMatch ? decodeURIComponent(prefixMatch[1]) : '';
        this.view = 'bucket-detail';
      } else if (hash === '#/buckets') {
        this.view = 'buckets';
      } else if (hash === '#/credentials') {
        this.view = this.isAdmin ? 'credentials' : 'buckets';
      } else if (hash.startsWith('#/users/')) {
        this.view = this.isAdmin ? 'user-detail' : 'buckets';
      } else if (hash === '#/users') {
        this.view = this.isAdmin ? 'users' : 'buckets';
      } else if (hash.startsWith('#/teams/')) {
        this.view = this.isAdmin ? 'team-detail' : 'buckets';
      } else if (hash === '#/teams') {
        this.view = this.isAdmin ? 'teams' : 'buckets';
      } else if (hash.startsWith('#/grants/')) {
        this.view = this.isAdmin ? 'grant-detail' : 'buckets';
      } else if (hash === '#/grants') {
        this.view = this.isAdmin ? 'grants' : 'buckets';
      } else if (hash === '#/settings') {
        this.view = this.isAdmin ? 'settings' : 'buckets';
      } else if (hash === '#/audit') {
        this.view = this.isAdmin ? 'audit' : 'buckets';
      } else if (hash === '#/monitoring') {
        this.view = this.isAdmin ? 'monitoring' : 'buckets';
      } else if (hash === '#/notifications') {
        this.view = this.isAdmin ? 'notifications' : 'buckets';
      } else if (hash === '#/dashboard' || hash === '#/' || hash === '#') {
        this.view = this.isAdmin ? 'dashboard' : 'buckets';
      } else {
        this.view = this.isAdmin ? 'dashboard' : 'buckets';
      }
    },

    navigate(view, params = {}) {
      this.sidebarOpen = false;
      if (view === 'bucket-detail') {
        let hash = '#/buckets/' + encodeURIComponent(params.bucket);
        if (params.prefix) hash += '?prefix=' + encodeURIComponent(params.prefix);
        window.location.hash = hash;
      } else if (view === 'bucket-settings') {
        window.location.hash = '#/buckets/' + encodeURIComponent(params.bucket) + '/settings';
      } else {
        window.location.hash = '#/' + view;
      }
    },

    async login() {
      this.loginError = '';
      this.loginLoading = true;
      try {
        const endpoint = this.loginForm.endpoint.replace(/\/+$/, '');
        if (!endpoint) { this.loginError = 'Endpoint URL is required'; this.loginLoading = false; return; }
        if (!this.loginForm.accessKey || !this.loginForm.secretKey) { this.loginError = 'Credentials are required'; this.loginLoading = false; return; }

        sessionStorage.setItem('arca_endpoint', endpoint);
        sessionStorage.setItem('arca_access_key', this.loginForm.accessKey);
        sessionStorage.setItem('arca_secret_key', this.loginForm.secretKey);

        // Test connection: health (unauthenticated)
        const healthResp = await fetch(endpoint + '/admin/health', { timeout: 5000 });
        if (!healthResp.ok) throw new Error('Cannot reach Arca server');

        // Probe admin access first; fall back to S3 for non-admin users
        let admin = false;
        try {
          const info = await api.adminGet('/info');
          if (info.version) admin = true;
        } catch {
          // 403 = valid credentials but non-admin; verify via S3
          const buckets = await api.s3ListBuckets();
          if (!Array.isArray(buckets)) throw new Error('Invalid credentials');
        }

        this.isAdmin = admin;
        sessionStorage.setItem('arca_is_admin', admin ? 'true' : 'false');
        this.authenticated = true;
        window.location.hash = admin ? '#/dashboard' : '#/buckets';
      } catch (e) {
        sessionStorage.clear();
        this.loginError = e.message || 'Connection failed';
      }
      this.loginLoading = false;
    },

    logout() {
      sessionStorage.clear();
      this.authenticated = false;
      this.isAdmin = false;
      this.loginForm.accessKey = '';
      this.loginForm.secretKey = '';
      window.location.hash = '#/dashboard';
    },

    showToast(message, type = 'success') {
      this.toast = message;
      this.toastType = type;
      clearTimeout(this.toastTimeout);
      this.toastTimeout = setTimeout(() => { this.toast = ''; }, 3000);
    },

    copyToClipboard(text) {
      navigator.clipboard.writeText(text).then(() => this.showToast('Copied to clipboard'));
    },
  };
}

// ==================== ALPINE REGISTRATION ====================
document.addEventListener('alpine:init', () => {
  Alpine.data('app', app);
  Alpine.data('dashboardView', dashboardView);
  Alpine.data('bucketsView', bucketsView);
  Alpine.data('bucketSettingsView', bucketSettingsView);
  Alpine.data('bucketDetailView', bucketDetailView);
  Alpine.data('credentialsView', credentialsView);
  Alpine.data('usersView', usersView);
  Alpine.data('userDetailView', userDetailView);
  Alpine.data('teamsView', teamsView);
  Alpine.data('teamDetailView', teamDetailView);
  Alpine.data('grantsView', grantsView);
  Alpine.data('grantDetailView', grantDetailView);
  Alpine.data('settingsView', settingsView);
  Alpine.data('auditView', auditView);
  Alpine.data('monitoringView', monitoringView);
  Alpine.data('notificationsView', notificationsView);
  Alpine.data('bucketNotificationEditor', bucketNotificationEditor);
});
