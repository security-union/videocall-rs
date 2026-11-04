import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { inspect } from "node:util";

import { afterEach, describe, expect, it, vi } from "vitest";

import { DECODE_BUDGET_WAIVER_NOTE } from "./decode-budget";
import { MAX_TIMER_MS } from "./dashboard";
import { RUN_END_AFTER_DEADLINE_MS, type BotTask } from "./orchestrator";
import { SPEECH_ENV_NAMES, SPEECH_PLAN_DEFAULT } from "./speech-plan";
import { SKIP_CANVAS_PAINT_ENV } from "./receiver-caps";

const mocks = vi.hoisted(() => ({ runBotsToCompletion: vi.fn() }));

// Stop at the orchestrator boundary: the tasks it is HANDED are what the CLI
// resolved, and no browser launches.
vi.mock("./orchestrator", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./orchestrator")>()),
  runBotsToCompletion: mocks.runBotsToCompletion,
}));

vi.mock("@playwright/test", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@playwright/test")>()),
  chromium: {
    launch: vi.fn(() => Promise.reject(new Error("chromium.launch stubbed"))),
  },
}));

vi.mock("./auth/sso-capture", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./auth/sso-capture")>()),
  captureSsoStateInteractive: vi.fn(() =>
    Promise.reject(new Error("captureSsoStateInteractive stubbed")),
  ),
}));

vi.mock("./dashboard", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./dashboard")>()),
  startDashboardServer: vi.fn(() =>
    Promise.resolve({ port: 5174, close: () => Promise.resolve() }),
  ),
  spawnViteDev: vi.fn(),
}));

vi.mock("./resource/session", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./resource/session")>()),
  ResourceCaptureSession: class {
    readonly label = "cli-test";
    startLocal(): void {}
    finalize(): Promise<null> {
      return Promise.resolve(null);
    }
  },
  RemoteResourceManager: class {},
}));

const workdirs: string[] = [];

function workdir(): string {
  const dir = mkdtempSync(join(tmpdir(), "bots-cli-"));
  workdirs.push(dir);
  return dir;
}

async function runCli(
  args: string[],
  env: Record<string, string | undefined> = {},
): Promise<{
  tasks: BotTask[];
  exits: number[];
  errors: string[];
  runOpts: { onEncoderFps?: unknown };
  logs: string[];
}> {
  const exits: number[] = [];
  const errors: string[] = [];
  const logs: string[] = [];
  let tasks: BotTask[] = [];
  let runOpts: { onEncoderFps?: unknown } = {};
  mocks.runBotsToCompletion.mockReset();
  mocks.runBotsToCompletion.mockImplementation(
    async (opts: { tasks: BotTask[]; onEncoderFps?: unknown }) => {
      tasks = [...opts.tasks];
      runOpts = opts;
    },
  );
  const spies = [
    vi.spyOn(process, "exit").mockImplementation(((code?: number) => {
      exits.push(code ?? 0);
      return undefined as never;
    }) as never),
    vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
      errors.push(a.map(String).join(" "));
    }),
    vi.spyOn(console, "log").mockImplementation((...a: unknown[]) => {
      logs.push(a.map(String).join(" "));
    }),
    vi.spyOn(console, "warn").mockImplementation(() => {}),
  ];
  const argv = process.argv;
  // Every key this call touches, so a case cannot leak env into its siblings.
  const savedEnv = new Map(Object.keys(env).map((k) => [k, process.env[k]]));
  process.argv = ["node", "cli.ts", "run", "--manifest", "", ...args];
  for (const [k, v] of Object.entries(env)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  try {
    vi.resetModules();
    await import("./cli");
    await vi.waitFor(() =>
      expect(mocks.runBotsToCompletion.mock.calls.length + exits.length).toBeGreaterThan(0),
    );
  } finally {
    process.argv = argv;
    for (const [k, v] of savedEnv) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    for (const spy of spies) spy.mockRestore();
  }
  return { tasks, exits, errors, runOpts, logs };
}

/**
 * Drive the real `conduct` subcommand. `globalThis.fetch` is the ONLY seam the
 * readiness probe can reach, so recording it observes the CLI's own wiring.
 */
async function runConduct(
  args: string[],
  healthz: () => Response = () => ({ ok: false }) as Response,
): Promise<{
  probed: string[];
  exits: number[];
  errors: string[];
}> {
  const probed: string[] = [];
  const exits: number[] = [];
  const errors: string[] = [];
  const spies = [
    vi.spyOn(process, "exit").mockImplementation(((code?: number) => {
      exits.push(code ?? 0);
      return undefined as never;
    }) as never),
    vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
      errors.push(a.map(String).join(" "));
    }),
    vi.spyOn(console, "log").mockImplementation(() => {}),
    vi.spyOn(globalThis, "fetch").mockImplementation((async (url: unknown) => {
      probed.push(String(url));
      return healthz();
    }) as typeof fetch),
  ];
  const argv = process.argv;
  const savedToken = process.env.BOT_CTL_TOKEN;
  process.argv = ["node", "cli.ts", "conduct", ...args];
  process.env.BOT_CTL_TOKEN = "conduct-test-token";
  try {
    vi.resetModules();
    await import("./cli");
    await vi.waitFor(() => expect(exits.length).toBeGreaterThan(0), { timeout: 10_000 });
  } finally {
    process.argv = argv;
    if (savedToken === undefined) delete process.env.BOT_CTL_TOKEN;
    else process.env.BOT_CTL_TOKEN = savedToken;
    for (const spy of spies) spy.mockRestore();
  }
  return { probed, exits, errors };
}

