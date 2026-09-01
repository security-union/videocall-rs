import { Command } from "commander";
import { describe, expect, it, vi } from "vitest";

import { type BotSnapshot } from "./registry";
import { ctlRequest } from "./client";
import { buildNetemRequest, printBotsTable, registerCtlCommands } from "./ctl";

vi.mock("./client", () => ({
  ctlRequest: vi.fn(async () => ({
    botId: "bot-aaa",
    camera: true,
    mic: true,
    ttl: "10m",
  })),
}));

describe("buildNetemRequest", () => {
  it("maps --clear to DELETE with no body", () => {
    expect(buildNetemRequest({ clear: true })).toEqual({ method: "DELETE" });
  });

  it("maps --profile to POST { profile }", () => {
    expect(buildNetemRequest({ profile: "satellite" })).toEqual({
      method: "POST",
      body: { profile: "satellite" },
    });
  });

  it("maps raw flags to POST with parsed numeric params", () => {
    expect(
      buildNetemRequest({ delay: "150", jitter: "50", loss: "5", rate: "800", limit: "40" }),
    ).toEqual({
      method: "POST",
      body: { delayMs: 150, jitterMs: 50, lossPct: 5, rateKbit: 800, limitPkts: 40 },
    });
  });

  it("omits absent raw flags", () => {
    expect(buildNetemRequest({ loss: "2" })).toEqual({
      method: "POST",
      body: { lossPct: 2 },
    });
  });

  it.each([
    ["--delay", { delay: "abc" }],
    ["--jitter", { delay: "10", jitter: "soon" }],
    ["--loss", { loss: "5%" }],
    ["--rate", { rate: "" }],
    ["--limit", { limit: "abc" }],
  ])("rejects a non-numeric %s instead of emitting NaN", (flag, opts) => {
    expect(() => buildNetemRequest(opts)).toThrow(`ctl netem: ${flag}: expected a number`);
  });

  it("prefers --clear over any other flag", () => {
    // Defensive: if both are somehow set, clear wins and produces no body.
    expect(buildNetemRequest({ clear: true, profile: "dialup" })).toEqual({ method: "DELETE" });
  });
});

function snapshot(botId: string, participant: string): BotSnapshot {
  return {
    botId,
    participant,
    status: "in-meeting",
    startedAt: 1_700_000_000_000,
    meetingURL: "https://example.test/meeting/room-1",
    network: null,
    videoMode: "clock",
    ttl: "10m",
    ttlRemainingMs: 425_000,
    finishedAt: null,
    joinedAt: 1_700_000_001_000,
    host: { kind: "local" },
  };
}

function renderBotsTable(bots: BotSnapshot[]): string[] {
  const lines: string[] = [];
  const spy = vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  });
  try {
    printBotsTable(bots);
  } finally {
    spy.mockRestore();
  }
  return lines;
}

// `k8s/bot-ctl` locates the bot ID and STATUS from the BOT_ID header row's column
// offsets, skipping the dash-only separator. These pin that layout.
describe("printBotsTable row layout (k8s/bot-ctl parse contract)", () => {
  it("names field 1 of row 1 BOT_ID — the wrapper's anchor", () => {
    const lines = renderBotsTable([snapshot("bot-aaa", "alice")]);
    expect(lines[0].split(/\s+/)[0]).toBe("BOT_ID");
  });

  it("puts a dash-only separator on row 2, which the wrapper skips", () => {
    const lines = renderBotsTable([snapshot("bot-aaa", "alice")]);
    expect(lines[1]).toMatch(/^[-\s]+$/);
    expect(lines[1]).toContain("-");
  });

  it("puts the first bot's ID in field 1 of row 3, not row 2", () => {
    const lines = renderBotsTable([snapshot("bot-aaa", "alice"), snapshot("bot-bbb", "bob")]);
    expect(lines).toHaveLength(4);
    expect(lines[2].split(/\s+/)[0]).toBe("bot-aaa");
    expect(lines[3].split(/\s+/)[0]).toBe("bot-bbb");
    expect(lines[1].split(/\s+/)[0]).not.toBe("bot-aaa");
  });

  it("emits a single non-tabular line when the registry is empty", () => {
    const lines = renderBotsTable([]);
    expect(lines).toEqual(["(no bots in registry)"]);
  });
});

function ctlProgram(): Command {
  const program = new Command();
  program.exitOverride();
  program.configureOutput({ writeOut: () => {}, writeErr: () => {} });
  registerCtlCommands(program, "/tmp/bots-app-run");
  return program;
}

// The wrapper builds ctl argv by hand, so an option it passes that the CLI does not
// define fails only at runtime. These lock the flag surface the wrapper targets.
describe("ctl option surface (k8s/bot-ctl argv contract)", () => {
  it("rejects `video --off` — camera-off is the flagless default", async () => {
    await expect(
      ctlProgram().parseAsync(["ctl", "video", "bot-aaa", "--off"], { from: "user" }),
    ).rejects.toMatchObject({ code: "commander.unknownOption" });
  });

  it("rejects `ttl --ttl` — TTL is set with --set/--extend", async () => {
    await expect(
      ctlProgram().parseAsync(["ctl", "ttl", "bot-aaa", "--ttl", "30s"], { from: "user" }),
    ).rejects.toMatchObject({ code: "commander.unknownOption" });
  });

  it("declares the flags the wrapper does pass", () => {
    const subcommandFlags = (name: string): string[] => {
      const cmd = ctlProgram()
        .commands.find((c) => c.name() === "ctl")
        ?.commands.find((c) => c.name() === name);
      if (cmd === undefined) throw new Error(`ctl ${name} is not registered`);
      return cmd.options.map((o) => o.long ?? o.short ?? "");
    };
    expect(subcommandFlags("video")).toContain("--on");
    expect(subcommandFlags("video")).not.toContain("--off");
    expect(subcommandFlags("mute")).toContain("--off");
    expect(subcommandFlags("ttl")).toContain("--set");
  });

  it("requires a botId on `status` — the wrapper must resolve one first", async () => {
    await expect(
      ctlProgram().parseAsync(["ctl", "status"], { from: "user" }),
    ).rejects.toMatchObject({ code: "commander.missingArgument" });
  });
});

