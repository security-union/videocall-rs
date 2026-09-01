import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { classifyStamp, parseSupervisedBackends, readStamp } from "./backend-freshness";

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const COMPOSE_REL = "docker/docker-compose.e2e.yaml";
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const COMPOSE_FILE = path.resolve(REPO_ROOT, COMPOSE_REL);
const WS = { service: "websocket-api", bin: "websocket_server" };
// Past STALE_GRACE_MS, where a cold stamp is a dead watcher rather than a
// stack that has not stamped yet.
const AFTER_GRACE = 90_000;

describe("parseSupervisedBackends", () => {
  // Drift lock: revert a service to a bare `cargo run` and this fails.
  it("finds every backend the committed e2e compose file supervises", () => {
    const backends = parseSupervisedBackends(fs.readFileSync(COMPOSE_FILE, "utf8"));
    expect(backends.sort((a, b) => a.bin.localeCompare(b.bin))).toEqual([
      { service: "meeting-api", bin: "meeting-api" },
      { service: "websocket-api", bin: "websocket_server" },
      { service: "webtransport-api", bin: "webtransport_server" },
    ]);
  });

  it("re-runs on a compose-file change, or a service edit lands with this lock unread", () => {
    const wf = parseYaml(fs.readFileSync(path.join(REPO_ROOT, PR_CHECK_REL), "utf8")) as Record<
      string,
      unknown
    >;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    expect(paths, `pr-check-e2e-lint-hcl.yaml must trigger on ${COMPOSE_REL}`).toContain(
      COMPOSE_REL,
    );
  });

  it("returns nothing for a compose file that runs cargo directly", () => {
    expect(parseSupervisedBackends('  websocket-api:\n    command: "cargo run --bin x"')).toEqual(
      [],
    );
  });

  it("de-duplicates repeated services", () => {
    const text = "  svc:\n    e2e-backend.sh supervise foo\n    e2e-backend.sh   supervise   foo\n";
    expect(parseSupervisedBackends(text)).toEqual([{ service: "svc", bin: "foo" }]);
  });
});

describe("classifyStamp", () => {
  const fresh = (build: string) => ({
    stamp: { service: "websocket_server", build, at: "2026-08-26T00:00:00Z" } as never,
    ageMs: 1_000,
  });

  it("passes a fresh successful build", () => {
    expect(classifyStamp(WS, fresh("ok"), AFTER_GRACE).verdict).toBe("ok");
  });

  it("keeps waiting while a build is in flight", () => {
    expect(classifyStamp(WS, fresh("building"), AFTER_GRACE).verdict).toBe("wait");
  });

  it("keeps waiting when no stamp exists yet", () => {
    expect(classifyStamp(WS, {}, AFTER_GRACE).verdict).toBe("wait");
  });

  it("fails fast on a build error rather than timing out on HTTP probes", () => {
    const { verdict, reason } = classifyStamp(WS, fresh("failed"), AFTER_GRACE);
    expect(verdict).toBe("fail");
    expect(reason).toContain("FAILED TO BUILD");
    // `docker compose logs websocket_server` is not a runnable command.
    expect(reason).toContain("logs websocket-api");
  });

  it("fails when the heartbeat has stopped, even though the last build was ok", () => {
    const { verdict, reason } = classifyStamp(
      WS,
      {
        ...fresh("ok"),
        ageMs: 300_000,
      },
      AFTER_GRACE,
    );
    expect(verdict).toBe("fail");
    expect(reason).toContain("no watcher is supervising it");
  });

  it("fails on a stopped watcher mid-build too", () => {
    expect(classifyStamp(WS, { ...fresh("building"), ageMs: 300_000 }, AFTER_GRACE).verdict).toBe(
      "fail",
    );
  });

  it("fails a cold unparseable stamp instead of polling until the deadline", () => {
    const { verdict, reason } = classifyStamp(WS, { ageMs: 3_600_000 }, AFTER_GRACE);
    expect(verdict).toBe("fail");
    expect(reason).toContain("no watcher is supervising it");
  });

  it("still waits on an absent stamp, which has no age to judge", () => {
    expect(classifyStamp(WS, {}, AFTER_GRACE).verdict).toBe("wait");
  });

  it("waits on a fresh unparseable stamp, which may be a partial write", () => {
    expect(classifyStamp(WS, { ageMs: 1_000 }, AFTER_GRACE).verdict).toBe("wait");
  });

  it("waits on a stale stamp seen within the grace window", () => {
    const { verdict, reason } = classifyStamp(WS, { ...fresh("ok"), ageMs: 300_000 }, 5_000);
    expect(verdict).toBe("wait");
    expect(reason).toContain("from an earlier run");
  });

  it("fails on a stale stamp still stale after the grace window", () => {
    const { verdict, reason } = classifyStamp(WS, { ...fresh("ok"), ageMs: 300_000 }, AFTER_GRACE);
    expect(verdict).toBe("fail");
    expect(reason).toContain("no watcher is supervising it");
  });

  it("passes a fresh stamp inside the grace window without waiting it out", () => {
    expect(classifyStamp(WS, fresh("ok"), 5_000).verdict).toBe("ok");
  });

  it("reports staleness rather than the recorded build error when both apply", () => {
    const { reason } = classifyStamp(WS, { ...fresh("failed"), ageMs: 300_000 }, AFTER_GRACE);
    expect(reason).toContain("no watcher is supervising it");
    expect(reason).not.toContain("FAILED TO BUILD");
  });
});

describe("readStamp", () => {
  it("reports nothing for a missing file", () => {
    expect(readStamp(path.join(os.tmpdir(), "vc-2513-absent.json"))).toEqual({});
  });

  it("round-trips a stamp written on disk and ages it", () => {
    const file = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "vc-2513-")), "s.json");
    fs.writeFileSync(file, JSON.stringify({ service: "s", build: "ok", at: "now" }));
    const { stamp, ageMs } = readStamp(file);
    expect(stamp?.build).toBe("ok");
    expect(ageMs).toBeLessThan(60_000);
  });

  it("surfaces an unparseable stamp as present-but-undecodable, not as absent", () => {
    const file = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "vc-2513-")), "s.json");
    fs.writeFileSync(file, "{ truncated");
    const read = readStamp(file);
    expect(read.stamp).toBeUndefined();
    expect(read.ageMs).toBeDefined();
    expect(classifyStamp(WS, read, AFTER_GRACE).reason).toContain("unparseable");
  });
});
