import { api } from './api.js';
import { topicFor } from './help.js';
import { dashboardView } from './views/dashboard.js';
import { bucketsView, bucketSettingsView } from './views/buckets.js';
import { bucketDetailView } from './views/bucket-detail.js?v=slideshow-nav-1';
import { credentialsView } from './views/credentials.js';
import { usersView, userDetailView } from './views/users.js';
import { teamsView, teamDetailView } from './views/teams.js';
import { grantsView, grantDetailView } from './views/grants.js';
import { settingsView } from './views/settings.js';
import { auditView } from './views/audit.js';
import { monitoringView } from './views/monitoring.js';
import { notificationsView, bucketNotificationEditor } from './views/notifications.js';
import { replicationView, bucketReplicationEditor, replicationCredentials } from './views/replication.js?v=repl-creds-5';

// ==================== SHARED SVG ICONS ====================
// Centralized SVG strings for consistent use across views.
export const icons = {
  encryptionShield: '<svg class="w-3 h-3 text-green-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M9 12.75 11.25 15 15 9.75m-3-7.036A11.959 11.959 0 0 1 3.598 6 11.99 11.99 0 0 0 3 9.749c0 5.592 3.824 10.29 9 11.623 5.176-1.332 9-6.03 9-11.622 0-1.31-.21-2.571-.598-3.751h-.152c-3.196 0-6.1-1.248-8.25-3.285Z"/></svg>',
  versioningClock: '<svg class="w-3 h-3 text-blue-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M12 6v6h4.5m4.5 0a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z"/></svg>',
  versioningSuspended: '<svg class="w-3 h-3 text-amber-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M12 6v6h4.5m4.5 0a9 9 0 1 1-18 0 9 9 0 0 1 18 0Z"/></svg>',
  objectLock: '<svg class="w-3 h-3 text-orange-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M16.5 10.5V6.75a4.5 4.5 0 1 0-9 0v3.75m-.75 11.25h10.5a2.25 2.25 0 0 0 2.25-2.25v-6.75a2.25 2.25 0 0 0-2.25-2.25H6.75a2.25 2.25 0 0 0-2.25 2.25v6.75a2.25 2.25 0 0 0 2.25 2.25Z"/></svg>',
  compression: '<svg class="w-3 h-3 text-cyan-400 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" d="M20.25 6.375c0 2.278-3.694 4.125-8.25 4.125S3.75 8.653 3.75 6.375m16.5 0c0-2.278-3.694-4.125-8.25-4.125S3.75 4.097 3.75 6.375m16.5 0v11.25c0 2.278-3.694 4.125-8.25 4.125S3.75 19.903 3.75 17.625V6.375m16.5 0v3.75m-16.5-3.75v3.75m16.5 0v3.75C20.25 16.153 16.556 18 12 18s-8.25-1.847-8.25-4.125v-3.75m16.5 0c0 2.278-3.694 4.125-8.25 4.125s-8.25-1.847-8.25-4.125"/></svg>',
};

// ==================== SHARED HELPERS ====================
export const ringColors = ['#00d4ff', '#6366f1', '#06b6d4', '#818cf8', '#22d3ee', '#a78bfa', '#67e8f9', '#c4b5fd'];

export function formatBytes(bytes) {
  if (bytes == null || bytes <= 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  if (i < 0 || i >= sizes.length) return bytes + ' B';
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
    username: sessionStorage.getItem('arca_username') || '',
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
      } else if (hash === '#/replication') {
        this.view = this.isAdmin ? 'replication' : 'buckets';
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

        // Probe identity + admin access via /admin/me (admin-only, same grant
        // as /admin/info). For non-admin users this 403s and we fall back to
        // S3 to validate the credentials; the username stays empty in that
        // case and the sidebar falls back to showing the access key.
        let admin = false;
        let username = '';
        try {
          const me = await api.adminGet('/me');
          username = me?.user?.username || '';
          admin = true;
        } catch {
          const buckets = await api.s3ListBuckets();
          if (!Array.isArray(buckets)) throw new Error('Invalid credentials');
        }

        this.isAdmin = admin;
        this.username = username;
        sessionStorage.setItem('arca_is_admin', admin ? 'true' : 'false');
        if (username) sessionStorage.setItem('arca_username', username);
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
      this.username = '';
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

// ==================== INLINE HELP ====================
//
// helpTrigger(id)  — drives each "?" button. Shows a hover tooltip with the
//                    topic's short hint; clicks dispatch an `arca:open-help`
//                    event that the single global helpModal listens for.
// helpModal()      — the single overlay that renders the full topic body.

export function helpTrigger(topicId) {
  return {
    topicId,
    hovered: false,
    get topic() { return topicFor(this.topicId); },
    show() {
      window.dispatchEvent(new CustomEvent('arca:open-help', { detail: this.topicId }));
    },
  };
}

export function helpModal() {
  return {
    open: false,
    current: null,
    get topic() { return this.current ? topicFor(this.current) : null; },
    mount() {
      window.addEventListener('arca:open-help', (e) => {
        this.current = e.detail;
        this.open = true;
      });
      window.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') this.open = false;
      });
    },
    close() { this.open = false; },
  };
}

