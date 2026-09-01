import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it, vi } from "vitest";

import { printBotsTable } from "./ctl";
import { type BotSnapshot, type BotStatus } from "./registry";

const SCRIPT = fileURLToPath(new URL("../../k8s/bot-ctl", import.meta.url));

const KUBECTL_STUB = `#!/usr/bin/env bash
echo "KUBECONFIG=\${KUBECONFIG:-<unset>}" >>"$ENV_LOG"
args="$*"
case "$args" in
*"get secret"*)
  printf '%s' "$FAKE_SECRET_B64"
  ;;
*"get pods"*)
  if [ -n "\${FAKE_PODS_FAIL:-}" ]; then
    echo "Unable to connect to the server: i/o timeout" >&2
    exit 1
  fi
  printf '%s' "\${FAKE_PODS:-}"
  ;;
*port-forward*)
  if [ -n "\${FAKE_PF_DIE:-}" ]; then
    echo "Unable to listen on port: bind: address already in use" >&2
    exit 1
  fi
  sleep 30 &
  wait
  ;;
*)
  echo "kubectl stub: unhandled $args" >&2
  exit 1
  ;;
esac
`;

// Records the ctl argv it was handed, and answers `ctl list` from a file the
// test renders with the production printBotsTable.
const NPM_STUB = `#!/usr/bin/env bash
argv=()
seen=0
for a in "$@"; do
  if [ "$a" = "ctl" ]; then seen=1; fi
  if [ "$seen" = "1" ]; then argv+=("$a"); fi
done
printf '%s\\n' "\${argv[*]}" >>"$ARGV_LOG"
echo "NPM_TOKEN_ENV=\${BOT_CTL_TOKEN:-<unset>}" >>"$ENV_LOG"
if [ -n "\${FAKE_CTL_FAIL:-}" ]; then
  echo "ctl: request failed (stub)" >&2
  exit 3
fi
if [ "\${argv[1]:-}" = "list" ]; then
  cat "$FAKE_LIST_FILE"
  exit 0
fi
echo "stub-ok \${argv[*]}"
`;

interface RunResult {
  status: number;
  stdout: string;
  stderr: string;
  /** One entry per `npm run bot -- ctl …` invocation, from `ctl` onward. */
  argv: string[];
  env: string[];
}

/**
 * Run the wrapper with stub kubectl/npm on PATH. `pods` is the fleet the stub
 * reports as Running; `list` is the `ctl list` payload every pod returns.
 */
function runBotCtl(
  args: string[],
  opts: { pods?: string[]; list?: string; fake?: Record<string, string> } = {},
): RunResult {
  const workdir = mkdtempSync(join(tmpdir(), "bot-ctl-"));
  const binDir = join(workdir, "bin");
  mkdirSync(binDir, { recursive: true });
  for (const [name, body] of [
    ["kubectl", KUBECTL_STUB],
    ["npm", NPM_STUB],
  ]) {
    const path = join(binDir, name);
    writeFileSync(path, body);
    chmodSync(path, 0o755);
  }
  const argvLog = join(workdir, "argv.log");
  const envLog = join(workdir, "env.log");
  const listFile = join(workdir, "list.out");
  writeFileSync(argvLog, "");
  writeFileSync(envLog, "");
  writeFileSync(listFile, opts.list ?? renderList([bot("bot-aaa", "alice", "in-meeting")]));

  const pods = opts.pods ?? ["videocall-bots-0"];
  const env: Record<string, string> = {
    PATH: `${binDir}:${process.env.PATH ?? "/usr/bin:/bin"}`,
    HOME: workdir,
    E2E_DIR: workdir,
    BOT_CTL_KUBECONFIG: join(workdir, "kubeconfig"),
    BOT_NAMESPACE: "bot-load",
    BOT_STS: "videocall-bots",
    // Sub-second so the suite does not pay the real 1s forwarder wait per call.
    BOT_CTL_PF_WAIT: "0.05",
    ARGV_LOG: argvLog,
    ENV_LOG: envLog,
    FAKE_LIST_FILE: listFile,
    FAKE_SECRET_B64: Buffer.from("fleet-secret").toString("base64"),
    FAKE_PODS: pods.length === 0 ? "" : `${pods.join("\n")}\n`,
    ...opts.fake,
  };
  try {
    const res = spawnSync("bash", [SCRIPT, ...args], { env, encoding: "utf8" });
    const lines = (path: string): string[] =>
      readFileSync(path, "utf8")
        .split("\n")
        .filter((l) => l !== "");
    return {
      status: res.status ?? 1,
      stdout: res.stdout,
      stderr: res.stderr,
      argv: lines(argvLog),
      env: lines(envLog),
    };
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
}

function bot(botId: string, participant: string, status: BotStatus): BotSnapshot {
  return {
    botId,
    participant,
    status,
    startedAt: 1_700_000_000_000,
    meetingURL: "https://example.test/meeting/room-1",
    network: null,
    videoMode: "clock",
    ttl: "10m",
    ttlRemainingMs: status === "in-meeting" ? 425_000 : null,
    finishedAt: null,
    joinedAt: 1_700_000_001_000,
    host: { kind: "local" },
  };
}

/** The real `ctl list` rendering, so a table change breaks the wrapper's parse here. */
function renderList(bots: BotSnapshot[]): string {
  const lines: string[] = [];
  const spy = vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  });
  try {
    printBotsTable(bots);
  } finally {
    spy.mockRestore();
  }
  return `${lines.join("\n")}\n`;
}

