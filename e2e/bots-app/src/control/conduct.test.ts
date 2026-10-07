import dns from "node:dns";
import { createServer as createHttpServer } from "node:http";
import { type AddressInfo, createServer } from "node:net";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { BotTask } from "../orchestrator";
import { SD_SOURCE } from "../posture";
import { generateToken } from "./auth";
import { CtlHttpError, CtlUnreachableError } from "./client";
import {
  type Clock,
  type ConductorClient,
  type ConductorClientFactory,
  type ControlCall,
  type HostResolveOptions,
  type ScheduledCall,
  applyAction,
  buildSchedule,
  classifyHealthz,
  fetchHealthzHttp,
  conductScenario,
  httpConductorClientFactory,
  parseAtDuration,
  parseScenario,
  READINESS_POLL_INTERVAL_MS,
  resolveBotHost,
  ScenarioValidationError,
} from "./conduct";
import { NETEM_PARAM_KEYS } from "./netem";
import {
  BotNotJoinedError,
  generateBotId,
  newRegistryEntry,
  type BotRegistryEntry,
} from "./registry";
import { type ControlServerHandle, startControlServer } from "./server";

// The canonical scenario from the deploy task's scenario.example.yaml.
// Kept verbatim so any drift between this and the schema contract is caught.
const EXAMPLE_SCENARIO = `
room: bottest
timeline:
  - { at: 0s,   bot: 0, action: unmute }
  - { at: 10s,  bot: 1, action: screenshare-on }
  - { at: 20s,  bot: 2, action: netem, profile: lossy_mobile }
  - { at: 35s,  bot: 1, action: screenshare-off }
  - { at: 40s,  bot: 0, action: talk, durationMs: 15000 }
  - { at: 60s,  bot: 2, action: netem-clear }
`;

const HOST_OPTS: HostResolveOptions = {
  service: "videocall-bots",
  namespace: "bot-load",
  dnsSuffix: "svc.cluster.local",
};

// ── parseAtDuration ──────────────────────────────────────────────────────

describe("parseAtDuration", () => {
  it("parses whole seconds", () => {
    expect(parseAtDuration("0s")).toBe(0);
    expect(parseAtDuration("10s")).toBe(10_000);
    expect(parseAtDuration("60s")).toBe(60_000);
  });

  it("parses milliseconds", () => {
    expect(parseAtDuration("500ms")).toBe(500);
    expect(parseAtDuration("0ms")).toBe(0);
  });

  it("parses fractional seconds, rounding to whole ms", () => {
    expect(parseAtDuration("1.5s")).toBe(1500);
  });

  it("trims surrounding whitespace", () => {
    expect(parseAtDuration("  10s ")).toBe(10_000);
  });

  it("rejects a missing/unknown unit", () => {
    expect(() => parseAtDuration("10")).toThrow(ScenarioValidationError);
    expect(() => parseAtDuration("10m")).toThrow(ScenarioValidationError);
    expect(() => parseAtDuration("10x")).toThrow(ScenarioValidationError);
  });

  it("rejects a non-numeric magnitude and negatives", () => {
    expect(() => parseAtDuration("abc")).toThrow(ScenarioValidationError);
    expect(() => parseAtDuration("-5s")).toThrow(ScenarioValidationError);
  });
});

// ── parseScenario: happy path ────────────────────────────────────────────

describe("parseScenario (valid)", () => {
  it("parses the canonical example into 6 validated entries", () => {
    const s = parseScenario(EXAMPLE_SCENARIO);
    expect(s.room).toBe("bottest");
    expect(s.entries).toHaveLength(6);
    expect(s.entries[0]).toMatchObject({ atMs: 0, bot: 0, action: "unmute" });
    expect(s.entries[1]).toMatchObject({ atMs: 10_000, bot: 1, action: "screenshare-on" });
    expect(s.entries[2]).toMatchObject({ atMs: 20_000, bot: 2, action: "netem" });
    expect(s.entries[2].netemBody).toEqual({ profile: "lossy_mobile" });
    expect(s.entries[2].netemLabel).toBe("lossy_mobile");
    expect(s.entries[4]).toMatchObject({ atMs: 40_000, bot: 0, action: "talk", durationMs: 15000 });
    expect(s.entries[5]).toMatchObject({ atMs: 60_000, bot: 2, action: "netem-clear" });
  });

  it("accepts an absent room (informational only)", () => {
    const s = parseScenario("timeline:\n  - { at: 0s, bot: 0, action: leave }\n");
    expect(s.room).toBeUndefined();
    expect(s.entries).toHaveLength(1);
  });

  it("accepts raw netem params and labels them 'custom'", () => {
    const s = parseScenario(
      "timeline:\n  - { at: 5s, bot: 0, action: netem, delayMs: 150, lossPct: 5 }\n",
    );
    expect(s.entries[0].netemBody).toEqual({ delayMs: 150, lossPct: 5 });
    expect(s.entries[0].netemLabel).toBe("custom");
  });

  it("carries limitPkts through to the netem body", () => {
    const s = parseScenario(
      "timeline:\n  - { at: 30s, bot: 2, action: netem, rateKbit: 56, limitPkts: 10 }\n",
    );
    expect(s.entries[0].netemBody).toEqual({ rateKbit: 56, limitPkts: 10 });
  });
});

// ── parseScenario: every error case ──────────────────────────────────────

