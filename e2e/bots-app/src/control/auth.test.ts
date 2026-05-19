import { stat, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  defaultTokenFilePath,
  extractBearerToken,
  findLatestTokenFile,
  generateToken,
  readTokenFile,
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