afterEach(() => {
  for (const dir of workdirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function baseArgs(): string[] {
  return ["--meeting-url", "http://127.0.0.1:3001/meeting/GeomTest", "--assets-dir", workdir()];
}

describe("bots-app run — join media and participant record (#2914)", () => {
  const JOIN_ENV = {
    BOT_JOIN_CAMERA_OFF: undefined,
    BOT_JOIN_MIC_MUTED: undefined,
    BOT_ROLE: undefined,
    BOT_NETEM_APPLIED: undefined,
    BOT_POD_ORDINAL: undefined,
    BOT_NODE_NAME: undefined,
    BOT_JOIN_STAGGER_MS: undefined,
    BOTS_IMAGE_REVISION: undefined,
  };

  it("defaults to camera and mic on", async () => {
    const { tasks } = await runCli([...baseArgs(), "--participant", "alice"], JOIN_ENV);
    expect(tasks[0].joinMedia).toEqual({ camera: true, mic: true });
  });

  it("carries --join-camera-off / --join-mic-muted into the task", async () => {
    const { tasks } = await runCli(
      [...baseArgs(), "--participant", "alice", "--join-camera-off", "--join-mic-muted"],
      JOIN_ENV,
    );
    expect(tasks[0].joinMedia).toEqual({ camera: false, mic: false });
  });

  it("reads the env when the flags are absent", async () => {
    const { tasks } = await runCli([...baseArgs(), "--participant", "alice"], {
      ...JOIN_ENV,
      BOT_JOIN_MIC_MUTED: "true",
    });
    expect(tasks[0].joinMedia).toEqual({ camera: true, mic: false });
  });

  it("exits 2 when a camera-off join meets the camera duty cycle", async () => {
    const { exits, errors } = await runCli(
      [...baseArgs(), "--participant", "alice", "--join-camera-off"],
      {
        ...JOIN_ENV,
        BOT_CAMERA_ON_SECS_MIN: "5",
        BOT_CAMERA_ON_SECS_MAX: "10",
        BOT_CAMERA_OFF_SECS_MIN: "5",
        BOT_CAMERA_OFF_SECS_MAX: "10",
      },
    );
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain("cannot be combined with the camera duty cycle");
  });

  it("wires the participant recorder into the run", async () => {
    const assetsDir = workdir();
    const { tasks, runOpts } = await runCli(
      [
        "--meeting-url",
        "http://127.0.0.1:3001/meeting/GeomTest",
        "--assets-dir",
        assetsDir,
        "--participant",
        "alice",
        "--auth",
        "jwt",
        "--role",
        "probe-mix",
        "--join-camera-off",
      ],
      {
        ...JOIN_ENV,
        BOT_NETEM_APPLIED: "good_4g",
        BOT_POD_ORDINAL: "2",
        BOT_NODE_NAME: "node-b",
        BOT_JOIN_STAGGER_MS: "17000",
        BOTS_IMAGE_REVISION: "cafe",
      },
    );
    const hooks = runOpts as {
      onRegister: (t: BotTask) => void;
      onJoin: (id: string, at: number, m: { camera: boolean | null; mic: boolean | null }) => void;
      onFinish: (id: string, at: number, reason: string | undefined) => void;
      onRejoin: (id: string, network: string | null) => void;
      onNetem: (action: { op: "clear"; label: string }, at: number, error?: unknown) => void;
    };
    const id = tasks[0].botId;
    hooks.onRegister(tasks[0]);
    const atRegister = JSON.parse(
      readFileSync(join(assetsDir, "participants", `${id}.json`), "utf8"),
    );
    expect(atRegister.participant.network).toMatchObject({ profile: "good_4g", shaped: true });
    hooks.onJoin(id, 1_000_500, { camera: false, mic: true });
    hooks.onRejoin(id, "dialup");
    hooks.onNetem({ op: "clear", label: "clear" }, 1_030_000);
    expect(
      JSON.parse(readFileSync(join(assetsDir, "participants", `${id}.json`), "utf8")).participant
        .network,
    ).toMatchObject({ profile: "dialup", shaper: "netsim" });
    hooks.onNetem({ op: "clear", label: "clear" }, 1_040_000, new Error("tc failed"));
    hooks.onFinish(id, 1_060_000, "ttl-expired");
    const rec = JSON.parse(readFileSync(join(assetsDir, "participants", `${id}.json`), "utf8"));
    expect(rec).toMatchObject({
      schema: "bots-app-participant-record/v0",
      outcome: "ttl-expired",
      participant: {
        user_id: "alice@bots-app.local",
        role: "probe-mix",
        publishes: { camera: false, mic: true, screen: false },
        network: { profile: "unknown", shaped: null, direction: null, shaper: null },
        transport_intended: "auto",
        join_ts: 1000.5,
        leave_ts: 1060,
        placement: { ordinal: 2, node: "node-b" },
        stagger_ms: 17_000,
      },
      code: { images: { "bots-app": "cafe" } },
    });
    expect(rec.unverified.map((u: { field: string }) => u.field)).toContain("rejoin");
  });

  it("exits 2 on a malformed --role", async () => {
    const { exits, errors } = await runCli(
      [...baseArgs(), "--participant", "alice", "--role", "probe mix"],
      JOIN_ENV,
    );
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain("--role / BOT_ROLE must match");
  });
});

describe("bots-app run — run deadline (#2914)", () => {
  it("puts --deadline on every task in epoch ms, and refuses a malformed one", async () => {
    const { tasks, runOpts } = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--deadline",
      "1791000835",
    ]);
    expect(tasks.map((t) => t.runDeadlineMs)).toEqual([1_791_000_835_000]);
    expect((runOpts as { runDeadlineMs?: number }).runDeadlineMs).toBe(1_791_000_835_000);
    const bad = await runCli([...baseArgs(), "--participant", "alice", "--deadline", "soon"]);
    expect(bad.exits).toContain(2);
    expect(bad.errors.join("\n")).toContain("--deadline must be epoch seconds");
    const none = await runCli([...baseArgs(), "--participant", "alice"]);
    expect(none.tasks.map((t) => t.runDeadlineMs)).toEqual([null]);
  });

  it("puts --join-deadline on every task in epoch ms, and refuses a malformed or distant one", async () => {
    const at = Math.floor(Date.now() / 1000) + 300;
    const { tasks } = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--join-deadline",
      `${at}`,
    ]);
    expect(tasks.map((t) => t.joinDeadlineMs)).toEqual([at * 1000]);
    for (const [value, message] of [
      ["soon", "--join-deadline must be epoch seconds"],
      [`${at * 1000}`, "is over 24 days away"],
    ]) {
      const bad = await runCli([...baseArgs(), "--participant", "alice", "--join-deadline", value]);
      expect(bad.exits).toContain(2);
      expect(bad.errors.join("\n")).toContain(message);
    }
    const none = await runCli([...baseArgs(), "--participant", "alice"]);
    expect(none.tasks.map((t) => t.joinDeadlineMs)).toEqual([null]);
  });

  it("refuses a --deadline too far away for a timer, such as epoch ms", async () => {
    const nowS = Math.floor(Date.now() / 1000);
    const ms = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--deadline",
      `${nowS * 1000}`,
    ]);
    expect(ms.exits).toContain(2);
    expect(ms.errors.join("\n")).toContain("is over 24 days away");
    const near = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--deadline",
      `${nowS + 24 * 86_400}`,
    ]);
    expect(near.exits).not.toContain(2);
    expect(near.tasks.map((t) => t.runDeadlineMs)).toEqual([(nowS + 24 * 86_400) * 1000]);
  });

  it("leaves the run's timers past the deadline inside the timer range", async () => {
    const deadlineS = Math.floor(Date.now() / 1000) + 24 * 86_400;
    const limit = MAX_TIMER_MS - RUN_END_AFTER_DEADLINE_MS;
    const at = async (msAhead: number) => {
      const now = vi.spyOn(Date, "now").mockReturnValue(deadlineS * 1000 - msAhead);
      try {
        return await runCli([
          ...baseArgs(),
          "--participant",
          "alice",
          "--deadline",
          `${deadlineS}`,
        ]);
      } finally {
        now.mockRestore();
      }
    };
    const max = await at(limit);
    expect(max.exits).not.toContain(2);
    expect(max.tasks.map((t) => t.runDeadlineMs)).toEqual([deadlineS * 1000]);
    const over = await at(limit + 1);
    expect(over.exits).toContain(2);
    expect(over.errors.join("\n")).toContain("is over 24 days away");
  });
});

