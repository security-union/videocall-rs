import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { Command } from "commander";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ResourceCaptureSession, resolveSamplerScriptPath, startRemoteSampler } from "./session";
import {
  type GeneratorVerdict,
  RAW_CSV_SCHEMA_VERSION,
  registerResourceVerdictCommand,
  RESOURCE_SAMPLE_INTERVAL_SEC,
  windowVerdict,
} from "./window-verdict";

const meta = (interval: number): string =>
  `meta,${RAW_CSV_SCHEMA_VERSION},probe,host,100,4,0,0,${interval}`;
const META = meta(5);

interface Tick {
  epoch: number;
  busy: number;
  steal?: number;
  noCpu?: boolean;
  garble?: (fields: string[]) => string[];
  reset?: boolean;
  frozen?: boolean;
}

/** Not ok, and never labelled `RESOURCE_OK`. */
function expectUnjudged(v: GeneratorVerdict, reason: RegExp): void {
  expect(v).toMatchObject({ ok: false, starved: false, noEvidence: true });
  expect(v.detail).toMatch(/^RESOURCE_NO_EVIDENCE: /);
  expect(v.reasons.join()).toMatch(reason);
}

/** A raw sampler CSV whose cumulative `cpu` rows yield exactly `busy`% per tick. */
function rawCsv(ticks: readonly Tick[], meta: string | null = META): string {
  let user = 0;
  let idle = 0;
  let steal = 0;
  const rows = meta === null ? [] : [meta];
  for (const t of ticks) {
    if (t.reset) [user, idle, steal] = [0, 0, 0];
    const st = t.steal ?? 0;
    if (!t.frozen) {
      user += t.busy - st;
      steal += st;
      idle += 100 - t.busy;
    }
    rows.push(`load,${t.epoch},1,1,1`);
    const valid = [user, 0, 0, idle, 0, 0, 0, steal].map(String);
    const fields = (t.garble ? t.garble(valid) : valid).join(",");
    if (!t.noCpu) rows.push(`cpu,${t.epoch},${fields}`);
  }
  return rows.join("\n") + "\n";
}

/** Ticks at `epochs`; each tick's busy% is `10 + 90 × (overlap of its interval with [b0, b1]) / dt`. */
function burstTicks(epochs: readonly number[], b0: number, b1: number): Tick[] {
  return epochs.map((e, i) => {
    const start = i === 0 ? e : epochs[i - 1];
    const overlap = Math.max(0, Math.min(e, b1) - Math.max(start, b0));
    return { epoch: e, busy: i === 0 ? 10 : 10 + (90 * overlap) / (e - start) };
  });
}

const spaced = (start: number, steps: readonly number[], n: number): number[] => {
  const out = [start];
  for (let k = 1; k < n; k++) out.push(out[k - 1] + steps[(k - 1) % steps.length]);
  return out;
};

/** Ticks every `step` s over `[start, end]`, saturated (95%) where `hot` says so, else 10%. */
function ticks(start: number, end: number, hot: (epoch: number) => boolean, step = 5): Tick[] {
  const out: Tick[] = [];
  for (let e = start; e <= end; e += step) out.push({ epoch: e, busy: hot(e) ? 95 : 10 });
  return out;
}