// `k8s/bot-ctl` passes NO flag for `video off` and `mute`, so the flagless
// default carries the meaning. `{camera:true}` is camera-OFF on the wire.
describe("ctl request polarity (k8s/bot-ctl argv contract)", () => {
  const request = vi.mocked(ctlRequest);

  async function bodyOf(argv: string[]): Promise<unknown> {
    request.mockClear();
    const log = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      await ctlProgram().parseAsync([...argv, "--port", "18080", "--token", "t"], {
        from: "user",
      });
    } finally {
      log.mockRestore();
    }
    expect(request).toHaveBeenCalledTimes(1);
    return request.mock.calls[0][3];
  }

  it("turns the camera OFF with no flag", async () => {
    expect(await bodyOf(["ctl", "video", "bot-aaa"])).toEqual({ camera: true });
  });

  it("turns the camera ON with --on", async () => {
    expect(await bodyOf(["ctl", "video", "bot-aaa", "--on"])).toEqual({ camera: false });
  });

  it("mutes with no flag", async () => {
    expect(await bodyOf(["ctl", "mute", "bot-aaa"])).toEqual({ mic: true });
  });

  it("unmutes with --off", async () => {
    expect(await bodyOf(["ctl", "mute", "bot-aaa", "--off"])).toEqual({ mic: false });
  });

  it("posts the duration string --set was given", async () => {
    expect(await bodyOf(["ctl", "ttl", "bot-aaa", "--set", "600s"])).toEqual({ ttl: "600s" });
  });
});

describe("ctl netem flag surface", () => {
  const request = vi.mocked(ctlRequest);

  async function netemBodyOf(argv: string[]): Promise<unknown> {
    request.mockClear();
    request.mockResolvedValueOnce({
      op: "shape",
      label: "custom",
      commands: [["tc"]],
      mirrorRemoved: false,
    } as never);
    const log = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      await ctlProgram().parseAsync([...argv, "--port", "18080", "--token", "t"], { from: "user" });
    } finally {
      log.mockRestore();
    }
    expect(request).toHaveBeenCalledTimes(1);
    return request.mock.calls[0][3];
  }

  it("sends --limit as limitPkts alongside --rate", async () => {
    expect(await netemBodyOf(["ctl", "netem", "--rate", "56", "--limit", "10"])).toEqual({
      rateKbit: 56,
      limitPkts: 10,
    });
  });

  it("omits limitPkts when --limit is absent", async () => {
    expect(await netemBodyOf(["ctl", "netem", "--rate", "56"])).toEqual({ rateKbit: 56 });
  });

  it("sends no request at all when --limit is non-numeric", async () => {
    request.mockClear();
    await expect(
      ctlProgram().parseAsync(
        ["ctl", "netem", "--rate", "56", "--limit", "abc", "--port", "18080", "--token", "t"],
        { from: "user" },
      ),
    ).rejects.toThrow('ctl netem: --limit: expected a number, got "abc"');
    expect(request).not.toHaveBeenCalled();
  });
});

describe("ctl token resolution", () => {
  const request = vi.mocked(ctlRequest);

  async function configOf(argv: string[]): Promise<unknown> {
    request.mockClear();
    const log = vi.spyOn(console, "log").mockImplementation(() => {});
    try {
      await ctlProgram().parseAsync(argv, { from: "user" });
    } finally {
      log.mockRestore();
    }
    return request.mock.calls[0][0];
  }

  it("reads the bearer token from BOT_CTL_TOKEN when only --port is given", async () => {
    vi.stubEnv("BOT_CTL_TOKEN", "fleet-secret");
    try {
      expect(await configOf(["ctl", "video", "bot-aaa", "--port", "18080"])).toMatchObject({
        port: 18080,
        token: "fleet-secret",
      });
    } finally {
      vi.unstubAllEnvs();
    }
  });

  it("prefers an explicit --token over the env var", async () => {
    vi.stubEnv("BOT_CTL_TOKEN", "from-env");
    try {
      expect(
        await configOf(["ctl", "video", "bot-aaa", "--port", "18080", "--token", "from-flag"]),
      ).toMatchObject({ token: "from-flag" });
    } finally {
      vi.unstubAllEnvs();
    }
  });

  it("rejects --port with no token from either source", async () => {
    vi.stubEnv("BOT_CTL_TOKEN", "");
    try {
      await expect(
        ctlProgram().parseAsync(["ctl", "video", "bot-aaa", "--port", "18080"], { from: "user" }),
      ).rejects.toThrow(/must be supplied together/);
    } finally {
      vi.unstubAllEnvs();
    }
  });

  it("does not let an exported token divert a tokenless call off token-file discovery", async () => {
    vi.stubEnv("BOT_CTL_TOKEN", "fleet-secret");
    try {
      await expect(
        ctlProgram().parseAsync(["ctl", "list", "--run-dir", "/tmp/bots-app-no-such-dir"], {
          from: "user",
        }),
      ).rejects.toThrow(/no token file found/);
    } finally {
      vi.unstubAllEnvs();
    }
  });
});