/** The mutating call — every per-bot command lists first to resolve the bot ID. */
function mutation(res: RunResult): string {
  const calls = res.argv.filter((a) => !a.startsWith("ctl list"));
  expect(calls).toHaveLength(1);
  return calls[0];
}

describe("k8s/bot-ctl — ctl argv it builds", () => {
  it("resolves the bot ID from the table body, never the dashed separator", () => {
    const res = runBotCtl(["video", "off"]);
    expect(res.status).toBe(0);
    expect(mutation(res)).toContain("ctl video bot-aaa");
    expect(res.argv.join("\n")).not.toMatch(/---/);
  });

  it("passes no flag for `video off` — camera-off is ctl's flagless default", () => {
    expect(mutation(runBotCtl(["video", "off"]))).not.toContain("--off");
  });

  it("passes --on for `video on`", () => {
    expect(mutation(runBotCtl(["video", "on"]))).toContain("ctl video bot-aaa --on");
  });

  it("passes no flag for `mute`, --off for `unmute`", () => {
    expect(mutation(runBotCtl(["mute"]))).toMatch(/^ctl mute bot-aaa --port/);
    expect(mutation(runBotCtl(["unmute"]))).toContain("ctl mute bot-aaa --off");
  });

  it("sets TTL with --set and a duration string, not --ttl", () => {
    const call = mutation(runBotCtl(["ttl", "600"]));
    expect(call).toContain("ctl ttl bot-aaa --set 600s");
    expect(call).not.toContain("--ttl");
  });

  it("rejects a TTL above the setTimeout ceiling before calling ctl", () => {
    const res = runBotCtl(["ttl", "2147484"]);
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("2147483");
    expect(res.argv).toEqual([]);
  });

  it("rejects a TTL that wraps bash arithmetic rather than exceeding it", () => {
    const res = runBotCtl(["ttl", "9999999999999999999"]);
    expect(res.status).not.toBe(0);
    expect(res.argv).toEqual([]);
  });

  it("accepts a zero-padded TTL", () => {
    expect(mutation(runBotCtl(["ttl", "0000600"]))).toContain("ctl ttl bot-aaa --set 600s");
  });

  it("rejects a video state that is not on|off", () => {
    const res = runBotCtl(["video", "sideways"]);
    expect(res.status).not.toBe(0);
    expect(res.argv).toEqual([]);
  });

  it("rejects a pod target that is neither an ordinal nor `all`", () => {
    const res = runBotCtl(["netem", "congested_wifi", "xyz"]);
    expect(res.status).not.toBe(0);
    expect(res.argv).toEqual([]);
  });

  it("targets every running pod by default, one port each", () => {
    const res = runBotCtl(["clear"], { pods: ["videocall-bots-0", "videocall-bots-1"] });
    expect(res.status).toBe(0);
    expect(res.argv).toHaveLength(2);
    expect(res.argv[0]).toContain("--port 18080");
    expect(res.argv[1]).toContain("--port 18081");
  });

  it("targets one pod when given an ordinal", () => {
    const res = runBotCtl(["clear", "1"], { pods: ["videocall-bots-0", "videocall-bots-1"] });
    expect(res.stdout).toContain("✓ videocall-bots-1");
    expect(res.stdout).not.toContain("videocall-bots-0");
  });

  it("hands the bearer token to the child env, never on argv", () => {
    const res = runBotCtl(["clear"]);
    expect(res.status).toBe(0);
    expect(res.argv.join("\n")).not.toContain("fleet-secret");
    expect(res.argv.join("\n")).not.toContain("--token");
    expect(res.env).toContain("NPM_TOKEN_ENV=fleet-secret");
  });

  it("prefers BOT_CTL_KUBECONFIG over KUBECONFIG", () => {
    const res = runBotCtl(["clear"], { fake: { KUBECONFIG: "/should/not/win" } });
    expect(res.env.filter((l) => l.startsWith("KUBECONFIG="))).not.toContain(
      "KUBECONFIG=/should/not/win",
    );
    expect(res.env.join("\n")).toContain("kubeconfig");
  });
});

/** Exhaustive over BotStatus, so widening the union fails typecheck here first. */
const ACCEPTS_MUTATION: Record<BotStatus, boolean> = {
  priming: false,
  launching: false,
  joining: false,
  "in-meeting": true,
  leaving: false,
  done: false,
  failed: false,
};