describe("windowVerdict", () => {
  const csv = rawCsv(ticks(1000, 1100, (e) => e >= 1005 && e <= 1030));

  it("is ok when the saturation lies only outside the window", () => {
    expect(windowVerdict(csv, 1000, 1100).starved).toBe(true);
    const v = windowVerdict(csv, 1050, 1100);
    expect(v).toMatchObject({ ok: true, starved: false, noEvidence: false, reasons: [] });
    expect(v.window).toEqual({ from: 1050, to: 1100, unit: "epoch_seconds", samples: 11 });
    expect(v.detail).toMatch(/^RESOURCE_OK: 11 samples/);
  });

  it("is starved and not ok when the saturation lies inside the window", () => {
    const v = windowVerdict(csv, 1000, 1040);
    expect(v).toMatchObject({ ok: false, starved: true, noEvidence: false });
    expect(v.reasons.join()).toMatch(/CPU saturated/);
    expect(v.detail).toMatch(/^RESOURCE_STARVED: /);
  });

  it("includes the samples stamped exactly at both window edges", () => {
    const edge = rawCsv(ticks(1000, 1100, (e) => e >= 1050 && e <= 1060));
    expect(windowVerdict(edge, 1050, 1060)).toMatchObject({ ok: false, starved: true });
  });

  it("is not ok, with no evidence, for a window that holds no sample", () => {
    const v = windowVerdict(csv, 2000, 3000);
    expectUnjudged(v, /0 derived samples in window \[2000, 3000\].*\[1000, 1100\]/);
    expect(v.window.samples).toBe(0);
  });

  it("is not ok for a window given in milliseconds against a seconds CSV", () => {
    expectUnjudged(windowVerdict(csv, 1_050_000, 1_100_000), /0 derived samples/);
    expectUnjudged(windowVerdict(csv, 1050, 1_100_000), /window end 1100000 is not covered/);
  });

  it("is not ok when the window runs past either end of the CSV", () => {
    expectUnjudged(windowVerdict(csv, 1050, 1200), /window end 1200 is not covered/);
    const quiet = rawCsv(ticks(1000, 1100, () => false));
    expectUnjudged(windowVerdict(quiet, 994, 1040), /window start 994 is not covered/);
    expect(windowVerdict(quiet, 995, 1040).ok).toBe(true);
    expect(windowVerdict(csv, 1050, 1105).ok).toBe(true);
  });

  it("accepts a trailing gap of up to 6.25 s before the window end", () => {
    const quiet = rawCsv(ticks(1000, 1095, () => false));
    expect(windowVerdict(quiet, 1050, 1101).ok).toBe(true);
    expect(windowVerdict(quiet, 1050, 1101.25).ok).toBe(true);
  });

  it("does not judge a trailing gap over 6.25 s, including 7 s or more", () => {
    const quiet = rawCsv(ticks(1000, 1095, () => false));
    for (const to of [1101.5, 1102, 1105]) {
      expectUnjudged(windowVerdict(quiet, 1050, to), new RegExp(`window end ${to} is not covered`));
    }
  });

  it("is not ok when a short CSV meets a far-off window end", () => {
    const hot = rawCsv(ticks(1000, 1020, () => true));
    expectUnjudged(windowVerdict(hot, 1018, 1759650000.25), /1 derived samples/);
  });

  it("is not ok for fewer samples than the CPU rule needs, even saturated ones", () => {
    const hot = rawCsv(ticks(1000, 1010, () => true));
    expectUnjudged(windowVerdict(hot, 1000, 1010), /2 derived samples.*fewer than the 3/);
    expect(windowVerdict(rawCsv(ticks(1000, 1015, () => true)), 1000, 1015).starved).toBe(true);
  });

  it("is not ok across a sampler gap or a clock step inside the window", () => {
    const quiet = (a: number, b: number): Tick[] => ticks(a, b, () => false);
    const gap = rawCsv([...quiet(1000, 1030), ...quiet(1090, 1120)]);
    expectUnjudged(windowVerdict(gap, 1000, 1120), /gap or clock step.*1030->1090/);
    expectUnjudged(
      windowVerdict(rawCsv([...quiet(1000, 1030), ...quiet(1037, 1067)]), 1000, 1067),
      /1030->1037/,
    );
    expectUnjudged(
      windowVerdict(rawCsv([...quiet(1000, 1030), ...quiet(1034, 1064)]), 1000, 1064),
      /1030->1034/,
    );
    expect(windowVerdict(rawCsv([...quiet(1000, 1030), ...quiet(1036, 1066)]), 1000, 1066).ok).toBe(
      true,
    );
    const stepBack = rawCsv([...quiet(1000, 1050), ...quiet(1033, 1068)]);
    expectUnjudged(windowVerdict(stepBack, 1000, 1068), /1050->1033/);
  });

  it("refuses a sampler interval slower than the calibrated one", () => {
    const slow = rawCsv(
      ticks(1000, 1100, () => false, 10),
      meta(10),
    );
    expectUnjudged(windowVerdict(slow, 1000, 1100), /interval 10s/);
    const six = rawCsv(
      ticks(1000, 1102, () => false, 6),
      meta(6),
    );
    expectUnjudged(windowVerdict(six, 1000, 1102), /interval 6s/);
    const zero = rawCsv(
      ticks(1000, 1100, () => false),
      meta(0),
    );
    expectUnjudged(windowVerdict(zero, 1000, 1100), /interval 0s/);
  });

  it("judges a 100/100/70% per-second load as starved at 5 s, and refuses it at 1 s", () => {
    const fiveSec = ticks(1000, 1100, () => false).map((t, i) => ({ ...t, busy: i % 2 ? 88 : 94 }));
    expect(windowVerdict(rawCsv(fiveSec), 1000, 1100)).toMatchObject({ ok: false, starved: true });
    const oneSec = ticks(1000, 1100, () => false, 1).map((t, i) => ({
      ...t,
      busy: i % 3 === 2 ? 70 : 100,
    }));
    expectUnjudged(windowVerdict(rawCsv(oneSec, meta(1)), 1000, 1100), /interval 1s/);
  });

  describe("a 25 s full-CPU burst", () => {
    const phases = Array.from({ length: 160 }, (_, k) => k / 4);
    const sweep = (steps: number[], phase: number): ReturnType<typeof windowVerdict> => {
      const epochs = spaced(1000, steps, Math.ceil(170 / Math.min(...steps)));
      const b0 = 1050 + phase;
      return windowVerdict(rawCsv(burstTicks(epochs, b0, b0 + 25)), 1000, 1100 + 40);
    };

    it.each([[[5]], [[6]], [[5, 6, 5, 6]], [[5, 5, 6]], [[6, 6, 5]]])(
      "reads STARVED at all 160 phases of ticks %j",
      (steps) => {
        const missed = phases.filter((p) => !sweep(steps, p).starved);
        expect(missed).toEqual([]);
      },
    );

    it.each([[[5]], [[6]], [[5, 6]], [[6, 5]], [[5, 5, 6]], [[6, 6, 5]]])(
      "at the window end with ticks %j: STARVED up to a 6 s trailing gap, never ok up to 10 s",
      (steps) => {
        const epochs = spaced(1000, steps, 30);
        const last = epochs[epochs.length - 1];
        for (let gap = 0; gap <= 10; gap++) {
          for (const toPhase of [0, 0.25, 0.5, 0.75]) {
            const to = last + gap + toPhase;
            const v = windowVerdict(rawCsv(burstTicks(epochs, to - 25, to)), 1000, to);
            expect(v.ok, `gap ${gap}, to ${to}`).toBe(false);
            if (gap + toPhase <= 6.25) expect(v.starved, `gap ${gap}, to ${to}`).toBe(true);
          }
        }
      },
    );

    it.each([[[5, 5, 10]], [[7]], [[6, 10, 6]], [[7, 7, 8, 8]]])(
      "is never judged, at any phase, with a tick over 6.25 s in %j",
      (steps) => {
        for (const p of phases) {
          const v = sweep(steps, p);
          expect(v.ok, `phase ${p}`).toBe(false);
          expect(v.detail).not.toMatch(/^RESOURCE_OK/);
          expect(v.reasons.join()).toMatch(/tick outside \[5, 6\.25\]s/);
        }
      },
    );
  });

  it("is not ok for an inverted or non-finite window", () => {
    expect(windowVerdict(csv, 1100, 1050)).toMatchObject({ ok: false, noEvidence: true });
    const unbounded = windowVerdict(csv, Number.NEGATIVE_INFINITY, Number.POSITIVE_INFINITY);
    expect(unbounded).toMatchObject({ ok: false, noEvidence: true, window: { from: 0, to: 0 } });
    expect(unbounded.reasons).toEqual(["invalid window [-Infinity, Infinity]"]);
  });

  it.each([
    [
      "no meta row",
      rawCsv(
        ticks(1000, 1100, () => false),
        null,
      ),
      /no meta row/,
    ],
    ["a derived CSV", "epoch,dt_sec,cpu_busy_pct\n1005,5,10\n", /no meta row/],
    [
      "an unknown schema",
      rawCsv(
        ticks(1000, 1100, () => false),
        META.replace(/^meta,\d+/, "meta,99"),
      ),
      /unknown schema version 99/,
    ],
    ["an unsupported box", `${META}\nunsupported,1000,no /proc/stat\n`, /unsupported/],
  ])("is not ok for a malformed CSV: %s", (_label, text, reason) => {
    expectUnjudged(windowVerdict(text, 1000, 1100), reason);
  });

  it.each<[string, (f: string[]) => string[]]>([
    ["NaN in every field", (f) => f.map(() => "NaN")],
    ["NaN in iowait", (f) => f.map((v, i) => (i === 4 ? "NaN" : v))],
    ["an empty iowait", (f) => f.map((v, i) => (i === 4 ? "" : v))],
    ["no steal field", (f) => f.slice(0, 7)],
  ])("is not ok when every other cpu row has %s", (_label, garble) => {
    const t = ticks(1000, 1100, () => false);
    t.forEach((tick, i) => {
      if (i % 2 === 1) tick.garble = garble;
    });
    expectUnjudged(windowVerdict(rawCsv(t), 1050, 1100), /11 of 11 samples .* lack valid CPU/);
  });

  describe("precedence on a saturated box", () => {
    const hot = (a: number, b: number): Tick[] => ticks(a, b, () => true);

    it("keeps STARVED, not ok, when the window runs past the CSV", () => {
      const v = windowVerdict(rawCsv(hot(1000, 1100)), 1050, 1200);
      expect(v).toMatchObject({ ok: false, starved: true, noEvidence: false });
      expect(v.detail).toMatch(/^RESOURCE_STARVED: /);
      expect(v.reasons.join()).toMatch(/window end 1200 is not covered/);
    });

    it("keeps STARVED, not ok, across a sampler gap", () => {
      const v = windowVerdict(rawCsv([...hot(1000, 1030), ...hot(1090, 1120)]), 1000, 1120);
      expect(v).toMatchObject({ ok: false, starved: true, noEvidence: false });
      expect(v.detail).toMatch(/^RESOURCE_STARVED: /);
      expect(v.reasons.join()).toMatch(/1030->1090/);
    });

    it.each<[string, (f: string[]) => string[]]>([
      ["NaN iowait", (f) => f.map((v, i) => (i === 4 ? "NaN" : v))],
    ])("never reports STARVED from invalid counters: every other cpu row has %s", (_l, garble) => {
      const t = hot(1000, 1100);
      t.forEach((tick, i) => {
        if (i % 2 === 1) tick.garble = garble;
      });
      const v = windowVerdict(rawCsv(t), 1050, 1100);
      expectUnjudged(v, /lack valid CPU counters/);
      expect(v.reasons.join()).not.toMatch(/CPU saturated/);
    });

    it("never reports STARVED when a zeroed cpu row sits inside the only hot run long enough", () => {
      const t = hot(1000, 1100);
      t[11].garble = (f) => f.map(() => "0");
      const v = windowVerdict(rawCsv(t), 1050, 1070);
      expectUnjudged(v, /2 of 5 samples .* lack valid CPU/);
      expect(v.reasons.join()).not.toMatch(/CPU saturated/);
    });

    it("keeps STARVED from valid counters when a bad cpu row lies outside the hot run", () => {
      const t = ticks(1000, 1100, (e) => e >= 1020 && e <= 1045);
      t[18].garble = (f) => f.map((v, i) => (i === 4 ? "NaN" : v));
      const v = windowVerdict(rawCsv(t), 1000, 1100);
      expect(v).toMatchObject({ ok: false, starved: true, noEvidence: false });
      expect(v.detail).toMatch(/^RESOURCE_STARVED: /);
      expect(v.reasons.join()).toMatch(/CPU saturated.*2 of 20 samples .* lack valid CPU/);
    });
  });

  it.each<[string, (t: Tick[]) => void, number, number, RegExp]>([
    [
      "a zeroed row right after a missing one",
      (t) => {
        t[10].noCpu = true;
        t[11].garble = (f) => f.map(() => "0");
      },
      1050,
      1100,
      /3 of 11 samples/,
    ],
    ["a zeroed first row", (t) => (t[0].garble = (f) => f.map(() => "0")), 1000, 1050, /1 of 10/],
    [
      "a reset right after a missing row",
      (t) => {
        t[10].noCpu = true;
        t[11].reset = true;
      },
      1050,
      1100,
      /3 of 11 samples/,
    ],
  ])("is not ok for %s", (_label, damage, from, to, count) => {
    const t = ticks(1000, 1100, () => false);
    damage(t);
    expectUnjudged(windowVerdict(rawCsv(t), from, to), count);
  });

  it("is not ok when the cpu counters do not move between two ticks", () => {
    const t = ticks(1000, 1100, () => false);
    t[15].frozen = true;
    expectUnjudged(windowVerdict(rawCsv(t), 1050, 1100), /2 of 11 samples .* lack valid CPU/);
  });

  it("is not ok across a CPU counter reset", () => {
    const t = ticks(1000, 1100, () => true);
    for (const i of [12, 15, 18]) t[i].reset = true;
    expectUnjudged(windowVerdict(rawCsv(t), 1050, 1100), /6 of 11 samples .* lack valid CPU/);
  });

  it("is not ok when a sample in the window lacks CPU counters", () => {
    const t = ticks(1000, 1100, () => false);
    t[12].noCpu = true;
    expect(windowVerdict(rawCsv(t), 1000, 1040).ok).toBe(true);
    const v = windowVerdict(rawCsv(t), 1050, 1100);
    expectUnjudged(v, /2 of 11 samples in the window lack valid CPU counters/);
  });

  it("reports the steal peak inside the window", () => {
    const t = ticks(1000, 1100, () => false);
    t[15].steal = 7;
    expect(windowVerdict(rawCsv(t), 1050, 1100).detail).toMatch(/steal peak 7\.0%/);
  });
});

