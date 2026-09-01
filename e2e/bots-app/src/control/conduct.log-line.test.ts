import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { Command } from "commander";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  type Clock,
  type ConductDeps,
  type ConductorClient,
  type HostResolveOptions,
  conductScenario,
  registerConductCommand,
} from "./conduct";

const SOURCE = fileURLToPath(new URL("./conduct.ts", import.meta.url));

/** A complete, correctly-prefixed conduct line an operator value must not be able to start. */
const FORGED = "conduct: FORGED-BY-OPERATOR-VALUE";

const SCENARIO = `
room: bottest
timeline:
  - { at: 0s, bot: 0, action: unmute }
`;

const dirs: string[] = [];

function scenarioFile(text = SCENARIO): string {
  const dir = mkdtempSync(join(tmpdir(), "conduct-logline-"));
  dirs.push(dir);
  const p = join(dir, "scenario.yaml");
  writeFileSync(p, text, "utf8");
  return p;
}

afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

/** Drive the real `conduct` subcommand, capturing every written line. */
async function runConduct(args: string[]): Promise<string[]> {
  const written: string[] = [];
  const record = (...a: unknown[]): void => {
    written.push(a.map(String).join(" "));
  };
  const spies = [
    vi.spyOn(process, "exit").mockImplementation(((code?: number) => {
      throw new ExitSignal(code ?? 0);
    }) as never),
    vi.spyOn(console, "error").mockImplementation(record),
    vi.spyOn(console, "log").mockImplementation(record),
    vi
      .spyOn(globalThis, "fetch")
      .mockImplementation((async () => ({ ok: false }) as Response) as typeof fetch),
  ];
  const savedToken = process.env.BOT_CTL_TOKEN;
  process.env.BOT_CTL_TOKEN = "conduct-logline-token";
  const program = new Command();
  program.exitOverride();
  registerConductCommand(program);
  try {
    await program.parseAsync(["node", "cli.ts", "conduct", ...args]);
  } catch (e) {
    if (!(e instanceof ExitSignal)) throw e;
  } finally {
    if (savedToken === undefined) delete process.env.BOT_CTL_TOKEN;
    else process.env.BOT_CTL_TOKEN = savedToken;
    for (const spy of spies) spy.mockRestore();
  }
  return written;
}

/** `process.exit` is not observable in-process; unwind the action instead. */
class ExitSignal extends Error {
  constructor(readonly code: number) {
    super(`exit ${code}`);
  }
}

/**
 * Both halves are required: no emitted physical line may START with the forged
 * marker, AND the marker must still be present somewhere — otherwise a probe
 * that produced no output at all would pass.
 */
function expectCollapsed(written: string[]): void {
  const lines = written.join("\n").split(/[\r\n]/);
  expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
  expect(lines.filter((l) => l.includes(FORGED)).length).toBeGreaterThan(0);
}