describe("parseScenario (validation errors)", () => {
  const bad = (text: string): (() => void) => {
    return () => parseScenario(text);
  };

  it("rejects a non-mapping document", () => {
    expect(bad("- 1\n- 2\n")).toThrow(/YAML mapping/);
    expect(bad("42\n")).toThrow(/YAML mapping/);
  });

  it("rejects invalid YAML", () => {
    expect(bad("timeline: [ { at: 0s, ")).toThrow(/not valid YAML/);
  });

  it("rejects a missing/non-array timeline", () => {
    expect(bad("room: x\n")).toThrow(/`timeline` must be an array/);
    expect(bad("timeline: 5\n")).toThrow(/`timeline` must be an array/);
  });

  it("rejects an empty timeline", () => {
    expect(bad("timeline: []\n")).toThrow(/at least one entry/);
  });

  it("rejects a non-mapping entry", () => {
    expect(bad("timeline:\n  - 5\n")).toThrow(/timeline\[0\] must be a mapping/);
  });

  it("rejects a bad or missing `at`", () => {
    // Missing, and a bare YAML number (not a string) both fail the type check.
    expect(bad("timeline:\n  - { bot: 0, action: leave }\n")).toThrow(/\.at must be a string/);
    expect(bad("timeline:\n  - { at: 10, bot: 0, action: leave }\n")).toThrow(
      /\.at must be a string/,
    );
    // A quoted string with no valid unit fails the duration parse path.
    expect(bad('timeline:\n  - { at: "10", bot: 0, action: leave }\n')).toThrow(/\.at: expected/);
    expect(bad("timeline:\n  - { at: 10m, bot: 0, action: leave }\n")).toThrow(/\.at: expected/);
  });

  it("rejects a bad `bot` ordinal", () => {
    expect(bad("timeline:\n  - { at: 0s, bot: -1, action: leave }\n")).toThrow(/non-negative/);
    expect(bad("timeline:\n  - { at: 0s, bot: 1.5, action: leave }\n")).toThrow(/non-negative/);
    expect(bad("timeline:\n  - { at: 0s, action: leave }\n")).toThrow(/non-negative/);
  });

  it("rejects an unknown action", () => {
    expect(bad("timeline:\n  - { at: 0s, bot: 0, action: explode }\n")).toThrow(/is unknown/);
  });

  it("rejects a netem action with no profile and no params", () => {
    expect(bad("timeline:\n  - { at: 0s, bot: 0, action: netem }\n")).toThrow(
      /timeline\[0\]: empty request/,
    );
  });

  it("rejects a netem action with both a profile and raw params", () => {
    expect(
      bad("timeline:\n  - { at: 0s, bot: 0, action: netem, profile: dialup, delayMs: 10 }\n"),
    ).toThrow(/either "profile" or raw params/);
  });

  it("rejects an out-of-range limitPkts at parse time", () => {
    expect(
      bad("timeline:\n  - { at: 0s, bot: 0, action: netem, rateKbit: 56, limitPkts: 0 }\n"),
    ).toThrow(/timeline\[0\]: "limitPkts" must be an integer >= 1/);
  });

  it("rejects a scenario downlink rate without its ingress depth", () => {
    expect(
      bad("timeline:\n  - { at: 0s, bot: 0, action: netem, lossPct: 1, downlinkRateKbit: 4000 }\n"),
    ).toThrow(/timeline\[0\]: .*supply both or neither/);
  });

  it.each(NETEM_PARAM_KEYS)("carries %s to the validator instead of dropping it", (key) => {
    expect(
      bad(`timeline:\n  - { at: 0s, bot: 0, action: netem, profile: satellite, ${key}: 10 }\n`),
    ).toThrow(/not both/);
  });

  it("rejects an unknown netem profile", () => {
    expect(bad("timeline:\n  - { at: 0s, bot: 0, action: netem, profile: nope }\n")).toThrow(
      /unknown profile/,
    );
  });

  it("rejects a talk action with a missing/invalid durationMs", () => {
    expect(bad("timeline:\n  - { at: 0s, bot: 0, action: talk }\n")).toThrow(
      /durationMs must be a positive integer/,
    );
    expect(bad("timeline:\n  - { at: 0s, bot: 0, action: talk, durationMs: 0 }\n")).toThrow(
      /durationMs must be a positive integer/,
    );
    expect(bad("timeline:\n  - { at: 0s, bot: 0, action: talk, durationMs: -5 }\n")).toThrow(
      /durationMs must be a positive integer/,
    );
  });
});

// ── Bot → host resolution ────────────────────────────────────────────────

describe("resolveBotHost", () => {
  it("builds the StatefulSet pod FQDN from the ordinal", () => {
    expect(resolveBotHost(0, HOST_OPTS)).toBe(
      "videocall-bots-0.videocall-bots.bot-load.svc.cluster.local",
    );
    expect(resolveBotHost(3, HOST_OPTS)).toBe(
      "videocall-bots-3.videocall-bots.bot-load.svc.cluster.local",
    );
  });

  it("honors overridden service/namespace/dns-suffix", () => {
    expect(
      resolveBotHost(2, {
        service: "load-bots",
        namespace: "perf",
        dnsSuffix: "svc.cluster.local",
      }),
    ).toBe("load-bots-2.load-bots.perf.svc.cluster.local");
  });
});

// ── buildSchedule: ordering + talk expansion ─────────────────────────────

describe("buildSchedule", () => {
  it("sorts the canonical example and expands talk into unmute + follow-up mute", () => {
    const s = parseScenario(EXAMPLE_SCENARIO);
    const schedule = buildSchedule(s.entries, HOST_OPTS);
    const summary = schedule.map((sc) => ({ atMs: sc.atMs, bot: sc.bot, call: sc.call }));
    expect(summary).toEqual([
      { atMs: 0, bot: 0, call: { kind: "mute", muted: false } },
      { atMs: 10_000, bot: 1, call: { kind: "share", on: true } },
      {
        atMs: 20_000,
        bot: 2,
        call: { kind: "netem", body: { profile: "lossy_mobile" }, label: "lossy_mobile" },
      },
      { atMs: 35_000, bot: 1, call: { kind: "share", on: false } },
      { atMs: 40_000, bot: 0, call: { kind: "mute", muted: false } },
      { atMs: 55_000, bot: 0, call: { kind: "mute", muted: true } },
      { atMs: 60_000, bot: 2, call: { kind: "netem-clear" } },
    ]);
    // Follow-up mute lands at start + durationMs (40s + 15s = 55s).
    const followUp = schedule.find((sc) => sc.atMs === 55_000);
    expect(followUp?.call).toEqual({ kind: "mute", muted: true });
    // Every scheduled call carries the resolved pod host.
    expect(schedule[0].host).toBe("videocall-bots-0.videocall-bots.bot-load.svc.cluster.local");
  });

  it("is sorted by ascending offset", () => {
    const s = parseScenario(EXAMPLE_SCENARIO);
    const schedule = buildSchedule(s.entries, HOST_OPTS);
    const offsets = schedule.map((sc) => sc.atMs);
    expect(offsets).toEqual([...offsets].sort((a, b) => a - b));
  });

  it("coalesces overlapping talk windows on the same bot into one unmute..mute", () => {
    // 40..55 and 50..60 overlap -> merged 40..60. A naive per-talk
    // expansion would (incorrectly) mute at 55 while the second window is
    // still open. Coalescing yields a single mute at 60.
    const s = parseScenario(
      "timeline:\n" +
        "  - { at: 40s, bot: 0, action: talk, durationMs: 15000 }\n" +
        "  - { at: 50s, bot: 0, action: talk, durationMs: 10000 }\n",
    );
    const schedule = buildSchedule(s.entries, HOST_OPTS);
    expect(schedule.map((sc) => ({ atMs: sc.atMs, call: sc.call }))).toEqual([
      { atMs: 40_000, call: { kind: "mute", muted: false } },
      { atMs: 60_000, call: { kind: "mute", muted: true } },
    ]);
  });

  it("coalesces touching talk windows (no mute/unmute flap at the boundary)", () => {
    const s = parseScenario(
      "timeline:\n" +
        "  - { at: 10s, bot: 0, action: talk, durationMs: 5000 }\n" +
        "  - { at: 15s, bot: 0, action: talk, durationMs: 5000 }\n",
    );
    const schedule = buildSchedule(s.entries, HOST_OPTS);
    expect(schedule.map((sc) => sc.atMs)).toEqual([10_000, 20_000]);
  });

  it("keeps talk windows on different bots independent", () => {
    const s = parseScenario(
      "timeline:\n" +
        "  - { at: 10s, bot: 0, action: talk, durationMs: 5000 }\n" +
        "  - { at: 12s, bot: 1, action: talk, durationMs: 5000 }\n",
    );
    const schedule = buildSchedule(s.entries, HOST_OPTS);
    const byBot = (n: number) => schedule.filter((sc) => sc.bot === n).map((sc) => sc.atMs);
    expect(byBot(0)).toEqual([10_000, 15_000]);
    expect(byBot(1)).toEqual([12_000, 17_000]);
  });
});