describe("bots-app run — source geometry reaches the bot task (#2236)", () => {
  it.each([
    [0, 640, 480],
    [1, 1280, 720],
    [7, 1280, 720],
    [8, 640, 480],
  ])("gives index %i a %ix%i source", async (index, width, height) => {
    const { tasks, exits } = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--bot-index",
      String(index),
    ]);
    expect(exits).toEqual([0]);
    expect(tasks).toHaveLength(1);
    expect(tasks[0].sourceGeometry).toEqual({ width, height });
  });

  it("defaults to 640x480 when neither the flag nor BOT_INDEX is set", async () => {
    const { tasks } = await runCli([...baseArgs(), "--participant", "alice"], {
      BOT_INDEX: undefined,
    });
    expect(tasks[0].sourceGeometry).toEqual({ width: 640, height: 480 });
  });

  it("reads BOT_INDEX from the env, and lets the flag win over it", async () => {
    const fromEnv = await runCli([...baseArgs(), "--participant", "alice"], { BOT_INDEX: "1" });
    expect(fromEnv.tasks[0].sourceGeometry).toEqual({ width: 1280, height: 720 });

    const flagWins = await runCli([...baseArgs(), "--participant", "alice", "--bot-index", "2"], {
      BOT_INDEX: "1",
    });
    expect(flagWins.tasks[0].sourceGeometry).toEqual({ width: 640, height: 480 });
  });

  it("walks the indices upward from --bot-index across a multi-bot run", async () => {
    const dir = workdir();
    const configPath = join(dir, "meeting.yaml");
    writeFileSync(
      configPath,
      [
        "meeting_url: http://127.0.0.1:3001/meeting/GeomTest",
        "bots:",
        "  - participant: alice",
        "  - participant: bob",
        "  - participant: carol",
        "",
      ].join("\n"),
      "utf8",
    );
    const { tasks } = await runCli([
      "--assets-dir",
      dir,
      "--config",
      configPath,
      "--bot-index",
      "5",
    ]);
    expect(tasks.map((t) => t.participant)).toEqual(["alice", "bob", "carol"]);
    expect(tasks.map((t) => t.sourceGeometry)).toEqual([
      { width: 640, height: 480 },
      { width: 640, height: 480 },
      { width: 1280, height: 720 },
    ]);
  });

  it("gives each bot its fleet index and the whole process's rotation size (issue 3072)", async () => {
    const dir = workdir();
    const configPath = join(dir, "meeting.yaml");
    writeFileSync(
      configPath,
      ["meeting_url: http://127.0.0.1:3001/meeting/TalkTest", "bots:"]
        .concat(["alice", "bob", "carol", "dave"].map((p) => `  - participant: ${p}`))
        .join("\n"),
      "utf8",
    );
    const { tasks } = await runCli(["--assets-dir", dir, "--config", configPath], {
      BOT_INDEX: undefined,
      BOT_FLEET_SIZE: undefined,
    });
    expect(tasks.map((t) => t.talkingClock)).toEqual(
      [0, 1, 2, 3].map((index) => ({
        index,
        fleetSize: 4,
      })),
    );
  });

  it("takes the rotation size from BOT_FLEET_SIZE, the flag winning (issue 3072)", async () => {
    const fromEnv = await runCli([...baseArgs(), "--participant", "alice", "--bot-index", "2"], {
      BOT_FLEET_SIZE: "8",
    });
    expect(fromEnv.tasks[0].talkingClock).toEqual({ index: 2, fleetSize: 8 });
    const flagWins = await runCli(
      [...baseArgs(), "--participant", "alice", "--bot-index", "2", "--fleet-size", "5"],
      { BOT_FLEET_SIZE: "8" },
    );
    expect(flagWins.tasks[0].talkingClock).toEqual({ index: 2, fleetSize: 5 });
    const unset = await runCli([...baseArgs(), "--participant", "alice", "--bot-index", "2"], {
      BOT_FLEET_SIZE: undefined,
    });
    expect(unset.tasks[0].talkingClock).toEqual({ index: 2, fleetSize: 3 });
  });

  it("exits 2 on a zero or malformed --fleet-size (issue 3072)", async () => {
    for (const bad of ["0", "4junk"]) {
      const { exits, errors } = await runCli([
        ...baseArgs(),
        "--participant",
        "alice",
        "--fleet-size",
        bad,
      ]);
      expect(exits[0]).toBe(2);
      expect(errors.join("\n")).toContain("--fleet-size (or BOT_FLEET_SIZE) must be a positive");
    }
  });

  it("hands every bot the speech plan from BOT_SPEECH_*, weighted when unset (issue 3090)", async () => {
    const unset = Object.fromEntries(SPEECH_ENV_NAMES.map((n) => [n, undefined]));
    const dflt = await runCli([...baseArgs(), "--participant", "alice"], unset);
    expect(dflt.tasks[0].speechPlan).toEqual(SPEECH_PLAN_DEFAULT);
    const rotation = await runCli([...baseArgs(), "--participant", "alice"], {
      ...unset,
      BOT_SPEECH_MODEL: "rotation",
    });
    expect(rotation.tasks[0].speechPlan).toEqual({ model: "rotation" });
    const bad = await runCli([...baseArgs(), "--participant", "alice"], {
      ...unset,
      BOT_SPEECH_KEEP: "1",
    });
    expect(bad.exits[0]).toBe(2);
    expect(bad.errors.join("\n")).toContain("BOT_SPEECH_KEEP must be a decimal");
  });

  it("exits 2 on a malformed --bot-index instead of silently using 0", async () => {
    const { exits, errors } = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--bot-index",
      "3junk",
    ]);
    // The stub returns where the real `exit(2)` terminates, so the aborted
    // action runs on and the CLI's own `.catch` appends a 1.
    expect(exits).toEqual([2, 1]);
    expect(errors.join("\n")).toContain(
      "--bot-index (or BOT_INDEX) must be a non-negative integer",
    );
  });

  it("hands the orchestrator the encoder-fps sink the capture receipt rides on (#2236)", async () => {
    const { runOpts } = await runCli([...baseArgs(), "--participant", "alice"]);
    expect(typeof runOpts.onEncoderFps).toBe("function");
  });
});

describe("bots-app run — form-login timeout budgets (#2356)", () => {
  it("threads both flags onto every bot task", async () => {
    const { tasks, exits } = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--login-timeout",
      "90000",
      "--login-action-timeout",
      "45000",
    ]);
    expect(exits).toEqual([0]);
    expect(tasks[0].formLoginTimeoutMs).toBe(90_000);
    expect(tasks[0].formLoginActionTimeoutMs).toBe(45_000);
  });

  it("reads the env when the flags are absent, and lets a flag win over it", async () => {
    const fromEnv = await runCli([...baseArgs(), "--participant", "alice"], {
      BOT_LOGIN_TIMEOUT_MS: "75000",
      BOT_LOGIN_ACTION_TIMEOUT_MS: "35000",
    });
    expect(fromEnv.tasks[0].formLoginTimeoutMs).toBe(75_000);
    expect(fromEnv.tasks[0].formLoginActionTimeoutMs).toBe(35_000);

    const flagWins = await runCli(
      [...baseArgs(), "--participant", "alice", "--login-timeout", "90000"],
      { BOT_LOGIN_TIMEOUT_MS: "75000" },
    );
    expect(flagWins.tasks[0].formLoginTimeoutMs).toBe(90_000);
  });

  it("leaves both null when neither flag nor env is set (defaults apply in form-login)", async () => {
    const { tasks } = await runCli([...baseArgs(), "--participant", "alice"], {
      BOT_LOGIN_TIMEOUT_MS: undefined,
      BOT_LOGIN_ACTION_TIMEOUT_MS: undefined,
    });
    expect(tasks[0].formLoginTimeoutMs).toBeNull();
    expect(tasks[0].formLoginActionTimeoutMs).toBeNull();
  });

  it("exits 2 on a malformed budget instead of silently using the default", async () => {
    const { exits, errors } = await runCli([
      ...baseArgs(),
      "--participant",
      "alice",
      "--login-timeout",
      "0",
    ]);
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain("--login-timeout must be a positive integer in ms");
  });
});

