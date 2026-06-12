import { api } from './api.js';

// ==================== CLUSTER NODE SELECTOR (R8, review D6) ====================
// Shared mixin for the four node-local admin views (audit log, monitoring,
// notification event log, replication journal). Behind a load balancer those
// stores answer with whichever node the LB picked, so each view offers a node
// selector: "This node (via LB)" (default — the responding node id is shown so
// the operator always knows what they are looking at), a specific eligible
// node (the server proxies the query to it over the cluster transport), or
// "All nodes" (rows from every eligible node merged newest-first, each labeled
// with its source node).
//
// Spread into a view's Alpine x-data object: `...nodeSelectorMixin('audit')`.
// The view must call `loadNodes()` in init, append `nodeQuery()` to its list
// request, and pass the response to `captureNodeMeta(data)`.

// Distinct, readable hues on the dark vault background. A node keeps ONE color
// everywhere (table badges, chart series, legend), assigned by its position in
// the /admin/cluster node list (self first, then peers — stable per session).
export const NODE_COLORS = [
  '#38bdf8', '#34d399', '#fbbf24', '#f472b6',
  '#a78bfa', '#fb7185', '#4ade80', '#60a5fa',
];

export function nodeSelectorMixin(viewKey) {
  const storageKey = viewKey + '_node';
  return {
    clusterEnabled: false,
    clusterNodes: [],
    selectedNode: sessionStorage.getItem(storageKey) || '',
    // Top-level `node` label of the last response: which node actually
    // answered (also set on the default LB path, where the LB decides).
    respondingNode: '',
    // Per-source report of the last merged ("All nodes") response.
    mergedSources: [],

    async loadNodes() {
      try {
        const c = await api.adminGet('/cluster');
        this.clusterEnabled = !!c.enabled;
        if (!c.enabled) return;
        this.clusterNodes = (c.nodes || []).map(n => ({
          node_id: n.node_id,
          local: !!n.local,
          // Only ELIGIBLE peers are valid proxy targets (alive +
          // authenticated + config-aligned), same gate as the server's.
          eligible: n.local || (n.alive && n.authenticated && n.config_ok),
        }));
        // A previously selected node may have left the cluster meanwhile:
        // fall back to the LB default instead of erroring on every load.
        if (this.selectedNode && this.selectedNode !== 'all'
            && !this.clusterNodes.some(n => n.node_id === this.selectedNode && n.eligible)) {
          this.selectedNode = '';
          sessionStorage.setItem(storageKey, '');
        }
      } catch {
        this.clusterEnabled = false;
      }
    },

    // Plain methods, NOT getters: the views spread this mixin into their
    // x-data object literal, and object spread copies a getter's VALUE at
    // spread time (freezing it forever), not the getter itself.
    selectableNodes() {
      return this.clusterNodes.filter(n => n.eligible);
    },

    // True when the merged all-nodes view is active (rows carry `node`).
    merged() {
      return this.selectedNode === 'all';
    },

    mergedErrors() {
      return this.mergedSources.filter(s => s.error);
    },

    setNode(v) {
      this.selectedNode = v;
      sessionStorage.setItem(storageKey, v);
      if (this.page !== undefined) this.page = 0;
      this.respondingNode = '';
      this.mergedSources = [];
      this.load();
    },

    // Appends the selector to a URLSearchParams-built query string.
    nodeQuery() {
      return this.selectedNode ? '&node=' + encodeURIComponent(this.selectedNode) : '';
    },

    captureNodeMeta(data) {
      this.respondingNode = data.node || '';
      this.mergedSources = data.sources || [];
    },

    shortNode(id) {
      return (id || '').slice(0, 8);
    },

    // Toast text for a failed node-selected load.
    nodeErrorMessage(e) {
      const target = this.selectedNode === 'all'
        ? 'the merged all-nodes view'
        : 'node ' + this.shortNode(this.selectedNode);
      return 'Failed to load ' + target + ': ' + e.message;
    },

    nodeColor(id) {
      const idx = this.clusterNodes.findIndex(n => n.node_id === id);
      return NODE_COLORS[(idx >= 0 ? idx : 0) % NODE_COLORS.length];
    },

    // Inline style for a node badge: tinted background + full-strength text in
    // the node's color ("RRGGBB" + "20" alpha suffix).
    nodeBadgeStyle(id) {
      const c = this.nodeColor(id);
      return `background:${c}20;color:${c}`;
    },
  };
}