// ── action -> control-call mapping (mocked client) ───────────────────────

function recordingClient(record: string[]): ConductorClient {
  return {
    mute: async (m) => void record.push(`mute:${m}`),
    setCameraOff: async (o) => void record.push(`camera:${o}`),
    setScreenShare: async (s) => void record.push(`share:${s}`),
    leave: async () => void record.push("leave"),
    applyNetem: async (b) => {
      record.push(`netem:${JSON.stringify(b)}`);
      return { ingressShaped: false, mirrorRemoved: false };
    },
    clearNetem: async () => {
      record.push("netem-clear");
      return { ingressShaped: false, mirrorRemoved: false };
    },
  };
}

describe("applyAction (control-call mapping)", () => {
  const cases: Array<[ControlCall, string]> = [
    [{ kind: "mute", muted: true }, "mute:true"],
    [{ kind: "mute", muted: false }, "mute:false"],
    [{ kind: "camera", off: true }, "camera:true"],
    [{ kind: "camera", off: false }, "camera:false"],
    [{ kind: "share", on: true }, "share:true"],
    [{ kind: "share", on: false }, "share:false"],
    [
      { kind: "netem", body: { profile: "lossy_mobile" }, label: "lossy_mobile" },
      'netem:{"profile":"lossy_mobile"}',
    ],
    [{ kind: "netem-clear" }, "netem-clear"],
    [{ kind: "leave" }, "leave"],
  ];

  it.each(cases)("routes %j to the right client method", async (call, expected) => {
    const record: string[] = [];
    await applyAction(recordingClient(record), call);
    expect(record).toEqual([expected]);
  });
});

// ── runner: injectable clock, ordering, timing, token hygiene ────────────

/** A fake clock whose sole time source is `sleep`, so no real time passes. */
function fakeClock(start = 1000): {
  clock: Clock;
  sleep: (ms: number) => Promise<void>;
  read: () => number;
} {
  let now = start;
  return {
    clock: { now: () => now },
    sleep: async (ms: number) => {
      now += ms;
    },
    read: () => now,
  };
}