describe("resource-verdict CLI", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "resource-verdict-"));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
    process.exitCode = undefined;
  });

  async function run(args: string[]): Promise<{ code: number | undefined; out: string }> {
    const program = new Command();
    program.exitOverride();
    registerResourceVerdictCommand(program);
    const log = vi.spyOn(console, "log").mockImplementation(() => {});
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await program.parseAsync(["node", "cli.ts", "resource-verdict", ...args]);
    } finally {
      log.mockRestore();
      err.mockRestore();
    }
    return { code: process.exitCode as number | undefined, out: join(dir, "v.json") };
  }

  function writeCsv(text: string): string {
    const p = join(dir, "probe-raw.csv");
    writeFileSync(p, text);
    return p;
  }

  const read = (p: string): GeneratorVerdict => JSON.parse(readFileSync(p, "utf8"));
  const hotCsv = rawCsv(ticks(1000, 1100, (e) => e >= 1005 && e <= 1030));

  it("writes an ok verdict and exits 0 for a healthy window", async () => {
    const csvPath = writeCsv(hotCsv);
    const out = join(dir, "v.json");
    const r = await run(["--csv", csvPath, "--from", "1050", "--to", "1100.5", "--out", out]);
    expect(r.code).toBe(0);
    expect(read(out)).toMatchObject({ ok: true, window: { from: 1050, to: 1100.5 } });
  });

  it("writes a not-ok verdict and exits 1 for a starved window", async () => {
    const csvPath = writeCsv(hotCsv);
    const out = join(dir, "v.json");
    const r = await run(["--csv", csvPath, "--from", "1000", "--to", "1100", "--out", out]);
    expect(r.code).toBe(1);
    expect(read(out)).toMatchObject({ ok: false, starved: true });
  });

  it.each([
    ["an unreadable CSV", ["--csv", "/nonexistent/raw.csv", "--from", "1000", "--to", "1100"]],
    ["a non-numeric --from", ["--from", "", "--to", "1100"]],
    ["an ISO --to", ["--from", "1000", "--to", "2026-10-05T00:00:00Z"]],
  ])("writes a not-ok verdict for %s", async (_label, args) => {
    const out = join(dir, "v.json");
    const base = args.includes("--csv") ? [] : ["--csv", writeCsv(hotCsv)];
    const r = await run([...base, ...args, "--out", out]);
    expect(r.code).toBe(1);
    expect(read(out)).toMatchObject({ ok: false, noEvidence: true });
  });

  it("exits 2 when the verdict cannot be written", async () => {
    const csvPath = writeCsv(hotCsv);
    const r = await run(["--csv", csvPath, "--from", "1050", "--to", "1100", "--out", dir]);
    expect(r.code).toBe(2);
  });

  it("is reachable through the real CLI, with its unit and scope in the help", () => {
    const e2e = fileURLToPath(new URL("../../..", import.meta.url));
    const res = spawnSync(
      join(e2e, "node_modules/.bin/tsx"),
      ["bots-app/src/cli.ts", "resource-verdict", "--help"],
      { cwd: e2e, encoding: "utf8" },
    );
    expect(res.status, res.stderr).toBe(0);
    const help = res.stdout.replace(/\s+/g, " ");
    expect(help).toMatch(/epoch SECONDS/);
    expect(help).toMatch(/CPU rule only/);
    expect(help).toMatch(/Steal has read 0 on WSL2/);
  }, 60_000);

  it("exits 2, writing nothing, on a usage error", () => {
    const e2e = fileURLToPath(new URL("../../..", import.meta.url));
    const out = join(dir, "never.json");
    const res = spawnSync(
      join(e2e, "node_modules/.bin/tsx"),
      ["bots-app/src/cli.ts", "resource-verdict", "--csv", "x.csv", "--from", "1", "--to", "2"],
      { cwd: e2e, encoding: "utf8" },
    );
    expect(res.status, res.stderr).toBe(2);
    expect(res.stderr).toMatch(/--out/);
    expect(() => readFileSync(out)).toThrow();
  }, 60_000);

  it("emits JSON the scorer's --generator-verdict loader turns into G-V5", async () => {
    const csvPath = writeCsv(hotCsv);
    const okOut = join(dir, "ok.json");
    const badOut = join(dir, "bad.json");
    await run(["--csv", csvPath, "--from", "1050", "--to", "1100", "--out", okOut]);
    await run(["--csv", csvPath, "--from", "1000", "--to", "1100", "--out", badOut]);
    const quality = fileURLToPath(new URL("../../../../scripts/quality", import.meta.url));
    const script = [
      "import json, sys",
      "sys.path.insert(0, sys.argv[1])",
      "import call_quality_score as c",
      'empty = b\'{"status":"success","data":{"resultType":"matrix","result":[]}}\'',
      "for verdict, out in ((sys.argv[2], sys.argv[3]), (sys.argv[4], sys.argv[5])):",
      "    c.main(['--manifest', sys.argv[1] + '/example_run_manifest.json', '--prom-url',",
      "            'http://prom.invalid', '--generator-verdict', verdict, '--out-dir', out],",
      "           transport=lambda u, b, h: empty, environ={})",
      "    r = json.load(open(out + '/result.json'))",
      "    print([g['status'] for g in r['steps'][0]['validity'] if g['gate'] == 'G-V5'])",
    ].join("\n");
    const res = spawnSync(
      "python3",
      ["-c", script, quality, okOut, join(dir, "s-ok"), badOut, join(dir, "s-bad")],
      { encoding: "utf8" },
    );
    expect(res.error).toBeUndefined();
    expect(res.stdout.trim().split("\n"), res.stderr).toEqual(["['pass']", "['fail']"]);
  });
});

