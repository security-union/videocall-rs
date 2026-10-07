// @vitest-environment jsdom
// Drives the real dioxus-ui/scripts/console-log-collector.js, loaded from disk.
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { beforeAll, describe, expect, it, vi } from "vitest";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..");
const SECRET = "change-me-lease-secret-placeholder";

type Collector = {
  setContext(meetingId: string, userId: string, displayName: string): void;
  flush(): void;
};

const fetchMock = vi.fn((_url: string, _opts: { body: string }) =>
  Promise.resolve({ ok: true, status: 200 }),
);

beforeAll(() => {
  const w = window as unknown as Record<string, unknown>;
  w.__APP_CONFIG = { consoleLogUploadEnabled: "true", hardwareMetricsEnabled: false };
  vi.stubGlobal("fetch", fetchMock);
  new Function(
    readFileSync(join(REPO_ROOT, "dioxus-ui/scripts/console-log-collector.js"), "utf8"),
  )();
  (w.__consoleLogCollector as Collector).setContext("room", "user", "name");
});

describe("console-log-collector scrub", () => {
  it("redacts lease secrets in plain and JSON-stringified arguments", () => {
    console.log("register", { recording_id: "r1", lease_secret: SECRET });
    console.log(`lease_secret=${SECRET}`);
    console.log({ aes_key: SECRET });
    console.log({ body: JSON.stringify({ lease_secret: SECRET }) });
    console.log({ leaseSecret: SECRET });
    (window as unknown as { __consoleLogCollector: Collector }).__consoleLogCollector.flush();

    const body = fetchMock.mock.calls.map(([, opts]) => opts.body).join("\n");
    expect(body).toContain("lease_secret=[REDACTED]");
    expect(body).toContain("aes_key=[REDACTED]");
    expect(body).not.toContain(SECRET);
  });
});