describe("conductScenario (live run under injectable clock)", () => {
  it("fires each action at its scheduled offset, in order", async () => {
    const fc = fakeClock(1000);
    const fired: Array<{ offset: number; host: string; label: string }> = [];
    const factory: ConductorClientFactory = (config) => ({
      mute: async (m) =>
        void fired.push({ offset: fc.read() - 1000, host: config.host, label: `mute:${m}` }),
      setCameraOff: async (o) =>
        void fired.push({ offset: fc.read() - 1000, host: config.host, label: `camera:${o}` }),
      setScreenShare: async (s) =>
        void fired.push({ offset: fc.read() - 1000, host: config.host, label: `share:${s}` }),
      leave: async () =>
        void fired.push({ offset: fc.read() - 1000, host: config.host, label: "leave" }),
      applyNetem: async (b) => {
        fired.push({
          offset: fc.read() - 1000,
          host: config.host,
          label: `netem:${JSON.stringify(b)}`,
        });
        return { ingressShaped: false, mirrorRemoved: false };
      },
      clearNetem: async () => {
        fired.push({ offset: fc.read() - 1000, host: config.host, label: "netem-clear" });
        return { ingressShaped: false, mirrorRemoved: false };
      },
    });

    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: { clientFactory: factory, clock: fc.clock, sleep: fc.sleep, log: () => {} },
    });

    expect(summary).toEqual({
      planned: 7,
      fired: 7,
      failed: 0,
      dropped: 0,
      unreachable: 0,
      dryRun: false,
    });
    expect(fired.map((f) => f.offset)).toEqual([0, 10_000, 20_000, 35_000, 40_000, 55_000, 60_000]);
    expect(fired.map((f) => f.label)).toEqual([
      "mute:false",
      "share:true",
      'netem:{"profile":"lossy_mobile"}',
      "share:false",
      "mute:false",
      "mute:true",
      "netem-clear",
    ]);
    // Each call went to the pod for its bot ordinal.
    expect(fired[0].host).toBe("videocall-bots-0.videocall-bots.bot-load.svc.cluster.local");
    expect(fired[1].host).toBe("videocall-bots-1.videocall-bots.bot-load.svc.cluster.local");
    expect(fired[2].host).toBe("videocall-bots-2.videocall-bots.bot-load.svc.cluster.local");
  });

  it("continues past a failing call and counts it", async () => {
    const fc = fakeClock();
    const factory: ConductorClientFactory = () => ({
      ...recordingClient([]),
      setScreenShare: async () => {
        throw new Error("pod unreachable");
      },
    });
    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: { clientFactory: factory, clock: fc.clock, sleep: fc.sleep, log: () => {} },
    });
    // Two screenshare calls fail; the other five fire.
    expect(summary).toEqual({
      planned: 7,
      fired: 5,
      failed: 2,
      dropped: 0,
      unreachable: 0,
      dryRun: false,
    });
  });

  it("counts a call rejected because no bot has joined as dropped, not failed (#2386)", async () => {
    const fc = fakeClock();
    const logs: string[] = [];
    const factory: ConductorClientFactory = () => ({
      ...recordingClient([]),
      mute: async () => {
        throw new BotNotJoinedError("no bot registered on h");
      },
      setScreenShare: async () => {
        throw new CtlHttpError(409, { error: "bot x is not yet in-meeting" });
      },
      clearNetem: async () => {
        throw new Error("pod unreachable");
      },
    });
    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: { clientFactory: factory, clock: fc.clock, sleep: fc.sleep, log: (l) => logs.push(l) },
    });
    expect(summary).toEqual({
      planned: 7,
      fired: 1,
      failed: 1,
      dropped: 5,
      unreachable: 0,
      dryRun: false,
    });
    expect(logs.filter((l) => l.includes("dropped (not joined):"))).toHaveLength(5);
    expect(logs.at(-1)).toBe(
      "conduct: done - 1 action(s) fired, 1 failed, 5 dropped (not joined), 0 unreachable",
    );
  });

  it("counts a call whose connection never opened as unreachable, apart from failed (#2386)", async () => {
    const fc = fakeClock();
    const logs: string[] = [];
    const factory: ConductorClientFactory = () => ({
      ...recordingClient([]),
      setScreenShare: async () => {
        throw new CtlUnreachableError("ctl: connection to h:8080 failed: connect ECONNREFUSED");
      },
      clearNetem: async () => {
        throw new Error("ctl: connection to h:8080 failed: socket hang up");
      },
    });
    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: { clientFactory: factory, clock: fc.clock, sleep: fc.sleep, log: (l) => logs.push(l) },
    });
    expect(summary).toEqual({
      planned: 7,
      fired: 4,
      failed: 1,
      dropped: 0,
      unreachable: 2,
      dryRun: false,
    });
    expect(logs.filter((l) => l.includes(" unreachable: "))).toHaveLength(2);
  });

  it("a failed name lookup rejects with CtlUnreachableError; an HTTP error after connect does not", async () => {
    const surface = { getRegistry: () => new Map(), expectedBots: () => 0 } as never;
    const lookup = vi
      .spyOn(dns, "lookup")
      .mockImplementation(((
        _host: string,
        _opts: unknown,
        cb: (e: NodeJS.ErrnoException) => void,
      ) => cb(Object.assign(new Error("getaddrinfo ENOTFOUND"), { code: "ENOTFOUND" }))) as never);
    try {
      const dead = httpConductorClientFactory()({
        host: "pod.fleet.invalid",
        port: 8080,
        token: "t",
      });
      await expect(dead.clearNetem()).rejects.toBeInstanceOf(CtlUnreachableError);
    } finally {
      lookup.mockRestore();
    }
    const reset = createServer((s) => s.destroy());
    await new Promise<void>((r) => reset.listen(0, "127.0.0.1", r));
    try {
      const resetPort = (reset.address() as AddressInfo).port;
      const cut = httpConductorClientFactory()({ host: "127.0.0.1", port: resetPort, token: "t" });
      const err = await cut.clearNetem().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(Error);
      expect(err).not.toBeInstanceOf(CtlUnreachableError);
    } finally {
      await new Promise((r) => reset.close(r));
    }
    const open = await startControlServer({ port: 0, token: "t", surface });
    try {
      const live = httpConductorClientFactory()({ host: "127.0.0.1", port: open.port, token: "x" });
      const err = await live.clearNetem().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(CtlHttpError);
      expect(err).not.toBeInstanceOf(CtlUnreachableError);
    } finally {
      await open.close();
    }
  });

  it("never writes the bearer token to a log line", async () => {
    const SECRET = "SUPER-SECRET-TOKEN-abc123";
    const logs: string[] = [];
    const fc = fakeClock();
    const factory: ConductorClientFactory = () => recordingClient([]);
    await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: SECRET,
      deps: { clientFactory: factory, clock: fc.clock, sleep: fc.sleep, log: (l) => logs.push(l) },
    });
    expect(logs.length).toBeGreaterThan(0);
    expect(logs.join("\n")).not.toContain(SECRET);
  });

  /** A pod carrying a mirror that an egress-only shape removes. */
  const mirrorRemovingClient = (): ConductorClient => ({
    ...recordingClient([]),
    applyNetem: async () => ({ ingressShaped: false, mirrorRemoved: true }),
    clearNetem: async () => ({ ingressShaped: false, mirrorRemoved: true }),
  });

  const conductLogs = async (
    client: () => ConductorClient,
    scenarioText = EXAMPLE_SCENARIO,
  ): Promise<string[]> => {
    const logs: string[] = [];
    const fc = fakeClock();
    await conductScenario({
      scenarioText,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "t",
      deps: { clientFactory: client, clock: fc.clock, sleep: fc.sleep, log: (l) => logs.push(l) },
    });
    return logs;
  };

  it("logs a profile shape as both directions, and a clear's removal as no warning", async () => {
    const logs = await conductLogs(() => ({
      ...recordingClient([]),
      applyNetem: async () => ({ ingressShaped: true, mirrorRemoved: false }),
      clearNetem: async () => ({ ingressShaped: false, mirrorRemoved: true }),
    }));
    expect(logs.filter((l) => l.includes("shaped both directions"))).toHaveLength(1);
    expect(logs.filter((l) => l.includes("UNSHAPED"))).toEqual([]);
  });

  it("logs an egress-only shape as such", async () => {
    const logs = await conductLogs(() => recordingClient([]));
    expect(logs.filter((l) => l.includes("shaped egress only"))).toHaveLength(1);
  });

  it("warns only for the shape that removed a mirror, never the clear", async () => {
    const logs = await conductLogs(mirrorRemovingClient);
    const warned = logs.filter((l) => l.includes("downlink is now UNSHAPED"));
    expect(warned).toHaveLength(1);
    expect(warned[0]).toContain("netem");
    expect(warned[0]).not.toContain("netem-clear");
    expect(warned[0]).not.toContain("startup");
  });

  it.each(["none", "clean"])("logs a netem profile: %s as a clear, not a shape", async (p) => {
    const logs = await conductLogs(
      mirrorRemovingClient,
      `timeline:\n  - { at: 0s, bot: 0, action: netem, profile: ${p} }\n`,
    );
    expect(logs.filter((l) => l.includes("shaped") || l.includes("UNSHAPED"))).toEqual([]);
  });

  it("records that a netem action tore down an ingress mirror", async () => {
    const logs: string[] = [];
    const fc = fakeClock();
    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "t",
      deps: {
        clientFactory: () => mirrorRemovingClient(),
        clock: fc.clock,
        sleep: fc.sleep,
        log: (l) => logs.push(l),
      },
    });
    expect(summary.failed).toBe(0);
    const disclosed = logs.filter((l) => l.includes("downlink is now UNSHAPED"));
    expect(
      disclosed.length,
      "a run whose record omits this credits the shaping change to the fix under test",
    ).toBeGreaterThan(0);
  });

  it("stays silent when no mirror was there to remove", async () => {
    const logs: string[] = [];
    const fc = fakeClock();
    await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "t",
      deps: {
        clientFactory: () => recordingClient([]),
        clock: fc.clock,
        sleep: fc.sleep,
        log: (l) => logs.push(l),
      },
    });
    expect(logs.filter((l) => l.includes("UNSHAPED"))).toEqual([]);
  });

  it("throws a ScenarioValidationError when a live run has no token", async () => {
    const fc = fakeClock();
    await expect(
      conductScenario({
        scenarioText: EXAMPLE_SCENARIO,
        hostOpts: HOST_OPTS,
        port: 8080,
        dryRun: false,
        token: undefined,
        deps: {
          clientFactory: () => recordingClient([]),
          clock: fc.clock,
          sleep: fc.sleep,
          log: () => {},
        },
      }),
    ).rejects.toBeInstanceOf(ScenarioValidationError);
  });
});

// ── readiness gate (awaitFleetReady, via conductScenario) ────────────────

const ONE_BOT_SCENARIO = "timeline:\n  - { at: 0s, bot: 0, action: unmute }\n";
const BOT0 = "videocall-bots-0.videocall-bots.bot-load.svc.cluster.local";
const BOT1 = "videocall-bots-1.videocall-bots.bot-load.svc.cluster.local";
const TWO_BOT_SCENARIO =
  "timeline:\n  - { at: 0s, bot: 0, action: unmute }\n  - { at: 0s, bot: 1, action: unmute }\n";
const JOINED = { ok: true, bots: 1, inMeeting: 1, pending: 0, expected: 1 };