describe("bots-app conduct — readiness seam wiring (#2356)", () => {
  function scenarioFile(): string {
    const path = join(workdir(), "scenario.yaml");
    writeFileSync(path, "timeline:\n  - { at: 0s, bot: 0, action: netem-clear }\n", "utf8");
    return path;
  }

  it("wires fetchHealthz into deps for live runs", async () => {
    const { probed, exits, errors } = await runConduct([
      "--scenario",
      scenarioFile(),
      "--readiness-timeout",
      "1",
    ]);
    expect(probed.length).toBeGreaterThan(0);
    expect(probed[0]).toBe(
      "http://videocall-bots-0.videocall-bots.bot-load.svc.cluster.local:8080/healthz",
    );
    expect(errors.join("\n")).toContain("never answered /healthz");
    expect(exits[0]).toBe(1);
  }, 15_000);

  it("reads the /healthz body: a 200 with no bot in the meeting is not ready (#2917)", async () => {
    const idle = { ok: true, bots: 0, inMeeting: 0, pending: 0, expected: 1 };
    const { exits, errors } = await runConduct(
      ["--scenario", scenarioFile(), "--readiness-timeout", "1"],
      () => Response.json(idle),
    );
    expect(errors.join("\n")).toContain("not in the meeting (inMeeting 0/1, still joining 0)");
    expect(exits[0]).toBe(1);
  }, 15_000);

  it("--fleet-size probes pods the timeline never names", async () => {
    const { probed } = await runConduct([
      "--scenario",
      scenarioFile(),
      "--readiness-timeout",
      "1",
      "--fleet-size",
      "2",
    ]);
    expect(probed).toContain(
      "http://videocall-bots-1.videocall-bots.bot-load.svc.cluster.local:8080/healthz",
    );
  }, 15_000);

  it("exits 2 on a malformed --fleet-size", async () => {
    const { exits, errors } = await runConduct([
      "--scenario",
      scenarioFile(),
      "--fleet-size",
      "2x",
    ]);
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain("--fleet-size must be a non-negative integer");
  }, 15_000);
});

describe("bots-app run — log-line forgery (#2375)", () => {
  /** A complete, correctly-prefixed CLI line an operator value must not be able to start. */
  const FORGED = "bots-app: FORGED-BY-CLI";

  const CASES: Array<[string, string[], Record<string, string | undefined>]> = [
    ["BOT_HW_CONCURRENCY (LF)", [], { BOT_HW_CONCURRENCY: `10\n${FORGED}` }],
    ["BOT_HW_CONCURRENCY (CR)", [], { BOT_HW_CONCURRENCY: `10\r${FORGED}` }],
    ["BOT_MAX_RECEIVED_LAYER (LF)", [], { BOT_MAX_RECEIVED_LAYER: `2\n${FORGED}` }],
    ["BOT_SKIP_CANVAS_PAINT (LF)", [], { BOT_SKIP_CANVAS_PAINT: `true\n${FORGED}` }],
    ["BOT_INDEX (LF)", [], { BOT_INDEX: `0\n${FORGED}` }],
    ["BOT_VIDEO_NOISE (LF)", [], { BOT_VIDEO_NOISE: `0.2\n${FORGED}` }],
    ["BOT_SPEECH_MODEL (LF)", [], { BOT_SPEECH_MODEL: `weighted\n${FORGED}` }],
    ["BOT_SPEECH_KEEP (CR)", [], { BOT_SPEECH_KEEP: `0.7\r${FORGED}` }],
    ["BOT_SPEECH_DOMINANT (LF)", [], { BOT_SPEECH_DOMINANT: `0.3\n${FORGED}` }],
    ["BOT_SPEECH_SEED (LF)", [], { BOT_SPEECH_SEED: `1\n${FORGED}` }],
    ["--ttl (LF)", ["--ttl", `5m\n${FORGED}`], {}],
    ["--auth (CR)", ["--auth", `jwt\r${FORGED}`], {}],
  ];

  it.each(CASES)("keeps %s from starting a second bots-app line", async (_label, args, env) => {
    const { errors } = await runCli([...baseArgs(), ...args], env);
    const lines = errors.join("\n").split(/[\r\n]/);
    expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
    // Truncated instead ⇒ the sanitiser dropped the tail rather than collapsing it.
    expect(lines.filter((l) => l.includes(FORGED)).length).toBeGreaterThan(0);
  });

  // Node joins console.error arguments with a space, so a raw 2nd arg forges a line.
  it.each([
    ["LF", "\n"],
    ["CR", "\r"],
  ])("keeps a %s in --config out of the config-read diagnostic", async (_name, ctrl) => {
    const { errors } = await runCli([
      ...baseArgs(),
      "--config",
      join(workdir(), `absent${ctrl}${FORGED}.yaml`),
    ]);
    const lines = errors.join("\n").split(/[\r\n]/);
    expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
    expect(lines.filter((l) => l.includes(FORGED)).length).toBeGreaterThan(0);
  });
});

/**
 * Drive any subcommand, recording writers the way Node formats them: strings
 * verbatim, everything else through `inspect`, joined with a space. Writes after
 * the first `process.exit` are dropped — only the stub lets the action run on.
 */
