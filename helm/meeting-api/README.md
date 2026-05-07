# meeting-api Helm chart

Deploys the videocall meeting API (auth, OAuth, room tokens, participant lifecycle, console-log collection).

## Gotcha: `--set env[N]` replaces the entire env array

Helm's `--set env[N].name=X --set env[N].value=Y` on a list field **replaces** the
default `env` list from `values.yaml`, it does not merge. If a deploy workflow
sets `env[0]` through `env[14]` but omits any entry from the chart default, that
value falls back to whatever the container code defines internally.

The symptoms are silent: everything appears to deploy successfully, but the
missing env var's code default (which is often conservative / wrong for prod)
takes effect.

### Specifically: `TOKEN_TTL_SECS`

`TOKEN_TTL_SECS` controls room-access token lifetime. Setting it too short (e.g.
60 seconds) causes cached tokens in WebTransport / WebSocket URLs to expire
before a client's re-election can complete, stranding users with
"No valid connections with RTT measurements found".

**If your workflow uses `--set env[N]` overrides, you MUST include
`TOKEN_TTL_SECS` explicitly.** The chart default is `86400` (24 hours) and the
code default is also `86400`. Any reasonable deployment should pass a value
≥ 3600. See [discussion #562](https://github01.hclpnp.com/labs-projects/videocall/discussions/562).

## Values

See [`values.yaml`](values.yaml) for all options. Notable:

- `env` — list of environment variables passed to the container. `--set env[N]` overrides replace the entire list, so include everything you need.
- `consoleLogs.enabled` — mounts a PVC at `/data/console-logs` to capture browser console uploads. Incompatible with replicas > 1 (ReadWriteOnce PVC).
- `ingress.enabled` / `ingress.hosts` — standard ingress config.
- `resources` / `autoscaling` — standard.

## Related

- Application code: `meeting-api/` crate
- Env var reference: [`docs/MEETING_API.md`](../../docs/MEETING_API.md), [`docs/MEETING_OWNERSHIP.md`](../../docs/MEETING_OWNERSHIP.md)
