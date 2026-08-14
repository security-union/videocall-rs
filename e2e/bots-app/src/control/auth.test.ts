import { stat, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  CTL_STATE_DIR_ENV,
  defaultTokenFilePath,
  extractBearerToken,
  findLatestTokenFile,
  generateToken,
  readTokenFile,
  resolveCtlStateDir,
  tokensMatch,
  writeTokenFile,
} from "./auth";

describe("generateToken", () => {
  it("produces a 64-char hex string", () => {
    const t = generateToken();
    expect(t).toMatch(/^[0-9a-f]{64}$/);
  });

  it("produces distinct values across invocations", () => {
    const a = generateToken();
    const b = generateToken();
    expect(a).not.toBe(b);
  });
});

describe("writeTokenFile + readTokenFile", () => {
  let dir: string;
  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), "bots-ctl-"));
  });
  afterEach(async () => {
    // Best-effort cleanup; vitest's CI runs in a temp dir anyway.
  });

  it("writes mode 0600 (owner read/write only)", async () => {
    const path = defaultTokenFilePath(dir, 12345);
    await writeTokenFile(path, {
      port: 9000,
      token: "deadbeef".repeat(8),
      startedAt: new Date().toISOString(),
      pid: 12345,
    });
    const st = await stat(path);
    // Mask off the file-type bits and compare only the permission
    // triple. POSIX permissions only — on Windows this check is
    // skipped (mode bits aren't faithfully reported there).
    const mode = st.mode & 0o777;
    if (process.platform !== "win32") {
      expect(mode).toBe(0o600);
    }
  });

  it("round-trips via readTokenFile", async () => {
    const path = defaultTokenFilePath(dir, 12345);
    const contents = {
      port: 9000,
      token: "a".repeat(64),
      startedAt: "2026-05-13T00:00:00.000Z",
      pid: 12345,
    };
    await writeTokenFile(path, contents);
    expect(await readTokenFile(path)).toEqual(contents);
  });

  it("rejects malformed JSON", async () => {
    const path = join(dir, "ctl-1.token");
    await writeFile(path, "not-json", "utf8");
    await expect(readTokenFile(path)).rejects.toThrow(/not valid JSON/);
  });

  it("rejects missing fields", async () => {
    const path = join(dir, "ctl-1.token");
    await writeFile(path, JSON.stringify({ port: 9000 }), "utf8");
    await expect(readTokenFile(path)).rejects.toThrow();
  });
});

describe("findLatestTokenFile", () => {
  let dir: string;
  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), "bots-ctl-"));
  });

  it("returns null when no token files are present", async () => {
    expect(await findLatestTokenFile(dir)).toBeNull();
  });

  it("returns null when the directory does not exist", async () => {
    expect(await findLatestTokenFile(join(dir, "no-such-dir"))).toBeNull();
  });

  it("picks the most recently modified ctl-*.token file", async () => {
    const a = join(dir, "ctl-100.token");
    const b = join(dir, "ctl-200.token");
    await writeTokenFile(a, {
      port: 1,
      token: "a".repeat(64),
      startedAt: "x",
      pid: 100,
    });
    // Ensure b's mtime is strictly later than a's even on FS with
    // 1s mtime resolution.
    await new Promise((r) => setTimeout(r, 25));
    await writeTokenFile(b, {
      port: 2,
      token: "b".repeat(64),
      startedAt: "x",
      pid: 200,
    });
    expect(await findLatestTokenFile(dir)).toBe(b);
  });

  it("ignores files that do not match ctl-<pid>.token", async () => {
    await writeFile(join(dir, "random.token"), "{}", "utf8");
    await writeFile(join(dir, "ctl.token"), "{}", "utf8");
    expect(await findLatestTokenFile(dir)).toBeNull();
  });
});

describe("tokensMatch", () => {
  it("returns true for equal strings", () => {
    expect(tokensMatch("abc", "abc")).toBe(true);
  });
  it("returns false for unequal strings", () => {
    expect(tokensMatch("abc", "abd")).toBe(false);
  });
  it("returns false for different lengths", () => {
    expect(tokensMatch("abc", "abcd")).toBe(false);
  });
});