async function runSubcommand(
  argv: string[],
  env: Record<string, string | undefined> = {},
  until?: (state: { output: string[]; exits: number[] }) => boolean,
): Promise<{ output: string[]; exits: number[] }> {
  const output: string[] = [];
  const exits: number[] = [];
  const record = (...a: unknown[]): void => {
    if (exits.length === 0) {
      output.push(a.map((x) => (typeof x === "string" ? x : inspect(x))).join(" "));
    }
  };
  const spies = [
    vi.spyOn(process, "exit").mockImplementation(((code?: number) => {
      exits.push(code ?? 0);
      return undefined as never;
    }) as never),
    vi.spyOn(console, "error").mockImplementation(record),
    vi.spyOn(console, "log").mockImplementation(record),
    vi.spyOn(console, "warn").mockImplementation(record),
  ];
  const savedArgv = process.argv;
  const savedEnv = new Map(Object.keys(env).map((k) => [k, process.env[k]]));
  process.argv = ["node", "cli.ts", ...argv];
  for (const [k, v] of Object.entries(env)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  try {
    vi.resetModules();
    await import("./cli");
    await vi.waitFor(() => expect(until ? until({ output, exits }) : exits.length > 0).toBe(true));
  } finally {
    process.argv = savedArgv;
    for (const [k, v] of savedEnv) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    for (const spy of spies) spy.mockRestore();
  }
  return { output, exits };
}

describe("bots-app non-run subcommands — log-line forgery (#2406)", () => {
  const FORGED = "bots-app: FORGED-BY-CLI";

  function assertCollapsed(output: string[]): void {
    const lines = output.join("\n").split(/[\r\n]/);
    expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
    // Positive control: absent entirely ⇒ the probe never reached the writer.
    expect(lines.filter((l) => l.includes(FORGED)).length).toBeGreaterThan(0);
  }

  it.each([
    ["gen --count (LF)", ["gen", "--count", `abc\n${FORGED}`]],
    ["gen --seed (LF)", ["gen", "--count", "1", "--seed", `zz\n${FORGED}`]],
  ])("keeps %s from starting a second bots-app line", async (_label, argv) => {
    const { output } = await runSubcommand([
      ...argv,
      "--meeting-url",
      "http://127.0.0.1:3001/meeting/Forge",
      "--manifest",
      join(workdir(), "absent.yaml"),
    ]);
    assertCollapsed(output);
  });

  it("keeps a LF in report --logs out of the error line and exits 2", async () => {
    const { output } = await runSubcommand(
      ["report", "--logs", join(workdir(), `absent\n${FORGED}`), "--fleet-size", "8"],
      {},
      (s) => s.output.length > 0,
    );
    assertCollapsed(output);
    expect(process.exitCode).toBe(2);
    process.exitCode = undefined;
  });

  it("exits 2, a usage error, when report has no --fleet-size", async () => {
    const stderr: string[] = [];
    const spy = vi.spyOn(process.stderr, "write").mockImplementation((chunk) => {
      stderr.push(String(chunk));
      return true;
    });
    try {
      const { exits } = await runSubcommand(["report", "--logs", workdir()]);
      expect(exits[0]).toBe(2);
    } finally {
      spy.mockRestore();
      process.exitCode = undefined;
    }
    expect(stderr.join("")).toContain("required option '--fleet-size <n>' not specified");
  });

  it("keeps a LF in prep-assets --manifest out of the not-found line", async () => {
    const { output } = await runSubcommand([
      "prep-assets",
      "--manifest",
      join(workdir(), `absent\n${FORGED}.yaml`),
    ]);
    assertCollapsed(output);
  });

  /**
   * `participant` is the only operator value that reaches the `[label]` writers,
   * and it arrives from both `--participants` and the manifest.
   */
  function forgedParticipantManifest(dir: string): string {
    const path = join(dir, "manifest.yaml");
    writeFileSync(
      path,
      [
        "pause_ms: 0",
        "participants:",
        `  - name: "alice\\n${FORGED}"`,
        "lines:",
        `  - speaker: "alice\\n${FORGED}"`,
        "    audio_file: line.wav",
        "",
      ].join("\n"),
      "utf8",
    );
    writeFileSync(join(dir, "line.wav"), "RIFF", "utf8");
    return path;
  }

  it("keeps a LF in a prep-assets participant out of the cached-audio line", async () => {
    const dir = workdir();
    const manifestPath = forgedParticipantManifest(dir);
    // Pre-seed the stitched output so the freshness check short-circuits ffmpeg.
    const audioDir = join(dir, "out", "audio");
    mkdirSync(audioDir, { recursive: true });
    writeFileSync(join(audioDir, `alice\n${FORGED}.wav`), "RIFF", "utf8");
    const { output } = await runSubcommand(
      [
        "prep-assets",
        "--manifest",
        manifestPath,
        "--output-dir",
        join(dir, "out"),
        "--costume-source",
        join(dir, "absent-costumes"),
        "--participants",
        `alice\n${FORGED}`,
      ],
      {},
      ({ output: o }) => o.some((l) => l.includes("audio cached")),
    );
    assertCollapsed(output);
  });

  it("keeps a LF in a prep-assets participant out of the prep-failed line", async () => {
    const dir = workdir();
    const manifestPath = forgedParticipantManifest(dir);
    const { output } = await runSubcommand(
      [
        "prep-assets",
        "--manifest",
        manifestPath,
        // Unwritable output dir ⇒ the audio prep throws inside the per-participant try.
        "--output-dir",
        "/dev/null/nope",
        "--participants",
        `alice\n${FORGED}`,
      ],
      {},
      ({ output: o }) => o.some((l) => l.includes("prep failed")),
    );
    assertCollapsed(output);
  });

  it("keeps a LF in prep-assets --costume-source out of the skip warning", async () => {
    const dir = workdir();
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, "pause_ms: 0\nparticipants:\n  - name: alice\nlines: []\n", "utf8");
    const { output } = await runSubcommand(
      [
        "prep-assets",
        "--manifest",
        manifestPath,
        "--output-dir",
        join(dir, "out"),
        "--costume-source",
        join(dir, `absent\n${FORGED}`),
      ],
      {},
      ({ output: o }) => o.some((l) => l.includes("skipping y4m")),
    );
    assertCollapsed(output);
  });

  it.each([
    ["login", ["login", "alice"]],
    ["sso-login", ["sso-login"]],
  ])("keeps a LF in %s --assets-dir out of the captured-file warning", async (_label, argv) => {
    // Writable but forged: the three pre-launch lines emit before the browser stub rejects.
    const assetsDir = join(workdir(), `forge\n${FORGED}`);
    mkdirSync(assetsDir, { recursive: true });
    const { output } = await runSubcommand([...argv, "--assets-dir", assetsDir]);
    assertCollapsed(output);
  });

  it("keeps a LF in dashboard --ctl-token-file out of the resolve error", async () => {
    const { output } = await runSubcommand([
      "dashboard",
      "--port",
      "0",
      "--ctl-token-file",
      join(workdir(), `absent\n${FORGED}.token`),
      "--no-open",
      "--manifest",
      "",
    ]);
    assertCollapsed(output);
  });

  it.each([
    ["built", true],
    ["fallback", false],
  ])("keeps a LF in dashboard --dist-dir out of the %s UI line", async (_label, built) => {
    const distDir = join(workdir(), `dist\n${FORGED}`);
    if (built) {
      mkdirSync(distDir, { recursive: true });
      writeFileSync(join(distDir, "index.html"), "<!doctype html>", "utf8");
    }
    const { output } = await runSubcommand(
      [
        "dashboard",
        "--port",
        "0",
        "--ctl-port",
        "5998",
        "--ctl-token",
        "forge-test-token",
        "--no-open",
        "--dist-dir",
        distDir,
        "--manifest",
        "",
      ],
      {},
      ({ output: o }) => o.some((l) => l.includes("UI")),
    );
    assertCollapsed(output);
  });

  it("keeps a LF in login --start-url out of the opening line", async () => {
    const { output } = await runSubcommand([
      "login",
      "alice",
      "--assets-dir",
      workdir(),
      "--start-url",
      `https://example.invalid/\n${FORGED}`,
    ]);
    assertCollapsed(output);
  });

  it("keeps a LF in sso-login --start-url out of the opening line", async () => {
    const { output } = await runSubcommand([
      "sso-login",
      "--assets-dir",
      workdir(),
      "--start-url",
      `https://example.invalid/\n${FORGED}`,
    ]);
    assertCollapsed(output);
  });

  it.each([
    ["--port", ["dashboard", "--port", `abc\n${FORGED}`]],
    ["--ctl-port", ["dashboard", "--port", "0", "--ctl-port", `abc\n${FORGED}`]],
  ])("keeps a LF in dashboard %s out of the validation error", async (_label, argv) => {
    const { output } = await runSubcommand([...argv, "--no-open", "--manifest", ""]);
    assertCollapsed(output);
  });

  it("keeps a LF in an unhandled rejection's payload out of the fatal marker line", async () => {
    const { output, exits } = await runSubcommand([
      "login",
      "alice",
      "--assets-dir",
      `/dev/null/forge\n${FORGED}`,
    ]);
    expect(exits).toEqual([1]);
    assertCollapsed(output);
    expect(output.join("\n").split(/[\r\n]/)).toContain("bots-app: fatal");
  });

  it("keeps a LF in BOT_CTL_PROXY_IDLE_TIMEOUT_MS out of the dashboard warning", async () => {
    const distDir = workdir();
    writeFileSync(join(distDir, "index.html"), "<!doctype html>", "utf8");
    const { output } = await runSubcommand(
      [
        "dashboard",
        "--port",
        "0",
        "--ctl-port",
        "5999",
        "--ctl-token",
        "forge-test-token",
        "--no-open",
        "--dist-dir",
        distDir,
        "--manifest",
        "",
      ],
      { BOT_CTL_PROXY_IDLE_TIMEOUT_MS: `abc\n${FORGED}` },
      ({ output: o }) => o.some((l) => l.includes("BOT_CTL_PROXY_IDLE_TIMEOUT_MS")),
    );
    assertCollapsed(output);
  });
});