describe("classifyHealthz (#2917)", () => {
  it.each([
    [null, "unreachable"],
    [{ ok: true, bots: 1 }, "legacy"],
    [{ ok: true, bots: 1, inMeeting: "1", expected: 1 }, "legacy"],
    [{ ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 1 }, "not-joined"],
    [{ ok: true, bots: 1, inMeeting: 0, pending: 1, expected: 1 }, "not-joined"],
    [{ ok: true, bots: 2, inMeeting: 1, pending: 1, expected: 2 }, "not-joined"],
    [{ ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 0 }, "not-joined"],
    [JOINED, "in-meeting"],
    [{ inMeeting: 1 }, "legacy"],
    [{ inMeeting: -1, expected: -1 }, "legacy"],
    [{ inMeeting: 0, expected: 1, pending: 2.5 }, "not-joined", { pending: 0 }],
  ] as Array<[unknown, string, object?]>)("%j -> %s", (body, state, fields) => {
    expect(classifyHealthz(body)).toMatchObject({ state, ...fields });
  });
});

describe("fetchHealthzHttp (#2917)", () => {
  it.each([
    [200, "<html>proxy</html>", "unreachable"],
    [503, JSON.stringify(JOINED), "unreachable"],
    [200, JSON.stringify(JOINED), "in-meeting"],
  ])("a %i with body %s reads %s", async (status, body, state) => {
    const pod = createHttpServer((_req, res) => {
      res.statusCode = status;
      res.end(body);
    });
    await new Promise<void>((r) => pod.listen(0, "127.0.0.1", r));
    try {
      const got = await fetchHealthzHttp("127.0.0.1", (pod.address() as AddressInfo).port);
      expect(classifyHealthz(got).state).toBe(state);
    } finally {
      pod.closeAllConnections();
      await new Promise((r) => pod.close(r));
    }
  });
});

describe("in-meeting readiness gate (#2917)", () => {
  it("probes every pod once even when the deadline has passed before the first sweep", async () => {
    let reads = 0;
    const probed: string[] = [];
    await expect(
      conductScenario({
        scenarioText: ONE_BOT_SCENARIO,
        hostOpts: HOST_OPTS,
        port: 8080,
        dryRun: false,
        token: "T",
        readinessTimeoutMs: 1,
        deps: {
          clientFactory: () => recordingClient([]),
          clock: { now: () => (reads++ === 0 ? 1000 : 5000) },
          sleep: async () => {},
          log: () => {},
          fetchHealthz: async (host) => {
            probed.push(host);
            return JOINED;
          },
        },
      }),
    ).resolves.toMatchObject({ fired: 1 });
    expect(probed).toEqual([BOT0]);
  });

  function gateRun(
    fetchHealthz: (host: string) => Promise<unknown>,
    extra: {
      scenarioText?: string;
      fleetSize?: number;
      allowLegacyHealthz?: boolean;
      logs?: string[];
      record?: string[];
    } = {},
  ): Promise<unknown> {
    const fc = fakeClock();
    return conductScenario({
      scenarioText: extra.scenarioText ?? TWO_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      readinessTimeoutMs: 3 * READINESS_POLL_INTERVAL_MS,
      fleetSize: extra.fleetSize,
      allowLegacyHealthz: extra.allowLegacyHealthz,
      deps: {
        clientFactory: () => recordingClient(extra.record ?? []),
        clock: fc.clock,
        sleep: fc.sleep,
        log: (l) => extra.logs?.push(l),
        fetchHealthz: (host) => fetchHealthz(host),
      },
    });
  }

  it("rejects a pod that answers 200 with no bot in the meeting, and issues no call", async () => {
    const record: string[] = [];
    const idle = { ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 1 };
    await expect(gateRun(async (h) => (h === BOT0 ? JOINED : idle), { record })).rejects.toThrow(
      `1 of 2 pod(s) not ready within 6000ms: ${BOT1} not in the meeting (inMeeting 0/1, still joining 0)`,
    );
    expect(record).toEqual([]);
  });

  it("names a still-joining pod and a pod whose image predates the field", async () => {
    const joining = { ok: true, bots: 1, inMeeting: 0, pending: 1, expected: 1 };
    const legacy = { ok: true, bots: 1 };
    await expect(gateRun(async (h) => (h === BOT0 ? joining : legacy))).rejects.toThrow(
      `2 of 2 pod(s) not ready within 6000ms: ${BOT0} not in the meeting (inMeeting 0/1, still joining 1); ${BOT1} /healthz reports no in-meeting count (image predates #2917; re-pin the fleet)`,
    );
  });

  it("anchors t0 once a joining pod joins, and logs the fleet in-meeting total", async () => {
    const logs: string[] = [];
    const record: string[] = [];
    let polls = 0;
    const joining = { ok: true, bots: 1, inMeeting: 0, pending: 1, expected: 1 };
    await gateRun(
      async (h) => {
        if (h !== BOT1) return JOINED;
        polls += 1;
        return polls > 1 ? JOINED : joining;
      },
      { logs, record },
    );
    expect(polls).toBe(2);
    expect(record).toEqual(["mute:false", "mute:false"]);
    expect(logs).toContain("conduct: all pods ready - 2 bot(s) in the meeting at t0");
  });

  const RAISE_HINT = "raise --readiness-timeout above BOT_MAX_JOIN_STAGGER_SECS";
  const joining = { ok: true, bots: 1, inMeeting: 0, pending: 1, expected: 1 };
  const legacy = { ok: true, bots: 1 };

  it("hints to raise --readiness-timeout only when every gated pod misses (#2387)", async () => {
    await expect(gateRun(async () => joining)).rejects.toThrow(RAISE_HINT);
    await expect(gateRun(async () => null)).rejects.toThrow(RAISE_HINT);
    const crashed = { ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 1 };
    await expect(gateRun(async () => crashed)).rejects.not.toThrow("--readiness-timeout");
    const partial = gateRun(async (h) => (h === BOT0 ? JOINED : joining));
    await expect(partial).rejects.toThrow("1 of 2 pod(s) not ready");
    await expect(partial).rejects.not.toThrow("--readiness-timeout");
  });

  it("points a legacy-image pod at a re-pin or --allow-legacy-healthz", async () => {
    await expect(gateRun(async (h) => (h === BOT0 ? JOINED : legacy))).rejects.toThrow(
      "re-pin the fleet to the current image, or pass --allow-legacy-healthz",
    );
  });

  it("--allow-legacy-healthz accepts a legacy pod but logs its bots UNVERIFIED", async () => {
    const logs: string[] = [];
    const record: string[] = [];
    await gateRun(async (h) => (h === BOT0 ? JOINED : legacy), {
      allowLegacyHealthz: true,
      logs,
      record,
    });
    expect(record).toEqual(["mute:false", "mute:false"]);
    expect(logs).toContain(
      `conduct: --allow-legacy-healthz: 1 pod(s) report no in-meeting count, so their bots are UNVERIFIED and not in the t0 total: ${BOT1}`,
    );
    expect(logs).toContain(
      "conduct: all pods ready - 1 bot(s) in the meeting at t0, 1 pod(s) UNVERIFIED",
    );
  });

  it("--allow-legacy-healthz still refuses a current pod that is not in the meeting", async () => {
    await expect(
      gateRun(async (h) => (h === BOT0 ? joining : legacy), { allowLegacyHealthz: true }),
    ).rejects.toThrow(`1 of 2 pod(s) not ready within 6000ms: ${BOT0} not in the meeting`);
  });

  it("names a pod whose every bot left on purpose", async () => {
    const left = { ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 0 };
    await expect(gateRun(async (h) => (h === BOT0 ? JOINED : left))).rejects.toThrow(
      `${BOT1} expects no bot (none launched, or every bot left on purpose)`,
    );
  });

  it("fails a pod that read ready and then lost its bot while another pod was joining", async () => {
    const record: string[] = [];
    const polls = new Map<string, number>();
    const gone = { ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 1 };
    const run = gateRun(
      async (h) => {
        const n = (polls.get(h) ?? 0) + 1;
        polls.set(h, n);
        if (h === BOT0) return n === 1 ? JOINED : gone;
        return n === 1 ? joining : JOINED;
      },
      { record },
    );
    await expect(run).rejects.toThrow(
      `1 of 2 pod(s) not ready within 6000ms: ${BOT0} not in the meeting (inMeeting 0/1, still joining 0)`,
    );
    expect(record).toEqual([]);
  });

  it("--fleet-size gates pods the timeline never names", async () => {
    const probed: string[] = [];
    const logs: string[] = [];
    await gateRun(
      async (h) => {
        probed.push(h);
        return JOINED;
      },
      { scenarioText: ONE_BOT_SCENARIO, fleetSize: 2, logs },
    );
    expect(probed).toEqual([BOT0, BOT1]);
    expect(logs.some((l) => l.includes("no --fleet-size"))).toBe(false);
  });

  it("warns that unnamed pods are unchecked when no fleet size is given", async () => {
    const logs: string[] = [];
    await gateRun(async () => JOINED, { scenarioText: ONE_BOT_SCENARIO, logs });
    expect(logs.some((l) => l.includes("no --fleet-size"))).toBe(true);
  });

  it("marks the in-meeting count UNVERIFIED when the gate is skipped", async () => {
    const logs: string[] = [];
    const fc = fakeClock();
    await conductScenario({
      scenarioText: ONE_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      readinessTimeoutMs: 0,
      deps: {
        clientFactory: () => recordingClient([]),
        clock: fc.clock,
        sleep: fc.sleep,
        log: (l) => logs.push(l),
        fetchHealthz: async () => JOINED,
      },
    });
    expect(logs.some((l) => l.includes("UNVERIFIED"))).toBe(true);
  });
});

