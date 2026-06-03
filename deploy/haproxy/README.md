# Bare-metal HA: HAProxy + keepalived in front of an Arca cluster

This directory provides a production-grade, highly-available front end for a
bare-metal / VM [Arca cluster](../../documentation/docs/guide/ha.md): two HAProxy
load balancers sharing a floating **Virtual IP (VIP)** via keepalived, so clients
have a single endpoint that survives the loss of any one node *and* either load
balancer.

```
                         VIP 10.0.0.10:9000
                                │
                 ┌──────────────┴──────────────┐
            HAProxy (MASTER)              HAProxy (BACKUP)
            10.0.0.21                     10.0.0.22
                 └──────────────┬──────────────┘
            ┌───────────────────┼───────────────────┐
        arca-1               arca-2               arca-3
       10.0.0.11            10.0.0.12            10.0.0.13
```

## Files

| File | Where it goes | Purpose |
|------|---------------|---------|
| [`haproxy.cfg`](haproxy.cfg) | `/etc/haproxy/haproxy.cfg` (both LBs) | Round-robin over the live nodes, health-checked via `GET /admin/health` |
| [`keepalived.conf`](keepalived.conf) | `/etc/keepalived/keepalived.conf` (both LBs) | VRRP floating VIP; demotes a host whose HAProxy is down |
| [`../config/arca-cluster.toml`](../config/arca-cluster.toml) | `/etc/arca/config.toml` (each node) | Identical cluster config for the Arca nodes |
| [`../systemd/arca.service`](../systemd/arca.service) | `/etc/systemd/system/arca.service` (each node) | Runs Arca on each node |

## Setup

1. **Nodes** — on each of the 3 Arca hosts, install the binary, drop
   `arca-cluster.toml` at `/etc/arca/config.toml` (edit `secret`, `seeds`, and
   `cluster_id` — identical on every node), and start it with the systemd unit.
   Confirm they form a cluster: `curl -s http://<node>:9000/admin/health?verbose=1`.

2. **Load balancers** — on each of the 2 LB hosts, install HAProxy + keepalived,
   drop in `haproxy.cfg` (set the three `server` addresses) and `keepalived.conf`
   (set `state`/`priority` per host, the `interface`, the VIP, and a shared
   `auth_pass`), then `systemctl enable --now haproxy keepalived`.

3. **Point clients at the VIP**, e.g.
   `aws s3 ls --endpoint-url http://10.0.0.10:9000`.

## Notes

- **Read-only under quorum loss is intended.** A node that is up but has lost
  write quorum still answers `200` on `/admin/health`, so it stays in rotation
  and serves reads; writes return `503` from the node. Only *draining* nodes go
  `503` on health and leave rotation.
- **TLS** can terminate at HAProxy (`bind ... ssl crt`) or end-to-end at Arca
  (use HAProxy `mode tcp` passthrough). See the comments in `haproxy.cfg`.
- **Encryption**: every node must share the same master key / KMS, or replicated
  encrypted blobs will not be byte-identical. See `arca-cluster.toml`.