describe("bots-app run — --diag-packets (#2970)", () => {
  const ENV = { BOT_DIAG_PACKETS: undefined, BOTS_IMAGE_REVISION: undefined };

  it("carries the flag, else the env, into the task; unset leaves it null", async () => {
    const flag = await runCli([...baseArgs(), "--participant", "a", "--diag-packets", "off"], ENV);
    expect(flag.tasks[0].diagPackets).toBe("off");
    const env = await runCli([...baseArgs(), "--participant", "a"], {
      ...ENV,
      BOT_DIAG_PACKETS: "0",
    });
    expect(env.tasks[0].diagPackets).toBe("off");
    const unset = await runCli([...baseArgs(), "--participant", "a"], ENV);
    expect(unset.tasks[0].diagPackets).toBeNull();
    const on = await runCli([...baseArgs(), "--participant", "a", "--diag-packets", "on"], {
      ...ENV,
      BOT_DIAG_PACKETS: "off",
    });
    expect(on.tasks[0].diagPackets).toBe("on");
  });

  it("exits 2 on an unknown value", async () => {
    const { exits, errors } = await runCli(
      [...baseArgs(), "--participant", "a", "--diag-packets", "maybe"],
      ENV,
    );
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain(
      '--diag-packets (or BOT_DIAG_PACKETS) must be "on", "off" or "default"',
    );
  });

  it("routes each bot's observation into its participant record", async () => {
    const assetsDir = workdir();
    const { tasks, runOpts } = await runCli(
      [
        "--meeting-url",
        "http://127.0.0.1:3001/meeting/GeomTest",
        "--assets-dir",
        assetsDir,
        "--participant",
        "a",
        "--diag-packets",
        "off",
      ],
      ENV,
    );
    const hooks = runOpts as {
      onRegister: (t: BotTask) => void;
      onDiagPackets: (id: string, obs: { state: string; source: string }) => void;
    };
    hooks.onRegister(tasks[0]);
    hooks.onDiagPackets(tasks[0].botId, { state: "DISABLED", source: "config" });
    const rec = JSON.parse(
      readFileSync(join(assetsDir, "participants", `${tasks[0].botId}.json`), "utf8"),
    );
    expect(rec.diagnostics_packets).toEqual({
      requested: "off",
      observed: "DISABLED",
      source: "config",
    });
  });
});

describe("bots-app run — --decode-budget (#2914)", () => {
  const ENV = { BOTS_IMAGE_REVISION: undefined };

  it("defaults every task to client and carries off when asked", async () => {
    const unset = await runCli([...baseArgs(), "--participant", "a"], ENV);
    expect(unset.tasks.map((t) => t.decodeBudget)).toEqual(["client"]);
    expect(unset.logs.join("\n")).not.toContain(DECODE_BUDGET_WAIVER_NOTE);
    const off = await runCli([...baseArgs(), "--participant", "a", "--decode-budget", "off"], ENV);
    expect(off.exits).not.toContain(2);
    expect(off.tasks.map((t) => t.decodeBudget)).toEqual(["off"]);
    expect(off.logs.join("\n")).toContain(DECODE_BUDGET_WAIVER_NOTE);
  });

  it("exits 2 on a value that is not client or off", async () => {
    const { exits, errors } = await runCli(
      [...baseArgs(), "--participant", "a", "--decode-budget", "all"],
      ENV,
    );
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain('--decode-budget must be "client" or "off", got "all"');
  });

  it("routes the join readback into the participant record", async () => {
    const assetsDir = workdir();
    const { tasks, runOpts } = await runCli(
      [
        "--meeting-url",
        "http://127.0.0.1:3001/meeting/GeomTest",
        "--assets-dir",
        assetsDir,
        "--participant",
        "a",
        "--decode-budget",
        "off",
      ],
      ENV,
    );
    const hooks = runOpts as {
      onRegister: (t: BotTask) => void;
      onJoin: (id: string, at: number, media: unknown, uid: null, readback: string) => void;
    };
    hooks.onRegister(tasks[0]);
    hooks.onJoin(tasks[0].botId, 1_000, { camera: true, mic: true }, null, "all");
    const rec = JSON.parse(
      readFileSync(join(assetsDir, "participants", `${tasks[0].botId}.json`), "utf8"),
    );
    expect(rec.decode_budget).toEqual({ requested: "off", observed: "all" });
    expect(rec.unverified.map((u: { field: string }) => u.field)).toContain("decode_budget");
  });
});

describe("bots-app run — --simulcast-max-layers (#3246)", () => {
  const ENV = { BOT_SIMULCAST_MAX_LAYERS: undefined, BOTS_IMAGE_REVISION: undefined };

  it("carries the flag, else the env, into the task; unset leaves it null", async () => {
    const flag = await runCli(
      [...baseArgs(), "--participant", "a", "--simulcast-max-layers", "3"],
      { ...ENV, BOT_SIMULCAST_MAX_LAYERS: "1" },
    );
    expect(flag.exits).not.toContain(2);
    expect(flag.tasks.map((t) => t.simulcastMaxLayers)).toEqual([3]);
    const env = await runCli([...baseArgs(), "--participant", "a"], {
      ...ENV,
      BOT_SIMULCAST_MAX_LAYERS: "2",
    });
    expect(env.tasks.map((t) => t.simulcastMaxLayers)).toEqual([2]);
    const unset = await runCli([...baseArgs(), "--participant", "a"], ENV);
    expect(unset.tasks.map((t) => t.simulcastMaxLayers)).toEqual([null]);
  });

  it.each(["0", "4", "x"])("exits 2 on %j", async (raw) => {
    const { exits, errors } = await runCli(
      [...baseArgs(), "--participant", "a", "--simulcast-max-layers", raw],
      ENV,
    );
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain(
      `--simulcast-max-layers (or BOT_SIMULCAST_MAX_LAYERS) must be an integer from 1 to 3, got "${raw}"`,
    );
  });

  it("routes the join readback into the participant record", async () => {
    const assetsDir = workdir();
    const { tasks, runOpts } = await runCli(
      [
        "--meeting-url",
        "http://127.0.0.1:3001/meeting/GeomTest",
        "--assets-dir",
        assetsDir,
        "--participant",
        "a",
        "--simulcast-max-layers",
        "3",
      ],
      ENV,
    );
    const hooks = runOpts as {
      onRegister: (t: BotTask) => void;
      onJoin: (...args: unknown[]) => void;
    };
    hooks.onRegister(tasks[0]);
    hooks.onJoin(tasks[0].botId, 1_000, { camera: true, mic: true }, null, null, null, null, 3);
    const rec = JSON.parse(
      readFileSync(join(assetsDir, "participants", `${tasks[0].botId}.json`), "utf8"),
    );
    expect(rec.simulcast_max_layers).toEqual({ requested: 3, observed: 3 });
  });
});