describe("awaitFleetReady via conductScenario", () => {
  it("runs the schedule once every pod answers /healthz on the first sweep", async () => {
    const fc = fakeClock();
    const record: string[] = [];
    const probes: string[] = [];
    const summary = await conductScenario({
      scenarioText: TWO_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: {
        clientFactory: () => recordingClient(record),
        clock: fc.clock,
        sleep: fc.sleep,
        log: () => {},
        fetchHealthz: async (host, port) => {
          probes.push(`${host}:${port}`);
          return JOINED;
        },
      },
    });
    expect(summary).toEqual({
      planned: 2,
      fired: 2,
      failed: 0,
      dropped: 0,
      unreachable: 0,
      dryRun: false,
    });
    // Probed each unique pod exactly once, on the control port.
    expect(probes).toEqual([`${BOT0}:8080`, `${BOT1}:8080`]);
    expect(record).toEqual(["mute:false", "mute:false"]);
  });

  it("retries the slow pod and anchors t0 only after it answers", async () => {
    const fc = fakeClock(1000);
    const firedAt: number[] = [];
    // Bot 1's control API has not bound yet on the first sweep.
    let bot1Attempts = 0;
    const summary = await conductScenario({
      scenarioText: TWO_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: {
        clientFactory: () => ({
          ...recordingClient([]),
          mute: async () => void firedAt.push(fc.read()),
        }),
        clock: fc.clock,
        sleep: fc.sleep,
        log: () => {},
        fetchHealthz: async (host) => {
          if (host !== BOT1) return JOINED;
          bot1Attempts += 1;
          return bot1Attempts > 1 ? JOINED : null;
        },
      },
    });
    expect(summary).toEqual({
      planned: 2,
      fired: 2,
      failed: 0,
      dropped: 0,
      unreachable: 0,
      dryRun: false,
    });
    expect(bot1Attempts).toBe(2);
    // One poll interval elapsed before t0, so the t+0 actions fire late in
    // wall-clock terms — that is the point of the gate.
    expect(firedAt).toEqual([1000 + READINESS_POLL_INTERVAL_MS, 1000 + READINESS_POLL_INTERVAL_MS]);
  });

  it("throws naming the pods that never answered, and issues no control call", async () => {
    const fc = fakeClock();
    const record: string[] = [];
    await expect(
      conductScenario({
        scenarioText: TWO_BOT_SCENARIO,
        hostOpts: HOST_OPTS,
        port: 8080,
        dryRun: false,
        token: "T",
        readinessTimeoutMs: 10_000,
        deps: {
          clientFactory: () => recordingClient(record),
          clock: fc.clock,
          sleep: fc.sleep,
          log: () => {},
          fetchHealthz: async (host) => (host === BOT0 ? JOINED : null),
        },
      }),
    ).rejects.toThrow(
      new RegExp(`1 of 2 pod\\(s\\) not ready within 10000ms: ${BOT1} never answered /healthz`),
    );
    // A partially-bound set of scheduled pods must not produce a run at all.
    expect(record).toEqual([]);
  });

  it("skips the gate when deps carry no fetchHealthz (no sleep, no error)", async () => {
    const fc = fakeClock();
    let sleeps = 0;
    const summary = await conductScenario({
      scenarioText: ONE_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: {
        clientFactory: () => recordingClient([]),
        clock: fc.clock,
        sleep: async (ms) => {
          sleeps += 1;
          await fc.sleep(ms);
        },
        log: () => {},
      },
    });
    expect(summary).toEqual({
      planned: 1,
      fired: 1,
      failed: 0,
      dropped: 0,
      unreachable: 0,
      dryRun: false,
    });
    expect(sleeps).toBe(0);
  });

  it("skips the gate at readinessTimeoutMs 0 without probing", async () => {
    const fc = fakeClock();
    let probes = 0;
    const summary = await conductScenario({
      scenarioText: ONE_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      readinessTimeoutMs: 0,
      deps: {
        clientFactory: () => recordingClient([]),
        clock: fc.clock,
        sleep: fc.sleep,
        log: () => {},
        // Never ready — the run must still proceed because the gate is off.
        fetchHealthz: async () => {
          probes += 1;
          return null;
        },
      },
    });
    expect(summary).toEqual({
      planned: 1,
      fired: 1,
      failed: 0,
      dropped: 0,
      unreachable: 0,
      dryRun: false,
    });
    expect(probes).toBe(0);
  });

  it("does not probe on a --dry-run (returns before the gate)", async () => {
    let probes = 0;
    const summary = await conductScenario({
      scenarioText: ONE_BOT_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: true,
      deps: {
        clientFactory: () => recordingClient([]),
        clock: {
          now: () => {
            throw new Error("dry-run must not read the clock");
          },
        },
        sleep: async () => {},
        log: () => {},
        fetchHealthz: async () => {
          probes += 1;
          return JOINED;
        },
      },
    });
    expect(summary.dryRun).toBe(true);
    expect(probes).toBe(0);
  });

  it("gives up at the deadline instead of polling forever", async () => {
    const fc = fakeClock(0);
    let probes = 0;
    await expect(
      conductScenario({
        scenarioText: ONE_BOT_SCENARIO,
        hostOpts: HOST_OPTS,
        port: 8080,
        dryRun: false,
        token: "T",
        readinessTimeoutMs: 5 * READINESS_POLL_INTERVAL_MS,
        deps: {
          clientFactory: () => recordingClient([]),
          clock: fc.clock,
          sleep: fc.sleep,
          log: () => {},
          fetchHealthz: async () => {
            probes += 1;
            return null;
          },
        },
      }),
    ).rejects.toThrow(/never answered \/healthz/);
    // Bounded: one probe per poll interval up to the deadline, not unbounded.
    expect(probes).toBe(5);
    expect(fc.read()).toBe(5 * READINESS_POLL_INTERVAL_MS);
  });
});

