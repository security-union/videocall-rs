import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { deriveSamples } from "./derive";
import { parseRawCsv } from "./proc";
import { resolveSamplerScriptPath } from "./session";

const SCRIPT = resolveSamplerScriptPath();
const SCRIPT_TEXT = readFileSync(SCRIPT, "utf8");

/**
 * Pull the PRODUCTION `/proc/net/dev` awk program straight out of the sampler
 * script (the one containing `sub(/:/`). Testing the extracted text — not a
 * hand-copied reimplementation — means reverting the loopback-exclusion fix in
 * the script makes this test fail. awk programs in the script contain no single
 * quotes, so the first `'` after `sub(/:/` is the program's closing quote.
 */
function extractNetAwkProgram(): string {
  // `[^']*` cannot cross a closing quote, so this captures exactly the one awk
  // program that contains `sub(/:/` — not the neighbouring meminfo/stat awks.
  const m = /awk '([^']*sub\(\/:\/[^']*)'/.exec(SCRIPT_TEXT);
  if (m === null) throw new Error("could not locate the net/dev awk program in the sampler");
  return m[1];
}

describe("resource-sampler.sh — /proc/net/dev extraction (cross-platform)", () => {
  it("sums non-loopback interfaces and excludes lo (guards the lo-exclusion fix)", () => {
    const program = extractNetAwkProgram();
    // A /proc/net/dev fixture where lo carries heavy traffic; rx is column 1
    // after the iface, tx is column 9.
    const fixture = [
      "Inter-|   Receive                                                |  Transmit",
      " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed",
      "    lo: 5000000  1000    0    0    0     0          0         0   5000000  1000    0    0    0     0       0          0",
      "  eth0: 1234567  8000    0    0    0     0          0         0   7654321  9000    0    0    0     0       0          0",
      " wlan0: 1000000  500     0    0    0     0          0         0    500000   400    0    0    0     0       0          0",
      "",
    ].join("\n");
    const dir = mkdtempSync(join(tmpdir(), "sampler-netdev-"));
    try {
      const fixturePath = join(dir, "net-dev");
      writeFileSync(fixturePath, fixture);
      const out = execFileSync("awk", [program, "NOW=1785293600", fixturePath], {
        encoding: "utf8",
      }).trim();
      // eth0 + wlan0 only; lo's 5_000_000/5_000_000 must be excluded.
      expect(out).toBe("net,1785293600,2234567,8154321");
      // Prove the fixture WOULD differ if lo were included (mutation would read
      // 7234567), so the assertion above is meaningfully testing the exclusion.
      const parts = out.split(",");
      expect(Number(parts[2])).toBeLessThan(2234567 + 5000000);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

/**
 * Functional test against the REAL /proc. Skips cleanly on any non-Linux box
 * (this dev machine is macOS); CI's vitest job runs on Linux, so it executes
 * there and turns a broken sampler (missing rows, unparseable output) red.
 */
describe("resource-sampler.sh — live /proc capture", () => {
  it.skipIf(process.platform !== "linux")(
    "samples real /proc into a CSV the production parser accepts",
    async () => {
      const dir = mkdtempSync(join(tmpdir(), "sampler-live-"));
      const out = join(dir, "raw.csv");
      try {
        await new Promise<void>((resolve, reject) => {
          const child = spawn(
            "bash",
            [SCRIPT, "--out", out, "--interval", "1", "--max-seconds", "3", "--label", "vitest"],
            { stdio: "ignore" },
          );
          const timer = setTimeout(() => {
            child.kill("SIGKILL");
            reject(new Error("sampler did not exit within 10s"));
          }, 10_000);
          child.on("error", reject);
          child.on("close", () => {
            clearTimeout(timer);
            resolve();
          });
        });

        const parsed = parseRawCsv(readFileSync(out, "utf8"));
        expect(parsed.unsupported).toBe(false);
        expect(parsed.meta).not.toBeNull();
        expect(parsed.meta!.clkTck).toBeGreaterThan(0);
        expect(parsed.meta!.ncpu).toBeGreaterThan(0);
        // Need at least two ticks so the derive step has a window.
        expect(parsed.samples.length).toBeGreaterThanOrEqual(2);

        const first = parsed.samples[0];
        expect(first.cpu).not.toBeNull(); // aggregate cpu row present
        expect(first.cores.size).toBeGreaterThan(0); // per-core rows present
        expect(Number.isFinite(first.cpu!.steal)).toBe(true); // steal field parsed
        expect(first.mem).not.toBeNull();
        expect(first.mem!.totalKb).toBeGreaterThan(0);
        expect(first.net).not.toBeNull();

        const derived = deriveSamples(parsed);
        expect(derived.length).toBeGreaterThanOrEqual(1);
        for (const d of derived) {
          expect(d.cpuBusyPct).toBeGreaterThanOrEqual(0);
          expect(d.cpuBusyPct).toBeLessThanOrEqual(100);
        }

        // Loopback exclusion on real /proc: the sampler's NIC bytes must not
        // include lo. Compare against a direct read; only meaningful when lo
        // has non-trivial traffic (a fresh CI runner may have ~0).
        const netDev = readFileSync("/proc/net/dev", "utf8");
        let loRx = 0;
        let allRx = 0;
        for (const line of netDev.split("\n").slice(2)) {
          const t = line.trim();
          if (t === "") continue;
          const [ifacePart, rest] = t.split(/:\s+/, 2);
          if (rest === undefined) continue;
          const rx = Number(rest.trim().split(/\s+/)[0]);
          if (!Number.isFinite(rx)) continue;
          allRx += rx;
          if (ifacePart.trim() === "lo") loRx += rx;
        }
        const lastNet = parsed.samples[parsed.samples.length - 1].net!;
        // Cumulative counters only grow, and the test reads /proc/net/dev AFTER
        // the sampler did, so the sampler's rx cannot exceed the current
        // lo-EXCLUSIVE total (with a little slack for the read gap being tiny).
        if (loRx > 1_000_000) {
          expect(lastNet.rx).toBeLessThanOrEqual(allRx - loRx + 5_000_000);
        }
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    },
  );
});