describe("bots-app run — --video-noise (issue 3089)", () => {
  const ENV = { BOT_VIDEO_NOISE: undefined, BOT_INDEX: undefined, BOTS_IMAGE_REVISION: undefined };

  it("leaves the alpha to the geometry when unset, reads the env, and lets the flag win", async () => {
    const unset = await runCli([...baseArgs(), "--participant", "a"], ENV);
    expect(unset.tasks.map((t) => t.videoNoise)).toEqual([null]);
    const fromEnv = await runCli([...baseArgs(), "--participant", "a"], {
      ...ENV,
      BOT_VIDEO_NOISE: "0.2",
    });
    expect(fromEnv.tasks.map((t) => t.videoNoise)).toEqual([0.2]);
    const flagWins = await runCli([...baseArgs(), "--participant", "a", "--video-noise", "0"], {
      ...ENV,
      BOT_VIDEO_NOISE: "0.2",
    });
    expect(flagWins.tasks.map((t) => t.videoNoise)).toEqual([0]);
  });

  it("exits 2 on an alpha outside 0..1", async () => {
    const { exits, errors } = await runCli(
      [...baseArgs(), "--participant", "a", "--video-noise", "1.5"],
      ENV,
    );
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain(
      '--video-noise (or BOT_VIDEO_NOISE) must be a number from 0 to 1, got "1.5"',
    );
  });

  it("records the HD default for an HD clock bot, and flags an override", async () => {
    const record = async (extra: string[], env: Record<string, string | undefined>) => {
      const assetsDir = workdir();
      const { tasks, runOpts } = await runCli(
        [
          "--meeting-url",
          "http://127.0.0.1:3001/meeting/GeomTest",
          "--assets-dir",
          assetsDir,
          "--participant",
          "a",
          "--video-mode",
          "clock",
          "--bot-index",
          "1",
          ...extra,
        ],
        { ...ENV, ...env },
      );
      (runOpts as { onRegister: (t: BotTask) => void }).onRegister(tasks[0]);
      return JSON.parse(
        readFileSync(join(assetsDir, "participants", `${tasks[0].botId}.json`), "utf8"),
      );
    };
    const byDefault = await record([], {});
    expect(byDefault.video_noise).toEqual({ requested: null, alpha: 0.15 });
    expect(byDefault.unverified.map((u: { field: string }) => u.field)).not.toContain(
      "video_noise",
    );
    const overridden = await record([], { BOT_VIDEO_NOISE: "0" });
    expect(overridden.video_noise).toEqual({ requested: 0, alpha: 0 });
    expect(overridden.unverified).toContainEqual({
      field: "video_noise",
      reason:
        "override 0 replaced the 0.15 default for 1280x720, so this bot's video upload is not the modelled camera's",
    });
  });
});

describe("bots-app run — BOT_SKIP_CANVAS_PAINT (issue 3086)", () => {
  it("carries the fleet env into each task and routes the join readback into the record", async () => {
    const assetsDir = workdir();
    const { tasks, runOpts } = await runCli(
      [
        "--meeting-url",
        "http://127.0.0.1:3001/meeting/GeomTest",
        "--assets-dir",
        assetsDir,
        "--participant",
        "a",
      ],
      { BOTS_IMAGE_REVISION: undefined, [SKIP_CANVAS_PAINT_ENV]: "true" },
    );
    expect(tasks.map((t) => t.skipCanvasPaint)).toEqual([true]);
    const hooks = runOpts as {
      onRegister: (t: BotTask) => void;
      onJoin: (...a: [string, number, unknown, null, null, string]) => void;
    };
    hooks.onRegister(tasks[0]);
    hooks.onJoin(tasks[0].botId, 1_000, { camera: true, mic: true }, null, null, "true");
    const rec = JSON.parse(
      readFileSync(join(assetsDir, "participants", `${tasks[0].botId}.json`), "utf8"),
    );
    expect(rec.skip_canvas_paint).toEqual({ requested: true, observed: "true" });
  });
});

describe("bots-app run — --transport / BOT_TRANSPORT_WS_EVERY (issue 3087)", () => {
  const ENV = {
    BOT_TRANSPORT: undefined,
    BOT_TRANSPORT_WS_EVERY: undefined,
    BOT_FLEET_SIZE: undefined,
    BOTS_IMAGE_REVISION: undefined,
  };
  const at = (index: number, extra: string[] = []): string[] => [
    ...baseArgs(),
    "--participant",
    "a",
    "--bot-index",
    String(index),
    ...extra,
  ];

  it("leaves an unconfigured bot on auto", async () => {
    const { tasks } = await runCli(at(3), ENV);
    expect(tasks[0].transport).toBe("auto");
  });

  it("puts the stride's indices on websocket and the rest on webtransport", async () => {
    const env = { ...ENV, BOT_TRANSPORT_WS_EVERY: "4", BOT_FLEET_SIZE: "8" };
    const ws = await runCli(at(7), env);
    expect(ws.tasks[0].transport).toBe("websocket");
    expect(ws.logs.join("\n")).toContain("websocket on index 3,7");
    const wt = await runCli(at(2), env);
    expect(wt.tasks[0].transport).toBe("webtransport");
  });

  it("warns about the HD share of the websocket group only for a stride sharing a factor with 6", async () => {
    const env = { ...ENV, BOT_FLEET_SIZE: "8" };
    const k4 = await runCli(at(3), { ...env, BOT_TRANSPORT_WS_EVERY: "4" });
    expect(k4.errors.join("\n")).toContain(
      "WARNING: BOT_TRANSPORT_WS_EVERY=4 puts 1280x720 bots at 1 of 2 websocket bots against 2 of 8",
    );
    const k5 = await runCli(at(2), { ...env, BOT_TRANSPORT_WS_EVERY: "5" });
    expect(k5.errors.join("\n")).not.toContain("1280x720");
    const empty = await runCli(at(0), { ...ENV, BOT_FLEET_SIZE: "3", BOT_TRANSPORT_WS_EVERY: "4" });
    expect(empty.errors.join("\n")).toContain("websocket on index none");
    expect(empty.errors.join("\n")).not.toContain("1280x720");
  });

  it("warns when the stride selects no bot in the fleet", async () => {
    const { errors } = await runCli(at(0), {
      ...ENV,
      BOT_TRANSPORT_WS_EVERY: "4",
      BOT_FLEET_SIZE: "3",
    });
    expect(errors.join("\n")).toContain("websocket on index none");
  });

  it("lets the flag beat BOT_TRANSPORT, and either beat the stride", async () => {
    const flag = await runCli(at(3, ["--transport", "wt"]), {
      ...ENV,
      BOT_TRANSPORT: "ws",
      BOT_TRANSPORT_WS_EVERY: "4",
    });
    expect(flag.tasks[0].transport).toBe("webtransport");
    expect(flag.logs.join("\n")).toContain("BOT_TRANSPORT_WS_EVERY=4 ignored");
    const env = await runCli(at(2), { ...ENV, BOT_TRANSPORT: "ws", BOT_TRANSPORT_WS_EVERY: "4" });
    expect(env.tasks[0].transport).toBe("websocket");
  });

  it("lets an explicit auto, from the flag or the env, beat the stride", async () => {
    const stride = { ...ENV, BOT_TRANSPORT_WS_EVERY: "4", BOT_FLEET_SIZE: "8" };
    expect((await runCli(at(7), stride)).tasks[0].transport).toBe("websocket");
    const flag = await runCli(at(7, ["--transport", "auto"]), stride);
    expect(flag.tasks[0].transport).toBe("auto");
    const env = await runCli(at(7), { ...stride, BOT_TRANSPORT: "auto" });
    expect(env.tasks[0].transport).toBe("auto");
  });

  it("gives each bot of a multi-bot run its own index's transport", async () => {
    const dir = workdir();
    const configPath = join(dir, "meeting.yaml");
    writeFileSync(
      configPath,
      [
        "meeting_url: http://127.0.0.1:3001/meeting/T",
        "bots:",
        "  - participant: alice",
        "  - participant: bob",
      ].join("\n"),
      "utf8",
    );
    const { tasks } = await runCli(
      ["--assets-dir", dir, "--config", configPath, "--bot-index", "6"],
      { ...ENV, BOT_INDEX: undefined, BOT_TRANSPORT_WS_EVERY: "4" },
    );
    expect(tasks.map((t) => t.transport)).toEqual(["webtransport", "websocket"]);
  });

  it("exits 2 on a malformed value", async () => {
    for (const env of [
      { ...ENV, BOT_TRANSPORT: "quic" },
      { ...ENV, BOT_TRANSPORT_WS_EVERY: "0" },
      { ...ENV, BOT_TRANSPORT_WS_EVERY: "6" },
      { ...ENV, BOT_TRANSPORT_WS_EVERY: "12" },
    ]) {
      const { exits } = await runCli(at(0), env);
      expect(exits[0]).toBe(2);
    }
  });

  it("routes each bot's transport lines into its participant record", async () => {
    const assetsDir = workdir();
    const { tasks, runOpts } = await runCli(
      [
        "--meeting-url",
        "http://127.0.0.1:3001/meeting/GeomTest",
        "--assets-dir",
        assetsDir,
        "--participant",
        "a",
        "--transport",
        "ws",
      ],
      ENV,
    );
    const hooks = runOpts as {
      onRegister: (t: BotTask) => void;
      onTransport: (id: string, e: unknown) => void;
    };
    hooks.onRegister(tasks[0]);
    hooks.onTransport(tasks[0].botId, { kind: "elected", active: "websocket" });
    const rec = JSON.parse(
      readFileSync(join(assetsDir, "participants", `${tasks[0].botId}.json`), "utf8"),
    );
    expect(rec.participant.transport_intended).toBe("websocket");
    expect(rec.transport_observed).toBe("websocket");
  });
});

