import { api } from '../api.js';
import { formatBytes, formatUptime, ringColors } from '../app.js';

// ==================== DASHBOARD VIEW ====================
export function dashboardView() {
  return {
    health: 'unknown',
    info: {},
    stats: {},
    bucketSizes: [],
    ringSegments: [],
    ringColors,
    refreshInterval: null,

    async load() {
      await this.refresh();
      this.refreshInterval = setInterval(() => this.refresh(), 30000);
    },

    async refresh() {
      try {
        // Health (unauthenticated)
        const endpoint = sessionStorage.getItem('arca_endpoint');
        const hResp = await fetch(endpoint + '/admin/health');
        const hBody = await hResp.json();
        this.health = hBody.status || 'error';
      } catch { this.health = 'error'; }

      try { this.info = await api.adminGet('/info'); } catch {}
      try { this.stats = await api.adminGet('/stats'); } catch {}

      // Get per-bucket sizes via ListBuckets + ListObjects
      try {
        const buckets = await api.s3ListBuckets();
        const sizes = [];
        for (const b of buckets) {
          try {
            // Get all objects to sum sizes
            let totalSize = 0;
            let token = '';
            do {
              const params = { 'list-type': '2' };
              if (token) params['continuation-token'] = token;
              const resp = await api.request('GET', '/' + encodeURIComponent(b.name), { queryParams: params });
              const xml = await resp.text();
              const result = api.parseListObjects(xml);
              for (const obj of result.objects) totalSize += obj.size;
              token = result.isTruncated ? result.nextToken : '';
            } while (token);
            sizes.push({ name: b.name, size: totalSize, encrypted: false });
          } catch { sizes.push({ name: b.name, size: 0, encrypted: false }); }
        }
        // Load encryption status in parallel
        await Promise.all(sizes.map(async (s) => {
          const enc = await api.s3GetBucketEncryption(s.name);
          s.encrypted = !!(enc && enc.algorithm);
        }));
        this.bucketSizes = sizes;
        this.computeRing(sizes);
      } catch {}
    },

    computeRing(sizes) {
      const total = sizes.reduce((s, b) => s + b.size, 0);
      if (total === 0) { this.ringSegments = []; return; }

      const circumference = 2 * Math.PI * 70; // r=70
      let offset = 0;
      this.ringSegments = sizes.filter(b => b.size > 0).map((b, i) => {
        const fraction = b.size / total;
        const dash = fraction * circumference;
        const gap = circumference - dash;
        const seg = {
          color: this.ringColors[i % this.ringColors.length],
          dash: `${dash} ${gap}`,
          offset: -offset,
        };
        offset += dash;
        return seg;
      });
    },

    renderRing() {
      let circles = '';
      if (this.ringSegments.length === 0) {
        circles = '<circle cx="100" cy="100" r="70" fill="none" stroke="#334155" stroke-width="24" opacity="0.3"/>';
      } else {
        for (const seg of this.ringSegments) {
          circles += `<circle cx="100" cy="100" r="70" fill="none" stroke="${seg.color}" stroke-width="24" stroke-dasharray="${seg.dash}" stroke-dashoffset="${seg.offset}" transform="rotate(-90 100 100)" class="ring-segment" stroke-linecap="round"/>`;
        }
      }
      const totalLabel = formatBytes(this.stats.total_size_bytes || 0);
      return `<svg viewBox="0 0 200 200" class="w-48 h-48">${circles}<text x="100" y="95" text-anchor="middle" fill="#e2e8f0" font-size="16" font-weight="600" font-family="DM Sans, system-ui">${totalLabel}</text><text x="100" y="115" text-anchor="middle" fill="#94a3b8" font-size="10" font-family="DM Sans, system-ui">Total</text></svg>`;
    },

    formatBytes,
    formatUptime,

    destroy() { clearInterval(this.refreshInterval); },
  };
}