describe("resolveCtlStateDir + defaultTokenFilePath (#2157)", () => {
  // The fix: the ctl token file must be relocatable OFF --assets-dir/BOT_RUN_DIR,
  // because in the K8s fleet that dir is a RETAINED PVC (kept on purpose so the
  // #2032 resource CSVs survive `scale --replicas=0`) — so a token written there
  // outlives the workload and survives a rotation of the bot-ctl-token Secret.
  //
  // Mutation sensitivity: reverting resolveCtlStateDir to `return runDir` fails
  // the two override cases; dropping the `.length > 0` guard fails the
  // empty-string case; changing the env-var NAME fails via CTL_STATE_DIR_ENV.
  // These two are ARBITRARY sample paths for exercising the resolver's logic —
  // they are NOT a drift check against the manifest, and must not be read as
  // one. Comparing a literal here to the same literal in the YAML would be the
  // `X == X` shape: it passes no matter what the manifest says. The real
  // manifest↔env drift assertions live in `statefulset-dir-drift.test.ts`,
  // which PARSES `k8s/statefulset.yaml` and compares the env var to the
  // volumeMount path it must equal.
  const RUN_DIR = "/var/lib/bots-run";
  const STATE_DIR = "/var/lib/bots-ctl";

  it("falls back to runDir when the override is unset (docker run / local dev)", () => {
    expect(resolveCtlStateDir(RUN_DIR, {})).toBe(RUN_DIR);
    expect(defaultTokenFilePath(RUN_DIR, 42)).toBe(join(RUN_DIR, "ctl-42.token"));
  });

  it("uses the override dir when set, keeping the token off runDir", () => {
    expect(resolveCtlStateDir(RUN_DIR, { [CTL_STATE_DIR_ENV]: STATE_DIR })).toBe(STATE_DIR);
  });

  it("treats an EMPTY override as unset (an empty env var must not yield a bare-root path)", () => {
    expect(resolveCtlStateDir(RUN_DIR, { [CTL_STATE_DIR_ENV]: "" })).toBe(RUN_DIR);
  });

  it("defaultTokenFilePath honors the override via process.env", () => {
    const prev = process.env[CTL_STATE_DIR_ENV];
    process.env[CTL_STATE_DIR_ENV] = STATE_DIR;
    try {
      // The path the orchestrator actually writes (cli.ts calls this with
      // --assets-dir). It must land in the emptyDir, NOT the PVC.
      expect(defaultTokenFilePath(RUN_DIR, 42)).toBe(join(STATE_DIR, "ctl-42.token"));
      expect(defaultTokenFilePath(RUN_DIR, 42)).not.toContain(RUN_DIR);
    } finally {
      if (prev === undefined) delete process.env[CTL_STATE_DIR_ENV];
      else process.env[CTL_STATE_DIR_ENV] = prev;
    }
  });

  it("findLatestTokenFile is NOT env-overridden — it scans exactly the dir it is given", async () => {
    // Deliberate asymmetry (documented on resolveCtlStateDir): an explicit
    // `ctl --run-dir <dir>` must never be silently redirected by an env var.
    const dir = await mkdtemp(join(tmpdir(), "bots-ctl-scan-"));
    const prev = process.env[CTL_STATE_DIR_ENV];
    process.env[CTL_STATE_DIR_ENV] = "/var/lib/bots-ctl-does-not-exist";
    try {
      const p = join(dir, "ctl-7.token");
      await writeTokenFile(p, {
        port: 1,
        token: "t".repeat(64),
        startedAt: new Date().toISOString(),
        pid: 7,
      });
      expect(await findLatestTokenFile(dir)).toBe(p);
    } finally {
      if (prev === undefined) delete process.env[CTL_STATE_DIR_ENV];
      else process.env[CTL_STATE_DIR_ENV] = prev;
    }
  });
});

describe("extractBearerToken", () => {
  it("extracts the token after `Bearer ` (mixed case)", () => {
    expect(extractBearerToken("Bearer xyz")).toBe("xyz");
    expect(extractBearerToken("bearer xyz")).toBe("xyz");
    expect(extractBearerToken("BEARER xyz")).toBe("xyz");
  });
  it("returns null on missing header", () => {
    expect(extractBearerToken(undefined)).toBeNull();
  });
  it("returns null on non-bearer header", () => {
    expect(extractBearerToken("Basic abc")).toBeNull();
  });
  it("handles arrays by picking the first element", () => {
    expect(extractBearerToken(["Bearer x"])).toBe("x");
  });
});
