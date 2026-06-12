import { api } from '../api.js';
import { formatBytes } from '../app.js';
import { nodeSelectorMixin } from '../node-selector.js?v=node-views-2';

// ==================== MONITORING VIEW ====================
export function monitoringView() {
  return {
    // Cluster node selector (R8): the metrics history is node-local. With
    // "All nodes" selected the charts draw one series per node.
    ...nodeSelectorMixin('monitoring'),

    snapshots: [],
    loading: true,
    timeRange: '24h',
    customFrom: '',
    customTo: '',
    datePickerOpen: false,
    formatBytes,

    init() {
      this.loadNodes();
      this.load();
    },

    async load() {
      this.loading = true;
      try {
        const { from, to } = this.dateRange();
        const params = new URLSearchParams();
        if (from) params.set('from', from);
        if (to) params.set('to', to);
        params.set('limit', '600');
        const data = await api.adminGet('/metrics/history?' + params.toString() + this.nodeQuery());
        // Reverse so oldest is first (for charting)
        this.snapshots = (data.snapshots || []).reverse();
        this.captureNodeMeta(data);
      } catch (e) {
        console.error('Failed to load metrics history:', e);
        this.snapshots = [];
        this.captureNodeMeta({});
        if (this.selectedNode) {
          this.$dispatch('show-toast', { message: this.nodeErrorMessage(e), type: 'error' });
        }
      }
      this.loading = false;
    },

    dateRange() {
      if (this.timeRange === 'custom') {
        return {
          from: this.customFrom ? new Date(this.customFrom).toISOString() : null,
          to: this.customTo ? new Date(this.customTo).toISOString() : null,
        };
      }
      const now = new Date();
      const ms = {
        '1h': 3600000,
        '6h': 6 * 3600000,
        '24h': 24 * 3600000,
        '2d': 2 * 86400000,
        '7d': 7 * 86400000,
        '30d': 30 * 86400000,
        '6m': 180 * 86400000,
        '1y': 365 * 86400000,
      };
      const offset = ms[this.timeRange];
      return {
        from: offset ? new Date(now - offset).toISOString() : null,
        to: null,
      };
    },

    setRange(range) {
      this.timeRange = range;
      if (range !== 'custom') {
        this.customFrom = '';
        this.customTo = '';
        this.datePickerOpen = false;
      }
      this.load();
    },

    openDatePicker() {
      this.datePickerOpen = !this.datePickerOpen;
      if (this.datePickerOpen) this.timeRange = 'custom';
    },

    applyCustomRange() {
      this.timeRange = 'custom';
      this.datePickerOpen = false;
      this.load();
    },

    clearCustomRange() {
      this.customFrom = '';
      this.customTo = '';
      this.datePickerOpen = false;
      this.setRange('24h');
    },

    // Compute the time span in hours for x-axis formatting
    _rangeHours() {
      if (this.snapshots.length < 2) return 1;
      const first = new Date(this.snapshots[0].timestamp).getTime();
      const last = new Date(this.snapshots[this.snapshots.length - 1].timestamp).getTime();
      return Math.max(1, (last - first) / 3600000);
    },

    // Format a value for Y-axis tick labels
    _formatYTick(value, key) {
      if (key === 'total_size_bytes') return formatBytes(value);
      if (value >= 1000000) return (value / 1000000).toFixed(1) + 'M';
      if (value >= 1000) return (value / 1000).toFixed(1) + 'K';
      return String(Math.round(value));
    },

    // Format a timestamp for X-axis tick labels, adapting to range span
    _formatXTick(ts) {
      const d = new Date(ts);
      const hours = this._rangeHours();
      if (hours <= 12) {
        // Short range: show time only
        return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
      }
      if (hours <= 72) {
        // Up to 3 days: show day + time
        return d.toLocaleDateString([], { month: 'short', day: 'numeric' }) +
          ' ' + d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
      }
      if (hours <= 24 * 90) {
        // Up to 3 months: show month + day
        return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
      }
      // Over 3 months: show month + year
      return d.toLocaleDateString([], { month: 'short', year: '2-digit' });
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
      // Ensure the last tick is >= max so the line stays inside the chart area
      if (ticks[ticks.length - 1] < max) {
        ticks.push(ticks[ticks.length - 1] + step);
      }
      // Ensure we don't have too many ticks
      return ticks.length > 6 ? ticks.filter((_, i) => i % 2 === 0) : ticks;
    },

    // One chart series per node when the merged view is active, otherwise the
    // single (whichever node answered) series. Each group keeps its own time
    // order; the merged rows interleave nodes, so they are split here.
    get nodeSeries() {
      if (!this.merged()) return [{ node: null, snapshots: this.snapshots }];
      const groups = new Map();
      for (const s of this.snapshots) {
        const k = s.node || '?';
        if (!groups.has(k)) groups.set(k, []);
        groups.get(k).push(s);
      }
      return [...groups.entries()].map(([node, snapshots]) => ({ node, snapshots }));
    },

    // Legend entries (merged view only): node id + its stable color.
    get legendNodes() {
      if (!this.merged()) return [];
      return this.nodeSeries
        .filter(s => s.node)
        .map(s => ({ node: s.node, color: this.nodeColor(s.node) }));
    },

    // SVG chart with axes. X is mapped by TIMESTAMP (not sample index) so
    // multiple per-node series share one honest time axis.
    sparkline(key, color) {
      const series = this.nodeSeries.filter(s => s.snapshots.length >= 2);
      if (series.length === 0) return '';

      const all = series.flatMap(s => s.snapshots);
      const times = all.map(s => new Date(s.timestamp).getTime());
      const tMin = Math.min(...times);
      const tSpan = Math.max(1, Math.max(...times) - tMin);
      const max = Math.max(...all.map(s => Number(s[key]) || 0), 1);

      // Chart dimensions with margins for labels
      const totalW = 600, totalH = 120;
      const ml = 55, mr = 10, mt = 8, mb = 22; // margins
      const w = totalW - ml - mr;
      const h = totalH - mt - mb;

      // Y-axis ticks
      const yTicks = this._yTicks(max);
      const yMax = yTicks[yTicks.length - 1] || max;

      const toPts = (snaps) => snaps.map(s => {
        const x = ml + ((new Date(s.timestamp).getTime() - tMin) / tSpan) * w;
        const y = mt + h - ((Number(s[key]) || 0) / yMax) * h;
        return `${x},${y}`;
      });

      // X-axis: ~5 evenly spaced time labels over the shared time domain.
      const xTickCount = Math.min(5, all.length);
      const xTicks = [];
      for (let i = 0; i < xTickCount; i++) {
        const frac = xTickCount === 1 ? 0 : i / (xTickCount - 1);
        xTicks.push({
          x: ml + frac * w,
          label: this._formatXTick(new Date(tMin + frac * tSpan).toISOString()),
        });
      }

      let svg = `<svg viewBox="0 0 ${totalW} ${totalH}" class="w-full" style="height:120px">`;

      // Y-axis grid lines and labels
      for (const tick of yTicks) {
        const y = mt + h - (tick / yMax) * h;
        svg += `<line x1="${ml}" y1="${y}" x2="${ml + w}" y2="${y}" stroke="#334155" stroke-width="0.5" stroke-dasharray="3,3"/>`;
        svg += `<text x="${ml - 6}" y="${y + 3.5}" text-anchor="end" fill="#94a3b8" font-size="9" font-family="JetBrains Mono, monospace">${this._formatYTick(tick, key)}</text>`;
      }

      // X-axis labels — anchor edge ticks against the chart boundary so the
      // first/last label can't overflow the card. Middle ticks stay centered
      // on their data position for consistency.
      for (let i = 0; i < xTicks.length; i++) {
        const tick = xTicks[i];
        let anchor = 'middle';
        if (i === 0) anchor = 'start';
        else if (i === xTicks.length - 1) anchor = 'end';
        svg += `<text x="${tick.x}" y="${mt + h + 14}" text-anchor="${anchor}" fill="#94a3b8" font-size="8.5" font-family="DM Sans, system-ui">${tick.label}</text>`;
      }

      // One line per series; the area fill only makes sense for a single
      // series (stacked tints would be unreadable).
      for (const s of series) {
        const lineColor = s.node ? this.nodeColor(s.node) : color;
        const pts = toPts(s.snapshots);
        if (series.length === 1) {
          const area = `${pts[0].split(',')[0]},${mt + h} ${pts.join(' ')} ${pts[pts.length - 1].split(',')[0]},${mt + h}`;
          svg += `<polygon points="${area}" fill="${lineColor}" opacity="0.12"/>`;
        }
        svg += `<polyline points="${pts.join(' ')}" fill="none" stroke="${lineColor}" stroke-width="1.5" stroke-linejoin="round"/>`;
      }

      svg += '</svg>';
      return svg;
    },

    latestValue(key) {
      if (this.snapshots.length === 0) return '—';
      return this.snapshots[this.snapshots.length - 1][key];
    },
  };
}
