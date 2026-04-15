import { api } from '../api.js';

// ==================== SETTINGS VIEW ====================
export function settingsView() {
  return {
    settings: null,
    loading: true,

    // Export state
    showExportModal: false,
    exportBusy: false,
    exportIncludeSecrets: false,
    exportSections: { settings: true, auth: true, credentials: true, buckets: true, bucket_configs: true },

    // Import state
    showImportModal: false,
    importBusy: false,
    importFile: null,
    importData: null,
    importPreview: null,
    importMode: 'skip',
    importResults: null,

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

    // ── Export ──

    async doExport() {
      this.exportBusy = true;
      try {
        const selected = Object.entries(this.exportSections)
          .filter(([, v]) => v)
          .map(([k]) => k);
        if (selected.length === 0) return;

        let path = '/export?sections=' + encodeURIComponent(selected.join(','));
        if (this.exportIncludeSecrets) path += '&include_secrets=true';

        const data = await api.adminGet(path);

        const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        const date = new Date().toISOString().slice(0, 10);
        a.download = `arca-export-${date}.json`;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        URL.revokeObjectURL(url);

        this.showExportModal = false;
        this.$dispatch('show-toast', { message: 'Configuration exported', type: 'success' });
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Export failed: ' + e.message, type: 'error' });
      }
      this.exportBusy = false;
    },

    // ── Import ──

    handleImportDrop(event) {
      const file = event.dataTransfer?.files?.[0];
      if (file) this.handleImportFile(file);
    },

    async handleImportFile(file) {
      if (!file || !file.name.endsWith('.json')) {
        this.$dispatch('show-toast', { message: 'Please select a JSON file', type: 'error' });
        return;
      }
      try {
        const text = await file.text();
        const data = JSON.parse(text);
        this.importFile = { name: file.name };
        this.importData = data;

        // Build preview
        const preview = {};
        if (data.arca_export?.version) preview.version = data.arca_export.version;
        if (data.settings) preview.settings = Object.keys(data.settings).length;
        if (data.users) preview.users = data.users.length;
        if (data.teams) preview.teams = data.teams.length;
        if (data.grants) preview.grants = data.grants.length;
        if (data.credentials) preview.credentials = data.credentials.length;
        if (data.buckets) preview.buckets = data.buckets.length;
        if (data.bucket_configs) preview.bucket_configs = Object.keys(data.bucket_configs).length;
        this.importPreview = preview;
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Invalid JSON file: ' + e.message, type: 'error' });
      }
    },

    async doImport() {
      if (!this.importData) return;
      this.importBusy = true;
      try {
        const resp = await api.adminPost('/import?mode=' + encodeURIComponent(this.importMode), this.importData);
        if (!resp.ok) {
          const body = await resp.json();
          throw new Error(body.message || `Error ${resp.status}`);
        }
        this.importResults = await resp.json();
        // Reload settings in case they changed
        await this.load();
      } catch (e) {
        this.$dispatch('show-toast', { message: 'Import failed: ' + e.message, type: 'error' });
      }
      this.importBusy = false;
    },

    closeImportModal() {
      this.showImportModal = false;
      this.importFile = null;
      this.importData = null;
      this.importPreview = null;
      this.importResults = null;
      this.importMode = 'skip';
    },
  };
}