describe("conduct CLI — log-line forgery (#2480)", () => {
  it.each([
    ["--port", ["--port", `abc\n${FORGED}`]],
    ["--readiness-timeout", ["--readiness-timeout", `abc\n${FORGED}`]],
  ])("keeps a non-numeric %s out of a new line", async (_flag, extra) => {
    // Non-numeric on purpose: `Number.parseInt` stops at the newline, so a
    // value like `2\n…` parses to 2 and the rejecting writer never runs.
    expectCollapsed(await runConduct(["--scenario", scenarioFile(), ...extra]));
  });

  it("keeps a CR in --readiness-timeout out of a new line", async () => {
    expectCollapsed(
      await runConduct(["--scenario", scenarioFile(), "--readiness-timeout", `abc\r${FORGED}`]),
    );
  });

  it("keeps an unreadable --scenario path out of a new line", async () => {
    expectCollapsed(await runConduct(["--scenario", join(tmpdir(), `absent\n${FORGED}.yaml`)]));
  });

  it("keeps an unreadable --token-file path out of a new line", async () => {
    expectCollapsed(
      await runConduct([
        "--scenario",
        scenarioFile(),
        "--token-file",
        join(tmpdir(), `absent\n${FORGED}`),
      ]),
    );
  });

  it("keeps a scenario validation message out of a new line", async () => {
    const file = scenarioFile(`
timeline:
  - { at: 0s, bot: 0, action: "bogus\\n${FORGED}" }
`);
    expectCollapsed(await runConduct(["--scenario", file, "--dry-run"]));
  });

  it("keeps a forged --namespace out of the readiness-failure diagnostic", async () => {
    // The unreachable pods are named in the thrown message, which the CLI's
    // catch re-emits — the only writer the readiness gate feeds a host to.
    const written = await runConduct([
      "--scenario",
      scenarioFile(),
      "--namespace",
      `bot-load\n${FORGED}`,
      "--readiness-timeout",
      "1",
    ]);
    expect(written.join("\n")).toContain("never answered /healthz");
    expectCollapsed(written);
  });

  it("keeps a forged --namespace out of the dry-run schedule lines", async () => {
    expectCollapsed(
      await runConduct([
        "--scenario",
        scenarioFile(),
        "--dry-run",
        "--namespace",
        `bot-load\n${FORGED}`,
      ]),
    );
  });
});

describe("conductScenario — log-line forgery (#2480)", () => {
  const hostOpts = (): HostResolveOptions => ({
    service: "videocall-bots",
    namespace: `bot-load\n${FORGED}`,
    dnsSuffix: "svc.cluster.local",
  });

  function deps(over: Partial<ConductDeps> = {}): { deps: ConductDeps; lines: string[] } {
    const lines: string[] = [];
    // Advancing clock: the readiness gate polls until `now >= deadline`, so a
    // frozen clock would spin forever under an instant `sleep`.
    let now = 0;
    const clock: Clock = { now: () => now };
    return {
      lines,
      deps: {
        clientFactory: () =>
          ({
            mute: () => Promise.reject(new Error(`upstream said no\n${FORGED}`)),
          }) as unknown as ConductorClient,
        clock,
        sleep: (ms) => {
          now += Math.max(ms, 1);
          return Promise.resolve();
        },
        log: (line) => lines.push(line),
        ...over,
      },
    };
  }

  it("keeps a forged pod FQDN out of the per-call schedule and failure lines", async () => {
    const { deps: d, lines } = deps();
    const summary = await conductScenario({
      scenarioText: SCENARIO,
      hostOpts: hostOpts(),
      port: 8080,
      dryRun: false,
      token: "t",
      readinessTimeoutMs: 0,
      deps: d,
    });
    expect(summary.failed).toBe(1);
    expectCollapsed(lines);
  });

  it("keeps a forged room name out of the summary line", async () => {
    const { deps: d, lines } = deps();
    await conductScenario({
      scenarioText: `room: "bottest\\n${FORGED}"\ntimeline:\n  - { at: 0s, bot: 0, action: unmute }\n`,
      hostOpts: {
        service: "videocall-bots",
        namespace: "bot-load",
        dnsSuffix: "svc.cluster.local",
      },
      port: 8080,
      dryRun: true,
      deps: d,
    });
    expectCollapsed(lines);
  });
});

describe("conduct.ts marker routing (#2480)", () => {
  /** Source with comments removed, so a doc comment naming the marker is not a hit. */
  function code(): string {
    return readFileSync(SOURCE, "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .split("\n")
      .filter((l) => !l.trimStart().startsWith("//"))
      .join("\n");
  }

  it("builds no `conduct: ` line from a raw literal — every writer goes through conductLine", () => {
    const offending = code()
      .split("\n")
      .map((l, i) => [i + 1, l] as const)
      .filter(([, l]) => /["'`]conduct: /.test(l))
      .map(([n, l]) => `${n}: ${l.trim()}`);
    expect(offending).toEqual([]);
    expect(code()).toContain("conductLine(");
  });
});