// ==================== PASSWORD TOGGLE DIRECTIVE ====================
//
// `x-password-toggle` on an <input type="password"> wraps the input in a
// relative container and appends an eye / eye-slash button that switches
// the input's `type` attribute between `password` and `text`. One-line
// ergonomics at every call site; the heavy lifting is here.

const EYE_SVG = '<svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" aria-hidden="true"><path stroke-linecap="round" stroke-linejoin="round" d="M2.036 12.322a1.012 1.012 0 0 1 0-.639C3.423 7.51 7.36 4.5 12 4.5c4.638 0 8.573 3.007 9.963 7.178.07.207.07.431 0 .639C20.577 16.49 16.64 19.5 12 19.5c-4.638 0-8.573-3.007-9.963-7.178Z"/><path stroke-linecap="round" stroke-linejoin="round" d="M15 12a3 3 0 1 1-6 0 3 3 0 0 1 6 0Z"/></svg>';
const EYE_SLASH_SVG = '<svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" aria-hidden="true"><path stroke-linecap="round" stroke-linejoin="round" d="M3.98 8.223A10.477 10.477 0 0 0 1.934 12C3.226 16.338 7.244 19.5 12 19.5c.993 0 1.953-.138 2.863-.395M6.228 6.228A10.451 10.451 0 0 1 12 4.5c4.756 0 8.773 3.162 10.065 7.498a10.522 10.522 0 0 1-4.293 5.774M6.228 6.228 3 3m3.228 3.228 3.65 3.65m7.894 7.894L21 21m-3.228-3.228-3.65-3.65m0 0a3 3 0 1 0-4.243-4.243m4.242 4.242L9.88 9.88"/></svg>';

function initPasswordToggle(el) {
  if (el.dataset.pwToggleInitialized === '1') return;
  el.dataset.pwToggleInitialized = '1';

  // Wrap input in a relative container without changing scope.
  const wrapper = document.createElement('div');
  wrapper.className = 'relative';
  el.parentNode.insertBefore(wrapper, el);
  wrapper.appendChild(el);

  // Reserve room on the right so the text doesn't slide under the button.
  if (!el.classList.contains('pr-9')) el.classList.add('pr-9');

  const btn = document.createElement('button');
  btn.type = 'button';
  btn.tabIndex = -1;
  btn.className = 'absolute inset-y-0 right-0 flex items-center pr-2.5 text-vault-muted hover:text-vault-accent transition';

  let shown = false;
  const render = () => {
    el.type = shown ? 'text' : 'password';
    btn.innerHTML = shown ? EYE_SLASH_SVG : EYE_SVG;
    const label = shown ? 'Hide' : 'Show';
    btn.title = label;
    btn.setAttribute('aria-label', shown ? 'Hide password' : 'Show password');
    btn.setAttribute('aria-pressed', String(shown));
  };
  btn.addEventListener('click', (e) => {
    e.preventDefault();
    shown = !shown;
    render();
  });
  render();
  wrapper.appendChild(btn);
}

// ==================== ALPINE REGISTRATION ====================
document.addEventListener('alpine:init', () => {
  Alpine.directive('password-toggle', (el) => initPasswordToggle(el));

  Alpine.data('app', app);
  Alpine.data('helpTrigger', helpTrigger);
  Alpine.data('helpModal', helpModal);
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
  Alpine.data('replicationView', replicationView);
  Alpine.data('bucketReplicationEditor', bucketReplicationEditor);
  Alpine.data('replicationCredentials', replicationCredentials);
});