describe("conductScenario (--dry-run)", () => {
  it("prints the resolved schedule and issues no calls (factory/clock/sleep untouched)", async () => {
    const logs: string[] = [];
    let sleeps = 0;
    const throwingFactory: ConductorClientFactory = () => {
      throw new Error("dry-run must not construct a client");
    };
    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: true,
      deps: {
        clientFactory: throwingFactory,
        clock: {
          now: () => {
            throw new Error("dry-run must not read the clock");
          },
        },
        sleep: async () => void (sleeps += 1),
        log: (l) => logs.push(l),
      },
    });
    expect(summary).toEqual({
      planned: 7,
      fired: 0,
      failed: 0,
      dropped: 0,
      unreachable: 0,
      dryRun: true,
    });
    expect(sleeps).toBe(0);
    // The resolved schedule lines name the pod host + action + offset.
    expect(
      logs.some(
        (l) =>
          l.includes("videocall-bots-2.videocall-bots.bot-load.svc.cluster.local") &&
          l.includes("netem (lossy_mobile)") &&
          l.includes("t+20000ms"),
      ),
    ).toBe(true);
    expect(logs.some((l) => l.includes("no control calls issued"))).toBe(true);
  });
});

// ── HTTP client integration against a real control server ────────────────

function fakeTask(overrides: Partial<BotTask> = {}): BotTask {
  return {
    botId: generateBotId(),
    meetingURL: "https://example.com/meeting/X",
    participant: "alice",
    displayName: "Alice",
    headless: false,
    authBackend: "jwt",
    sourceGeometry: SD_SOURCE,
    cameraCycle: null,
    storageStateFile: null,
    ssoStateFile: null,
    manifest: null,
    runDir: null,
    ttl: 300_000,
    network: null,
    ...overrides,
  };
}