describe("bots-app run — --personas (issue 3091)", () => {
  const ENV = {
    BOT_PERSONAS: undefined,
    BOT_CAMERA: undefined,
    BOT_PERSONA_MIC_ON: undefined,
    BOT_PERSONA_CAMERA_ON: undefined,
    BOT_JOIN_CAMERA_OFF: undefined,
    BOT_JOIN_MIC_MUTED: undefined,
    BOT_CAMERA_ON_SECS_MIN: undefined,
    BOT_CAMERA_ON_SECS_MAX: undefined,
    BOT_CAMERA_OFF_SECS_MIN: undefined,
    BOT_CAMERA_OFF_SECS_MAX: undefined,
    BOT_FLEET_SIZE: undefined,
    BOT_INDEX: undefined,
    ...Object.fromEntries(SPEECH_ENV_NAMES.map((n) => [n, undefined])),
  };
  const clock = (extra: string[] = [], index = 0): string[] => [
    ...baseArgs(),
    "--participant",
    "a",
    "--bot-index",
    String(index),
    "--fleet-size",
    "3",
    "--video-mode",
    "clock",
    ...extra,
  ];
  // One run per index, as each fleet pod runs; runCli shares mocks, so in sequence.
  const fleet = async (
    extra: string[],
    env: Record<string, string | undefined>,
  ): Promise<BotTask[]> => {
    const tasks: BotTask[] = [];
    for (const i of [0, 1, 2]) tasks.push(...(await runCli(clock(extra, i), env)).tasks);
    return tasks;
  };

  it("leaves bots without a persona when off", async () => {
    const { tasks } = await runCli(clock(), ENV);
    expect(tasks[0].persona ?? null).toBeNull();
    expect(tasks[0].joinMedia).toEqual({ camera: true, mic: true });
  });

  it("gives every bot the shared room's persona and joins it in the persona's steady state", async () => {
    const tasks = await fleet(["--personas"], ENV);
    const { logs, errors } = await runCli(clock(["--personas"]), ENV);
    expect(tasks.map((t) => [t.persona?.mic, t.persona?.camera])).toEqual([
      ["push-to-talk", "always"],
      ["push-to-talk", "never"],
      ["open", "always"],
    ]);
    expect(tasks.map((t) => t.joinMedia)).toEqual([
      { camera: true, mic: false },
      { camera: false, mic: false },
      { camera: true, mic: true },
    ]);
    expect(logs.join("\n")).toContain("personas: room of 3: mic open=1 push-to-talk=2 observer=0");
    expect(errors.join("\n")).toContain(
      "WARNING: expected mic-on share 0.54 is below the target 0.80",
    );
  });

  it("joins a with-voice camera off", async () => {
    const tasks = await fleet(["--personas"], {
      ...ENV,
      BOT_PERSONA_CAMERA_ON: "0.1501,0.48,0.14,0.01,0.08",
    });
    const voiced = tasks.filter((t) => t.persona?.camera === "with-voice");
    expect(voiced.length).toBeGreaterThan(0);
    expect(voiced.map((t) => t.joinMedia?.camera)).toEqual(voiced.map(() => false));
  });

  it("reads BOT_PERSONAS=on and BOT_CAMERA=always-on", async () => {
    const tasks = await fleet([], { ...ENV, BOT_PERSONAS: "on", BOT_CAMERA: "always-on" });
    expect(tasks.map((t) => t.persona?.camera)).toEqual(["always", "always", "always"]);
  });

  it.each([
    [
      "--join-camera-off",
      clock(["--personas", "--join-camera-off"]),
      {},
      "--join-camera-off / --join-mic-muted",
    ],
    [
      "--join-mic-muted",
      clock(["--personas", "--join-mic-muted"]),
      {},
      "--join-camera-off / --join-mic-muted",
    ],
    [
      "the camera duty cycle",
      clock(["--personas"]),
      {
        BOT_CAMERA_ON_SECS_MIN: "5",
        BOT_CAMERA_ON_SECS_MAX: "10",
        BOT_CAMERA_OFF_SECS_MIN: "5",
        BOT_CAMERA_OFF_SECS_MAX: "10",
      },
      "camera duty cycle",
    ],
    ["an index outside the fleet", clock(["--personas"], 3), {}, "inside the fleet size 3"],
    [
      "a non-clock video mode",
      [...baseArgs(), "--participant", "a", "--personas"],
      {},
      "--video-mode clock",
    ],
    [
      "BOT_CAMERA without personas",
      clock(),
      { BOT_CAMERA: "always-on" },
      "has no effect without --personas",
    ],
  ])("exits 2 for personas with %s", async (_what, args, env, message) => {
    const { exits, errors } = await runCli(args, { ...ENV, ...env });
    expect(exits[0]).toBe(2);
    expect(errors.join("\n")).toContain(message);
  });
});
