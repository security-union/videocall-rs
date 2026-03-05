const UI_URL = process.env.UI_URL || "http://localhost:80";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";
const WS_URL = process.env.WS_CHECK_URL || "http://localhost:8080";

const MAX_WAIT_MS = 300_000;
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
    { name: "UI", url: UI_URL },
    { name: "Meeting API", url: `${API_URL}/session` },
    { name: "WebSocket API", url: WS_URL },
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
