import { api } from '../api.js';

// ==================== SETTINGS VIEW ====================
export function settingsView() {
  return {
    settings: null,
    loading: true,

    async load() {
      this.loading = true;
      try {
        this.settings = await api.adminGet('/settings');
      } catch (e) {
        console.error('Failed to load settings:', e);
      }
      this.loading = false;
    },

    sourceLabel(source) {
      switch (source) {
        case 'config_file': return 'Config file';
        case 'database': return 'Custom';
        case 'default': return 'Default';
        default: return source;
      }
    },

    sourceClass(source) {
      switch (source) {
        case 'config_file': return 'bg-slate-500/20 text-slate-400';
        case 'database': return 'bg-vault-accent/20 text-vault-accent';
        case 'default': return 'bg-vault-border/30 text-vault-muted';
        default: return 'bg-vault-border/30 text-vault-muted';
      }
    },

    async saveSetting(key, value) {
      if (value === '' || value === undefined) return;
      // Skip if value hasn't actually changed
      if (this.settings[key] && value === this.settings[key].value) return;
      try {
        const resp = await api.adminPut('/settings/' + key, { value: String(value) });
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || `Error ${resp.status}`);
        }
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Save failed: ' + e.message, type: 'error' });
      }
    },

    async resetSetting(key) {
      try {
        const resp = await api.adminDelete('/settings/' + key);
        if (!resp.ok && resp.status !== 204) {
          const body = await resp.json();
          throw new Error(body.message || `Error ${resp.status}`);
        }
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Reset failed: ' + e.message, type: 'error' });
      }
    },
  };
}
