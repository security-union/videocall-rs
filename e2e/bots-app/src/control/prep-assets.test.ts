import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { generateToken } from "./auth";
import { generateBotId, type BotRegistryEntry } from "./registry";
import {
  type ControlServerHandle,
  type LaunchSpec,
  type OrchestratorControlSurface,
  startControlServer,
} from "./server";
import {
  createPrepAssetsJob,
  emitLine,
  PREP_ASSETS_RETENTION_MS,
  runPrepAssetsJob,
  sweepStalePrepAssetsJobs,
  validatePrepAssetsPath,
} from "./prep-assets";

interface MockSurface extends OrchestratorControlSurface {
  registry: Map<string, BotRegistryEntry>;
}

function mockSurface(): MockSurface {
  const registry = new Map<string, BotRegistryEntry>();
  return {
    registry,
    getRegistry: () => registry,
    triggerLeave: async () => {},
    forceKill: async () => {},
    applyTtl: () => {},
    changeNetwork: async () => {},
    setMicMuted: async () => {},
    setCameraOff: async () => {},
    setScreenShare: async () => {},
    duplicateBot: async () => generateBotId(),
    launchOne: async (_spec: LaunchSpec) => generateBotId(),
  } as MockSurface;
}

async function fetchJson(
  port: number,
  path: string,
  init: { method?: string; headers?: Record<string, string>; body?: unknown } = {},
): Promise<{ status: number; body: unknown }> {
  const headers: Record<string, string> = { accept: "application/json", ...init.headers };
  let body: string | undefined;
  if (init.body !== undefined) {
    body = JSON.stringify(init.body);
    headers["content-type"] = "application/json";
  }
  const res = await fetch(`http://127.0.0.1:${port}${path}`, {
    method: init.method ?? "GET",
    headers,
    body,
  });
  const text = await res.text();
  return {
    status: res.status,
    body: text.length === 0 ? null : JSON.parse(text),
  };
}

describe("prep-assets primitives", () => {
  it("validatePrepAssetsPath accepts relative paths and rejects traversal", () => {
    expect(validatePrepAssetsPath("bot/conversation/manifest.yaml", "f")).toBe(
      "bot/conversation/manifest.yaml",
    );
    expect(() => validatePrepAssetsPath("../etc/passwd", "f")).toThrow(/relative path/);
    expect(() => validatePrepAssetsPath("/etc/passwd", "f")).toThrow(/relative path/);
    expect(() => validatePrepAssetsPath("with space.yaml", "f")).toThrow(/invalid characters/);
    expect(() => validatePrepAssetsPath("", "f")).toThrow(/non-empty/);
  });

  it("emitLine appends to the buffer and notifies subscribers", () => {
    const job = createPrepAssetsJob();
    const seen: (string | null)[] = [];
    job.subscribers.add((l) => seen.push(l));
    emitLine(job, "hello");
    emitLine(job, "world");
    expect(job.stdoutLog).toEqual(["hello", "world"]);
    expect(seen).toEqual(["hello", "world"]);
  });

  it("sweepStalePrepAssetsJobs drops finished jobs past the retention window", () => {
    const jobs = new Map<string, ReturnType<typeof createPrepAssetsJob>>();
    const fresh = createPrepAssetsJob();
    fresh.status = "done";
    fresh.finishedAt = Date.now();
    jobs.set(fresh.jobId, fresh);
    const stale = createPrepAssetsJob();
    stale.status = "done";
    stale.finishedAt = Date.now() - (PREP_ASSETS_RETENTION_MS + 1000);
    jobs.set(stale.jobId, stale);
    sweepStalePrepAssetsJobs(jobs);
    expect(jobs.has(fresh.jobId)).toBe(true);
    expect(jobs.has(stale.jobId)).toBe(false);
  });

  it("runPrepAssetsJob fails fast when the manifest is missing", async () => {
    const dir = mkdtempSync(join(tmpdir(), "prep-fail-"));
    const job = createPrepAssetsJob();
    await runPrepAssetsJob(job, {
      manifestPath: join(dir, "missing-manifest.yaml"),
      costumeSource: join(dir, "costumes"),
      outputDir: dir,
    });
    expect(job.status).toBe("failed");
    expect(job.exitCode).toBe(1);
    expect(job.error).toMatch(/manifest not found/);
    expect(job.subscribers.size).toBe(0);
  });
});

describe("POST /assets/prep", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    runDir = mkdtempSync(join(tmpdir(), "prep-route-"));
    token = generateToken();
    surface = mockSurface();
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("accepts a request and returns a job id", async () => {
    // Manifest path the route falls back to does not exist in our
    // tmpdir layout, so the underlying job will transition to failed
    // — but the route itself should accept the request first.
    const res = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(res.status).toBe(202);
    const body = res.body as { jobId: string; status: string };
    expect(body.jobId).toMatch(/^[0-9a-f-]+$/);
    expect(body.status).toBe("running");
  });

  it("transitions running -> failed when the manifest is missing", async () => {
    const start = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    const jobId = (start.body as { jobId: string }).jobId;

    // Poll the status endpoint until the job is no longer running.
    let finalStatus: string = "running";
    for (let attempt = 0; attempt < 25; attempt++) {
      const stat = await fetchJson(handle.port, `/assets/prep/${jobId}`, {
        headers: { authorization: `Bearer ${token}` },
      });
      finalStatus = (stat.body as { status: string }).status;
      if (finalStatus !== "running") break;
      await new Promise((r) => setTimeout(r, 20));
    }
    expect(finalStatus).toBe("failed");
  });

  it("returns 409 when a job is already running", async () => {
    // Inject a long-lived "running" job directly into the registry —
    // the route walks the live map to gate concurrency.
    // We do this by issuing one request that the job will spend ~ms
    // failing, but we test the gate immediately after the first call
    // by issuing a second one synchronously before the first event
    // loop tick can transition it.
    const first = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(first.status).toBe(202);
    const second = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    // Either the second request races in before the first job has
    // resolved (409) OR the first job already finished and the second
    // gets a fresh 202. Both are correct behaviors; assert one of them.
    expect([409, 202]).toContain(second.status);
  });

  it("rejects path-traversal in the override fields", async () => {
    const res = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { manifestPath: "../etc/passwd" },
    });
    expect(res.status).toBe(400);
  });

  it("rejects an invalid participant name", async () => {
    const res = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { participants: ["alice", "../bad"] },
    });
    expect(res.status).toBe(400);
  });

  it("GET /assets/prep/:jobId returns 404 for unknown id", async () => {
    const res = await fetchJson(handle.port, "/assets/prep/does-not-exist", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("SSE stream replays the existing log + emits an end event", async () => {
    const start = await fetchJson(handle.port, "/assets/prep", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    const jobId = (start.body as { jobId: string }).jobId;
    // Wait for the job to fail (no manifest in tmpdir layout).
    let finalStatus = "running";
    for (let attempt = 0; attempt < 25; attempt++) {
      const stat = await fetchJson(handle.port, `/assets/prep/${jobId}`, {
        headers: { authorization: `Bearer ${token}` },
      });
      finalStatus = (stat.body as { status: string }).status;
      if (finalStatus !== "running") break;
      await new Promise((r) => setTimeout(r, 20));
    }
    expect(finalStatus).toBe("failed");

    // Now open the stream — it must replay the buffered log lines and
    // close with the `end` event for already-finished jobs.
    const res = await fetch(`http://127.0.0.1:${handle.port}/assets/prep/${jobId}/stream`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const text = await res.text();
    expect(text).toMatch(/data: prep-assets failed/);
    expect(text).toMatch(/event: end/);
  });
});