describe("k8s/bot-ctl — bot selection", () => {
  const noMutation = (res: RunResult): string[] =>
    res.argv.filter((a) => !a.startsWith("ctl list"));

  for (const [status, accepts] of Object.entries(ACCEPTS_MUTATION) as [BotStatus, boolean][]) {
    it(`${accepts ? "mutates" : "refuses to mutate"} a ${status} bot`, () => {
      const res = runBotCtl(["mute"], { list: renderList([bot("bot-x", "alice", status)]) });
      if (accepts) {
        expect(res.status).toBe(0);
        expect(mutation(res)).toContain("ctl mute bot-x");
      } else {
        expect(res.status).not.toBe(0);
        expect(noMutation(res)).toEqual([]);
      }
    });
  }

  it("skips a retained done bot and mutates the live one", () => {
    // The registry keeps done/failed bots for an hour, and a participant name
    // may contain a space — both must not shift the parse.
    const res = runBotCtl(["video", "off"], {
      list: renderList([
        bot("bot-old", "alice smith", "done"),
        bot("bot-new", "bob", "in-meeting"),
      ]),
    });
    expect(res.status).toBe(0);
    expect(mutation(res)).toContain("ctl video bot-new");
  });

  it("skips a done bot whose meeting URL contains a space", () => {
    const spaced: BotSnapshot = {
      ...bot("bot-old", "alice", "done"),
      meetingURL: "https://example.test/meeting/room one",
    };
    const res = runBotCtl(["video", "off"], {
      list: renderList([spaced, bot("bot-new", "bob", "in-meeting")]),
    });
    expect(res.status).toBe(0);
    expect(mutation(res)).toContain("ctl video bot-new");
  });

  it("fails the pod when every bot is terminal", () => {
    const res = runBotCtl(["mute"], {
      list: renderList([bot("bot-old", "alice", "done"), bot("bot-bad", "bob", "failed")]),
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("no bot in-meeting");
    expect(noMutation(res)).toEqual([]);
  });

  it("fails the pod when the registry is empty", () => {
    const res = runBotCtl(["mute"], { list: renderList([]) });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("no bot in-meeting");
  });

  it("fails the pod on a status the wrapper does not know", () => {
    const unknown = { ...bot("bot-x", "alice", "in-meeting"), status: "zombie" as BotStatus };
    const res = runBotCtl(["mute"], { list: renderList([unknown]) });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("could not parse the bot table");
    expect(noMutation(res)).toEqual([]);
  });

  it("reports a launching bot for the read-only status command", () => {
    const res = runBotCtl(["status", "0"], {
      list: renderList([bot("bot-new", "bob", "launching")]),
    });
    expect(res.status).toBe(0);
    expect(mutation(res)).toContain("ctl status bot-new");
  });
});

describe("k8s/bot-ctl — failures are visible", () => {
  it("reports ✗, surfaces ctl's stderr, and exits non-zero", () => {
    const res = runBotCtl(["netem", "congested_wifi"], {
      pods: ["videocall-bots-0", "videocall-bots-1"],
      fake: { FAKE_CTL_FAIL: "1" },
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("ctl: request failed (stub)");
    expect(res.stderr).toContain("✗ videocall-bots-0");
    expect(res.stderr).toContain("2 pod(s) failed");
    expect(res.stdout).not.toContain("✓");
  });

  it("never sends the request when the port-forward died", () => {
    const res = runBotCtl(["video", "off"], {
      fake: { FAKE_PF_DIE: "1", BOT_CTL_PF_WAIT: "1" },
    });
    expect(res.status).not.toBe(0);
    expect(res.argv).toEqual([]);
    expect(res.stderr).toContain("port-forward to videocall-bots-0");
    expect(res.stderr).toContain("bind: address already in use");
  });

  it("distinguishes a kubectl failure from an empty fleet", () => {
    const failed = runBotCtl(["list"], { fake: { FAKE_PODS_FAIL: "1" } });
    expect(failed.status).not.toBe(0);
    expect(failed.stderr).toContain("kubectl could not list pods");

    const empty = runBotCtl(["list"], { pods: [] });
    expect(empty.status).not.toBe(0);
    expect(empty.stderr).toContain("no running pods");
    expect(empty.argv).toEqual([]);
  });

  it("refuses to run when the secret carries no token", () => {
    const res = runBotCtl(["list"], { fake: { FAKE_SECRET_B64: "" } });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("no non-empty 'token' key");
    expect(res.argv).toEqual([]);
  });

  it("prints usage and exits non-zero with no command", () => {
    const res = runBotCtl([]);
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("bot-ctl list");
  });

  it("prints usage on `help` and exits 0", () => {
    const res = runBotCtl(["help"]);
    expect(res.status).toBe(0);
    expect(res.stdout).toContain("BOT_CTL_KUBECONFIG");
  });
});
