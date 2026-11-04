import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { BotTask } from "./orchestrator";
import { SPEAKING_GLOW_ENV, SPEAKING_GLOW_WAIVER_NOTE } from "./speaking-glow";

const mocks = vi.hoisted(() => ({
  runBotsToCompletion: vi.fn(),
  finalized: 0,
}));

vi.mock("./orchestrator", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./orchestrator")>()),
  runBotsToCompletion: mocks.runBotsToCompletion,
}));

vi.mock("./resource/session", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./resource/session")>()),
  ResourceCaptureSession: class {
    readonly label = "cli-glow-test";
    startLocal(): void {}
    finalize(): Promise<null> {
      mocks.finalized += 1;
      return Promise.resolve(null);
    }
  },
  RemoteResourceManager: class {},
}));

type JoinArgs = [string | null, string | null, string | null];
type Record = {
  speaking_glow: { requested: string; observed: string | null };
  unverified: Array<{ field: string; reason: string }>;
};

const dirs: string[] = [];
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
  delete process.env[SPEAKING_GLOW_ENV];
});

/** Runs `bots-app run` with the orchestrator mocked; joins the one bot with `readbacks` and returns its task and record. */
async function run(
  extraArgs: string[],
  readbacks: JoinArgs,
): Promise<{ task: BotTask | undefined; record: Record | null; exitCodes: unknown[] }> {
  const assetsDir = mkdtempSync(join(tmpdir(), "bots-glow-cli-"));
  dirs.push(assetsDir);
  mocks.finalized = 0;
  let seen: BotTask | undefined;
  mocks.runBotsToCompletion.mockReset();
  mocks.runBotsToCompletion.mockImplementation(
    async (opts: {
      tasks: BotTask[];
      onRegister?: (t: BotTask) => void;
      onJoin?: (id: string, at: number, ...rest: unknown[]) => void;
    }) => {
      seen = opts.tasks[0];
      opts.onRegister?.(seen);
      opts.onJoin?.(seen.botId, 1_000, { camera: false, mic: false }, null, ...readbacks);
    },
  );
  const exit = vi.spyOn(process, "exit").mockImplementation((() => undefined as never) as never);
  const quiet = (["log", "warn", "error"] as const).map((m) =>
    vi.spyOn(console, m).mockImplementation(() => {}),
  );
  const argv = process.argv;
  process.argv = [
    "node",
    "cli.ts",
    "run",
    "--manifest",
    "",
    "--meeting-url",
    "http://127.0.0.1:3001/meeting/GlowTest",
    "--assets-dir",
    assetsDir,
    "--participant",
    "alice",
    ...extraArgs,
  ];
  const exitCodes = () => exit.mock.calls.map((c) => c[0]);
  try {
    vi.resetModules();
    await import("./cli");
    await vi.waitFor(() => expect(mocks.finalized === 1 || exitCodes().includes(2)).toBe(true));
    const record =
      seen === undefined
        ? null
        : (JSON.parse(
            readFileSync(join(assetsDir, "participants", `${seen.botId}.json`), "utf8"),
          ) as Record);
    return { task: seen, record, exitCodes: exitCodes() };
  } finally {
    process.argv = argv;
    exit.mockRestore();
    for (const s of quiet) s.mockRestore();
  }
}

const glowEntries = (r: Record | null): string[] =>
  (r?.unverified ?? []).filter((u) => u.field === "speaking_glow").map((u) => u.reason);

describe("bots-app run --speaking-glow (#2914)", () => {
  it("defaults to the client's glow: the task asks for nothing and the record lists nothing", async () => {
    const { task, record } = await run([], [null, null, null]);
    expect(task?.speakingGlow).toBe("client");
    expect(record?.speaking_glow).toEqual({ requested: "client", observed: null });
    expect(glowEntries(record)).toEqual([]);
  });

  it("carries off to the task and records the join readback as the waiver", async () => {
    const { task, record } = await run(["--speaking-glow", "off"], [null, null, "false"]);
    expect(task?.speakingGlow).toBe("off");
    expect(record?.speaking_glow).toEqual({ requested: "off", observed: "false" });
    expect(glowEntries(record)).toEqual([
      `${SPEAKING_GLOW_WAIVER_NOTE}: vc_appearance_glow_enabled=false read back after join`,
    ]);
  });

  it("reads the env when the flag is absent, and the flag wins over it", async () => {
    process.env[SPEAKING_GLOW_ENV] = "off";
    expect((await run([], [null, null, "false"])).task?.speakingGlow).toBe("off");
    process.env[SPEAKING_GLOW_ENV] = "off";
    expect((await run(["--speaking-glow", "client"], [null, null, null])).task?.speakingGlow).toBe(
      "client",
    );
  });

  it("refuses an unknown value before launching anything", async () => {
    const { exitCodes } = await run(["--speaking-glow", "dim"], [null, null, null]);
    expect(exitCodes).toContain(2);
  });
});
