import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { BotTask } from "../orchestrator";
import { generateToken } from "./auth";
import {
  type Clock,
  type ConductorClient,
  type ConductorClientFactory,
  type ControlCall,
  type HostResolveOptions,
  type ScheduledCall,
  applyAction,
  buildSchedule,
  conductScenario,
  httpConductorClientFactory,
  parseAtDuration,
  parseScenario,
  resolveBotHost,
  ScenarioValidationError,
} from "./conduct";
import { generateBotId, newRegistryEntry, type BotRegistryEntry } from "./registry";
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
    applyNetem: async (b) => void record.push(`netem:${JSON.stringify(b)}`),
    clearNetem: async () => void record.push("netem-clear"),
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
      applyNetem: async (b) =>
        void fired.push({
          offset: fc.read() - 1000,
          host: config.host,
          label: `netem:${JSON.stringify(b)}`,
        }),
      clearNetem: async () =>
        void fired.push({ offset: fc.read() - 1000, host: config.host, label: "netem-clear" }),
    });

    const summary = await conductScenario({
      scenarioText: EXAMPLE_SCENARIO,
      hostOpts: HOST_OPTS,
      port: 8080,
      dryRun: false,
      token: "T",
      deps: { clientFactory: factory, clock: fc.clock, sleep: fc.sleep, log: () => {} },
    });

    expect(summary).toEqual({ planned: 7, fired: 7, failed: 0, dryRun: false });
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
    expect(summary).toEqual({ planned: 7, fired: 5, failed: 2, dryRun: false });
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
    expect(summary).toEqual({ planned: 7, fired: 0, failed: 0, dryRun: true });
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

  beforeEach(async () => {
    token = generateToken();
    calls = [];
    const registry = new Map<string, BotRegistryEntry>();
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
        triggerLeave: async (id) => void calls.push(`leave:${id}`),
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async (id, m) => void calls.push(`mic:${id}:${m}`),
        setCameraOff: async (id, c) => void calls.push(`cam:${id}:${c}`),
        setScreenShare: async (id, s) => void calls.push(`share:${id}:${s}`),
        setNetem: async (action) => {
          calls.push(`netem:${action.op}:${action.label}`);
          return { argv: ["tc", "qdisc"], label: action.label, op: action.op };
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
        triggerLeave: async () => {},
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async () => {},
        setCameraOff: async () => {},
        setScreenShare: async () => {},
        setNetem: async (action) => ({ argv: ["tc"], label: action.label, op: action.op }),
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
      // netem needs no bot id — succeeds against an empty registry.
      await expect(client.clearNetem()).resolves.toBeUndefined();
      // mute needs a bot id — surfaces a clear error.
      await expect(client.mute(true)).rejects.toThrow(/no bot registered/);
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
        triggerLeave: async () => {},
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async (id, m) => void retryCalls.push(`mic:${id}:${m}`),
        setCameraOff: async () => {},
        setScreenShare: async () => {},
        setNetem: async (action) => ({ argv: ["tc"], label: action.label, op: action.op }),
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
