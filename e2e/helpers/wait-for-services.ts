const DIOXUS_UI_URL = process.env.DIOXUS_UI_URL || "http://localhost:3001";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";
const WS_URL = process.env.WS_CHECK_URL || "http://localhost:8080";
// WebTransport health endpoint (TCP) — bound by the `webtransport_server`
// binary from `HEALTH_LISTEN_URL` (see
// `actix-api/src/bin/webtransport_server.rs`). The Playwright stack publishes
// 5321 on the host so we can probe readiness here; the actual WT traffic
// runs on UDP 4433 which has no portable readiness check. If the health
// listener is up, the QUIC listener is up — both are spawned in the same
// process.
const WT_HEALTH_URL = process.env.WT_HEALTH_URL || "http://localhost:5321/healthz";

const MAX_WAIT_MS = 600_000;
const POLL_INTERVAL_MS = 2_000;

async function probe(url: string): Promise<boolean> {
  try {
    const resp = await fetch(url, { signal: AbortSignal.timeout(3_000) });
    // Any HTTP response means the service is up (401, 404, etc. are fine)
    return resp.status > 0;
  } catch {
    return false;
  }
}

export async function waitForServices(): Promise<void> {
  const services = [
    { name: "Dioxus UI", url: DIOXUS_UI_URL },
    { name: "Meeting API", url: `${API_URL}/session` },
    { name: "WebSocket API", url: WS_URL },
    // WebTransport readiness — keep this last because it is the slowest to
    // come up (debug build of `webtransport_server` + QUIC bind). Specs that
    // force `vc_transport_preference=webtransport` rely on this being ready
    // before they navigate; without it the WT handshake silently fails and
    // every `canvas-container` assertion times out (see RCA in
    // `e2e/tests/wt-persistent-streams-freeze-regression.spec.ts`).
    { name: "WebTransport API", url: WT_HEALTH_URL },
  ];

  const deadline = Date.now() + MAX_WAIT_MS;

  for (const svc of services) {
    console.log(`Waiting for ${svc.name} at ${svc.url}...`);
    let attempts = 0;
    while (Date.now() < deadline) {
      if (await probe(svc.url)) {
        console.log(`${svc.name} ready after ${attempts} attempts`);
        break;
      }
      attempts++;
      if (attempts % 5 === 0) {
        const elapsed = Math.round((MAX_WAIT_MS - (deadline - Date.now())) / 1000);
        console.log(`  still waiting for ${svc.name}... (${elapsed}s elapsed)`);
      }
      await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
    }
    if (Date.now() >= deadline) {
      throw new Error(
        `Timed out waiting for ${svc.name} at ${svc.url} after ${MAX_WAIT_MS / 1000}s`,
      );
    }
  }
}
