import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { inspect } from "node:util";

import { afterEach, describe, expect, it, vi } from "vitest";

import { type BotTask } from "./orchestrator";

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

vi.mock("./resource/session", () => ({
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
}> {
  const exits: number[] = [];
  const errors: string[] = [];
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
    vi.spyOn(console, "log").mockImplementation(() => {}),
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
  return { tasks, exits, errors, runOpts };
}

/**
 * Drive the real `conduct` subcommand. `globalThis.fetch` is the ONLY seam the
 * readiness probe can reach, so recording it observes the CLI's own wiring.
 */
async function runConduct(args: string[]): Promise<{
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
      return { ok: false } as Response;
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
