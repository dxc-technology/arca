import { api } from '../api.js';
import { formatBytes } from '../app.js';

// ==================== MONITORING VIEW ====================
export function monitoringView() {
  return {
    snapshots: [],
    loading: true,
    timeRange: '24h',
    formatBytes,

    async load() {
      this.loading = true;
      try {
        const from = this.fromDate();
        const params = new URLSearchParams();
        if (from) params.set('from', from);
        params.set('limit', '500');
        const data = await api.adminGet('/metrics/history?' + params.toString());
        // Reverse so oldest is first (for charting)
        this.snapshots = (data.snapshots || []).reverse();
      } catch (e) {
        console.error('Failed to load metrics history:', e);
      }
      this.loading = false;
    },

    fromDate() {
      const now = new Date();
      switch (this.timeRange) {
        case '1h': return new Date(now - 3600000).toISOString();
        case '6h': return new Date(now - 6 * 3600000).toISOString();
        case '24h': return new Date(now - 24 * 3600000).toISOString();
        case '7d': return new Date(now - 7 * 86400000).toISOString();
        case '30d': return new Date(now - 30 * 86400000).toISOString();
        default: return null;
      }
    },

    setRange(range) {
      this.timeRange = range;
      this.load();
    },

    // Format a value for Y-axis tick labels
    _formatYTick(value, key) {
      if (key === 'total_size_bytes') return formatBytes(value);
      if (value >= 1000000) return (value / 1000000).toFixed(1) + 'M';
      if (value >= 1000) return (value / 1000).toFixed(1) + 'K';
      return String(Math.round(value));
    },

    // Format a timestamp for X-axis tick labels
    _formatXTick(ts) {
      const d = new Date(ts);
      if (this.timeRange === '1h' || this.timeRange === '6h') {
        return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
      }
      if (this.timeRange === '24h') {
        return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
      }
      return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
    },

    // Choose nice Y-axis tick values
    _yTicks(max) {
      if (max <= 0) return [0];
      const count = 4;
      // Round step up to a nice number
      const rawStep = max / count;
      const mag = Math.pow(10, Math.floor(Math.log10(rawStep)));
      const nice = [1, 2, 2.5, 5, 10];
      let step = mag;
      for (const n of nice) {
        if (n * mag >= rawStep) { step = n * mag; break; }
      }
      const ticks = [];
      for (let v = 0; v <= max + step * 0.01; v += step) {
        ticks.push(v);
      }
      // Ensure we don't have too many ticks
      return ticks.length > 6 ? ticks.filter((_, i) => i % 2 === 0) : ticks;
    },

    // SVG chart with axes
    sparkline(key, color) {
      if (this.snapshots.length < 2) return '';

      const values = this.snapshots.map(s => Number(s[key]) || 0);
      const timestamps = this.snapshots.map(s => s.timestamp);
      const max = Math.max(...values, 1);

      // Chart dimensions with margins for labels
      const totalW = 600, totalH = 120;
      const ml = 55, mr = 10, mt = 8, mb = 22; // margins
      const w = totalW - ml - mr;
      const h = totalH - mt - mb;

      // Y-axis ticks
      const yTicks = this._yTicks(max);
      const yMax = yTicks[yTicks.length - 1] || max;

      // Data points mapped to chart area
      const pts = values.map((v, i) => {
        const x = ml + (i / (values.length - 1)) * w;
        const y = mt + h - (v / yMax) * h;
        return { x, y };
      });
      const polyPoints = pts.map(p => `${p.x},${p.y}`).join(' ');
      const areaPoints = `${pts[0].x},${mt + h} ${polyPoints} ${pts[pts.length - 1].x},${mt + h}`;

      // X-axis: pick ~5 evenly spaced time labels
      const xTickCount = Math.min(5, this.snapshots.length);
      const xTicks = [];
      for (let i = 0; i < xTickCount; i++) {
        const idx = Math.round(i * (this.snapshots.length - 1) / (xTickCount - 1));
        xTicks.push({
          x: ml + (idx / (values.length - 1)) * w,
          label: this._formatXTick(timestamps[idx]),
        });
      }

      let svg = `<svg viewBox="0 0 ${totalW} ${totalH}" class="w-full" style="height:120px">`;

      // Y-axis grid lines and labels
      for (const tick of yTicks) {
        const y = mt + h - (tick / yMax) * h;
        svg += `<line x1="${ml}" y1="${y}" x2="${ml + w}" y2="${y}" stroke="#334155" stroke-width="0.5" stroke-dasharray="3,3"/>`;
        svg += `<text x="${ml - 6}" y="${y + 3.5}" text-anchor="end" fill="#94a3b8" font-size="9" font-family="JetBrains Mono, monospace">${this._formatYTick(tick, key)}</text>`;
      }

      // X-axis labels
      for (const tick of xTicks) {
        svg += `<text x="${tick.x}" y="${mt + h + 14}" text-anchor="middle" fill="#94a3b8" font-size="8.5" font-family="DM Sans, system-ui">${tick.label}</text>`;
      }

      // Area fill + line
      svg += `<polygon points="${areaPoints}" fill="${color}" opacity="0.12"/>`;
      svg += `<polyline points="${polyPoints}" fill="none" stroke="${color}" stroke-width="1.5" stroke-linejoin="round"/>`;

      svg += '</svg>';
      return svg;
    },

    latestValue(key) {
      if (this.snapshots.length === 0) return '—';
      return this.snapshots[this.snapshots.length - 1][key];
    },
  };
}