describe("raw CSV schema ↔ scripts/resource-sampler.sh", () => {
  it("reads the schema version the sampler writes", () => {
    const m = /emit "meta,(\d+),/.exec(readFileSync(resolveSamplerScriptPath(), "utf8"));
    expect(m, "the sampler no longer emits a meta row").not.toBeNull();
    expect(Number(m?.[1])).toBe(RAW_CSV_SCHEMA_VERSION);
  });

  it("calibrates for the interval the sampler and the capture session default to", () => {
    const m = /^INTERVAL=(\d+)$/m.exec(readFileSync(resolveSamplerScriptPath(), "utf8"));
    expect(Number(m?.[1])).toBe(RESOURCE_SAMPLE_INTERVAL_SEC);
    expect(new ResourceCaptureSession({ runDir: tmpdir() }).intervalSec).toBe(
      RESOURCE_SAMPLE_INTERVAL_SEC,
    );
    const calls: string[][] = [];
    const child = { stdin: { write: vi.fn(), end: vi.fn() }, stderr: { on: vi.fn() } };
    startRemoteSampler("", {
      host: {
        label: "h",
        host: "h",
        user: "u",
        sshKey: null,
        reposPath: "/r",
        notes: null,
        shell: null,
        profileFile: null,
        preCommand: null,
        forwardSsoState: false,
        addedAt: 0,
      },
      maxSeconds: 60,
      spawn: ((_cmd: string, args: string[]) => (calls.push(args), child)) as never,
    });
    const remote = /--interval '?(\d+)'?/.exec(calls[0]?.at(-1) ?? "");
    expect(Number(remote?.[1])).toBe(RESOURCE_SAMPLE_INTERVAL_SEC);
  });
});