describe("httpConductorClientFactory (against a live control server)", () => {
  let handle: ControlServerHandle;
  let token: string;
  let calls: string[];
  let liveBotId: string;
  let registry: Map<string, BotRegistryEntry>;

  beforeEach(async () => {
    token = generateToken();
    calls = [];
    registry = new Map<string, BotRegistryEntry>();
    // A terminated bot lingers in the registry's retention window; the
    // client must skip it and target the live one.
    const dead = newRegistryEntry(fakeTask({ participant: "zombie" }));
    dead.status = "done";
    const live = newRegistryEntry(fakeTask({ participant: "alice" }));
    live.status = "in-meeting";
    liveBotId = live.botId;
    registry.set(dead.botId, dead);
    registry.set(live.botId, live);

    handle = await startControlServer({
      port: 0,
      token,
      surface: {
        getRegistry: () => registry,
        expectedBots: () => 0,
        triggerLeave: async (id) => void calls.push(`leave:${id}`),
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async (id, m) => void calls.push(`mic:${id}:${m}`),
        setCameraOff: async (id, c) => void calls.push(`cam:${id}:${c}`),
        setScreenShare: async (id, s) => void calls.push(`share:${id}:${s}`),
        setNetem: async (action) => {
          calls.push(`netem:${action.op}:${action.label}`);
          return {
            commands: [["tc", "qdisc"]],
            label: action.label,
            op: action.op,
            ingressShaped: false,
            mirrorRemoved: false,
            readback: {},
          };
        },
        duplicateBot: async () => "x",
        launchOne: async () => "x",
      },
    });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("resolves the single live bot and hits the matching route per action", async () => {
    const client = httpConductorClientFactory()({ host: "127.0.0.1", port: handle.port, token });
    await client.mute(true);
    await client.setCameraOff(true);
    await client.setScreenShare(true);
    await client.applyNetem({ profile: "lossy_mobile" });
    await client.clearNetem();
    await client.leave();
    expect(calls).toEqual([
      `mic:${liveBotId}:true`,
      `cam:${liveBotId}:true`,
      `share:${liveBotId}:true`,
      "netem:shape:lossy_mobile",
      "netem:clear:clear",
      `leave:${liveBotId}`,
    ]);
  });

  /** A pod whose `GET /bots` lists `bots`; a control on `old` answers `oldStatus` with `oldBody`. */
  async function withPod(
    oldStatus: number,
    oldBody: unknown,
    run: (pod: {
      client: ConductorClient;
      posts: string[];
      lookups: () => number;
      list: (b: Array<{ botId: string; status: string }>) => void;
    }) => Promise<void>,
  ): Promise<void> {
    let bots = [{ botId: "old", status: "in-meeting" }];
    let lookups = 0;
    const posts: string[] = [];
    const pod = createHttpServer((req, res) => {
      res.setHeader("content-type", "application/json");
      if (req.method === "GET" && req.url === "/bots") {
        lookups += 1;
        res.end(JSON.stringify({ bots }));
        return;
      }
      posts.push(req.url ?? "");
      const old = req.url?.startsWith("/bots/old/") === true;
      res.statusCode = old ? oldStatus : 200;
      res.end(JSON.stringify(old ? oldBody : {}));
    });
    await new Promise<void>((r) => pod.listen(0, "127.0.0.1", r));
    try {
      const client = httpConductorClientFactory()({
        host: "127.0.0.1",
        port: (pod.address() as AddressInfo).port,
        token,
      });
      await run({ client, posts, lookups: () => lookups, list: (b) => (bots = b) });
    } finally {
      pod.closeAllConnections();
      await new Promise((r) => pod.close(r));
    }
  }

  const REPLACED = [
    { botId: "old", status: "failed" },
    { botId: "new", status: "in-meeting" },
  ];

  it("re-resolves the bot after a 409, so a replacement bot gets the next control", async () => {
    await withPod(409, { error: "bot old is not yet in-meeting" }, async (pod) => {
      await expect(pod.client.mute(true)).rejects.toMatchObject({ status: 409 });
      pod.list(REPLACED);
      await pod.client.mute(false);
      expect(pod.posts).toEqual(["/bots/old/mute", "/bots/new/mute"]);
    });
  });

  it("reads the control server's 404 for a dropped bot as not joined, then reaches its replacement", async () => {
    const client = httpConductorClientFactory()({ host: "127.0.0.1", port: handle.port, token });
    await client.mute(true);
    registry.delete(liveBotId);
    const next = newRegistryEntry(fakeTask({ participant: "bob" }));
    next.status = "in-meeting";
    registry.set(next.botId, next);
    await expect(client.mute(false)).rejects.toBeInstanceOf(BotNotJoinedError);
    await client.mute(true);
    expect(calls).toEqual([`mic:${liveBotId}:true`, `mic:${next.botId}:true`]);
  });

  it("treats a 404 for a dropped bot id as not joined and re-resolves the replacement", async () => {
    await withPod(404, { error: "bot old not found" }, async (pod) => {
      await expect(pod.client.mute(true)).rejects.toBeInstanceOf(BotNotJoinedError);
      pod.list([{ botId: "new", status: "in-meeting" }]);
      await pod.client.mute(false);
      expect(pod.posts).toEqual(["/bots/old/mute", "/bots/new/mute"]);
    });
  });

  it.each([
    [500, { error: "boom" }],
    [404, { error: "no route for POST /bots/old/mute" }],
  ])("keeps the cached bot id after a %i that is not a dropped bot", async (status, body) => {
    await withPod(status, body, async (pod) => {
      const err = await pod.client.mute(true).catch((e: unknown) => e);
      expect(err).toBeInstanceOf(CtlHttpError);
      expect(err).not.toBeInstanceOf(BotNotJoinedError);
      pod.list(REPLACED);
      await pod.client.mute(false).catch(() => {});
      expect(pod.lookups()).toBe(1);
      expect(pod.posts).toEqual(["/bots/old/mute", "/bots/old/mute"]);
    });
  });

  it("resolves the bot id exactly once across multiple meeting-control calls", async () => {
    const client = httpConductorClientFactory()({ host: "127.0.0.1", port: handle.port, token });
    await Promise.all([client.mute(true), client.setCameraOff(false), client.setScreenShare(true)]);
    // All three routed to the same (single) live bot.
    expect(calls.filter((c) => c.includes(liveBotId)).length).toBe(3);
  });

  it("errors clearly on a meeting-control call when no bot is registered", async () => {
    const emptyToken = generateToken();
    const emptyRegistry = new Map<string, BotRegistryEntry>();
    const emptyHandle = await startControlServer({
      port: 0,
      token: emptyToken,
      surface: {
        getRegistry: () => emptyRegistry,
        expectedBots: () => 0,
        triggerLeave: async () => {},
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async () => {},
        setCameraOff: async () => {},
        setScreenShare: async () => {},
        setNetem: async (action) => ({
          commands: [["tc"]],
          label: action.label,
          op: action.op,
          ingressShaped: action.op === "shape",
          mirrorRemoved: action.op === "clear",
          readback: {},
        }),
        duplicateBot: async () => "x",
        launchOne: async () => "x",
      },
    });
    try {
      const client = httpConductorClientFactory()({
        host: "127.0.0.1",
        port: emptyHandle.port,
        token: emptyToken,
      });
      // netem needs no bot id — succeeds against an empty registry. The outcome
      // is asserted end to end: the server's mirrorRemoved must reach the caller.
      await expect(client.clearNetem()).resolves.toEqual({
        ingressShaped: false,
        mirrorRemoved: true,
      });
      await expect(client.applyNetem({ profile: "lossy_mobile" })).resolves.toEqual({
        ingressShaped: true,
        mirrorRemoved: false,
      });
      // mute needs a bot id — surfaces a clear error.
      await expect(client.mute(true)).rejects.toThrow(/no bot registered/);
      await expect(client.mute(true)).rejects.toBeInstanceOf(BotNotJoinedError);
    } finally {
      await emptyHandle.close();
    }
  });

  it("retries bot-id resolution after a failed lookup (does not cache the rejection)", async () => {
    // Locks the botId() `.catch` reset: a first action at t=0 can hit the pod
    // mid-boot (registry momentarily empty) and GET /bots refuses. That rejected
    // lookup must NOT be memoized, or every later mute/camera/share/leave for
    // this pod fails for the whole scenario. Reverting the reset breaks this.
    const retryToken = generateToken();
    const retryRegistry = new Map<string, BotRegistryEntry>();
    const retryCalls: string[] = [];
    const retryHandle = await startControlServer({
      port: 0,
      token: retryToken,
      surface: {
        getRegistry: () => retryRegistry,
        expectedBots: () => 0,
        triggerLeave: async () => {},
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async (id, m) => void retryCalls.push(`mic:${id}:${m}`),
        setCameraOff: async () => {},
        setScreenShare: async () => {},
        setNetem: async (action) => ({
          commands: [["tc"]],
          label: action.label,
          op: action.op,
          ingressShaped: false,
          mirrorRemoved: false,
          readback: {},
        }),
        duplicateBot: async () => "x",
        launchOne: async () => "x",
      },
    });
    try {
      const client = httpConductorClientFactory()({
        host: "127.0.0.1",
        port: retryHandle.port,
        token: retryToken,
      });
      // First call: registry empty → rejects (and must clear the memoized promise).
      await expect(client.mute(true)).rejects.toThrow(/no bot registered/);
      // A bot now finishes starting up and registers.
      const live = newRegistryEntry(fakeTask({ participant: "alice" }));
      live.status = "in-meeting";
      retryRegistry.set(live.botId, live);
      // Second call MUST re-resolve the id (not replay the cached rejection).
      await client.mute(false);
      expect(retryCalls).toEqual([`mic:${live.botId}:false`]);
    } finally {
      await retryHandle.close();
    }
  });
});

// Ensure ScheduledCall stays structurally exercised (guards accidental
// field removal that would still type-check elsewhere).
describe("ScheduledCall shape", () => {
  it("carries atMs/bot/host/call/seq", () => {
    const s = parseScenario("timeline:\n  - { at: 1s, bot: 0, action: leave }\n");
    const [sc] = buildSchedule(s.entries, HOST_OPTS);
    const shape: ScheduledCall = sc;
    expect(shape).toMatchObject({
      atMs: 1000,
      bot: 0,
      host: "videocall-bots-0.videocall-bots.bot-load.svc.cluster.local",
      call: { kind: "leave" },
    });
    expect(typeof shape.seq).toBe("number");
  });
});
