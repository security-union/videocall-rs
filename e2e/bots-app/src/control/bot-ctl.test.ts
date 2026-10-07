import { spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it, vi } from "vitest";
import { parse as parseYaml } from "yaml";

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
*"get statefulset"*envFrom*)
  echo "ENV_GET=$args" >>"$ENV_LOG"
  echo "Warning: stub stderr" >&2
  printf '%s' "\${FAKE_LIVE_ENV:-}"
  exit "\${FAKE_LIVE_ENV_RC:-0}"
  ;;
"create --dry-run=client --validate=false -f -"*)
  echo "ENV_CREATE=$args" >>"$ENV_LOG"
  cat >"$DIFF_DIR/CREATE"
  echo "Warning: stub stderr" >&2
  printf '%s' "\${FAKE_COMMITTED_ENV:-}"
  exit "\${FAKE_COMMITTED_ENV_RC:-0}"
  ;;
*"get statefulset"*)
  if [ -n "\${FAKE_STS_FAIL:-}" ]; then
    echo "Unable to connect to the server: i/o timeout" >&2
    exit 1
  fi
  printf '%s' "\${FAKE_STS:-}"
  ;;
*"get resourcequota"*)
  if [ -n "\${FAKE_QUOTA_FAIL:-}" ]; then
    echo "Unable to connect to the server: i/o timeout" >&2
    exit 1
  fi
  printf '%s' "\${FAKE_QUOTA:-}"
  ;;
*"get pods"*controller-revision-hash*)
  printf '%s' "\${FAKE_FLEET_PODS:-}"
  ;;
"diff "*"-f -"*)
  echo "DIFF_ARGS=$args" >>"$ENV_LOG"
  cat >"$DIFF_DIR/stdin"
  if grep -q '^kind: StatefulSet' "$DIFF_DIR/stdin"; then
    mv "$DIFF_DIR/stdin" "$DIFF_DIR/STS"
    printf '%s' "\${FAKE_DIFF_STS_OUT:-}"
    exit "\${FAKE_DIFF_STS_RC:-0}"
  fi
  mv "$DIFF_DIR/stdin" "$DIFF_DIR/NS"
  printf '%s' "\${FAKE_DIFF_NS_OUT:-}"
  exit "\${FAKE_DIFF_NS_RC:-0}"
  ;;
*"get pods"*)
  if [ -n "\${FAKE_PODS_FAIL:-}" ]; then
    echo "Unable to connect to the server: i/o timeout" >&2
    exit 1
  fi
  printf '%s' "\${FAKE_PODS:-}"
  ;;
*port-forward*)
  for a in "$@"; do case "$a" in *:8080) lport="\${a%%:*}" ;; esac; done
  case "\${FAKE_PF_MODE:-}" in
  die)
    echo "Unable to listen on port $lport: bind: address already in use" >&2
    exit 1
    ;;
  silent) ;;
  v6only) echo "Forwarding from [::1]:$lport -> 8080" ;;
  *)
    echo "Forwarding from 127.0.0.1:$lport -> 8080"
    echo "Forwarding from [::1]:$lport -> 8080"
    ;;
  esac
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
  /** Manifests piped to kubectl: `diff -f -` keyed STS / NS, `create --dry-run` CREATE. */
  diffed: Record<string, string>;
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
  const diffDir = join(workdir, "diff");
  mkdirSync(diffDir);

  const pods = opts.pods ?? ["videocall-bots-0"];
  const env: Record<string, string> = {
    PATH: `${binDir}:${process.env.PATH ?? "/usr/bin:/bin"}`,
    HOME: workdir,
    E2E_DIR: workdir,
    BOT_CTL_KUBECONFIG: join(workdir, "kubeconfig"),
    BOT_NAMESPACE: "bot-load",
    BOT_STS: "videocall-bots",
    // A ceiling, not a sleep: the stub's Forwarding line ends the wait at once.
    BOT_CTL_PF_WAIT: "5",
    DIFF_DIR: diffDir,
    ARGV_LOG: argvLog,
    ENV_LOG: envLog,
    FAKE_LIST_FILE: listFile,
    FAKE_SECRET_B64: Buffer.from("fleet-secret").toString("base64"),
    FAKE_PODS: pods.length === 0 ? "" : `${pods.join("\n")}\n`,
    ...opts.fake,
  };
  try {
    const res = spawnSync("bash", [SCRIPT, ...args], { env, encoding: "utf8" });
    const diffed: Record<string, string> = {};
    for (const which of ["STS", "NS", "CREATE"]) {
      const path = join(diffDir, which);
      if (existsSync(path)) diffed[which] = readFileSync(path, "utf8");
    }
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
      diffed,
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
      fake: { FAKE_PF_MODE: "die", BOT_CTL_PF_WAIT: "1" },
    });
    expect(res.status).not.toBe(0);
    expect(res.argv).toEqual([]);
    expect(res.stderr).toContain("port-forward to videocall-bots-0");
    expect(res.stderr).toContain("bind: address already in use");
  });

  for (const mode of ["silent", "v6only"]) {
    it(`never sends the request when kubectl is alive but not on 127.0.0.1 (${mode})`, () => {
      const res = runBotCtl(["video", "off"], {
        fake: { FAKE_PF_MODE: mode, BOT_CTL_PF_WAIT: "0.3" },
      });
      expect(res.status).not.toBe(0);
      expect(res.argv).toEqual([]);
      expect(res.stderr).toContain("not listening on 127.0.0.1");
    });
  }

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

const STS_YAML = readFileSync(
  fileURLToPath(new URL("../../k8s/statefulset.yaml", import.meta.url)),
  "utf8",
);

interface ManifestContainer {
  name: string;
  env?: { name: string }[];
  envFrom?: { secretRef?: { name: string }; configMapRef?: { name: string }; prefix?: string }[];
}

/** COMMITTED_ENV's row format is this template's output; a template change must update both. */
const ENV_TEMPLATE = String.raw`{{range .spec.template.spec.containers}}{{$c := .name}}{{range .env}}container {{$c}} env {{.name}}{{"\n"}}{{end}}{{range .envFrom}}container {{$c}} envFrom {{with .configMapRef}}configMap {{.name}}{{end}}{{with .secretRef}}secret {{.name}}{{end}} prefix={{with .prefix}}{{.}}{{end}}{{"\n"}}{{end}}{{end}}`;

/** The committed manifest's env in ENV_TEMPLATE's row format. */
const COMMITTED_ENV = (
  parseYaml(STS_YAML) as { spec: { template: { spec: { containers: ManifestContainer[] } } } }
).spec.template.spec.containers
  .flatMap((c) => [
    ...(c.env ?? []).map((e) => `container ${c.name} env ${e.name}`),
    ...(c.envFrom ?? []).map((f) => {
      const src = f.configMapRef
        ? `configMap ${f.configMapRef.name}`
        : `secret ${f.secretRef?.name ?? ""}`;
      return `container ${c.name} envFrom ${src} prefix=${f.prefix ?? ""}`;
    }),
  ])
  .map((l) => `${l}\n`)
  .join("");

/** The live `bot-load` quota as read on 2026-09-30. */
const LIVE_HARD: Record<string, string> = {
  "limits.cpu": "176",
  "limits.memory": "96Gi",
  pods: "48",
  "requests.cpu": "48",
  "requests.memory": "64Gi",
};
const LIVE_USED: Record<string, string> = {
  "limits.cpu": "700m",
  "limits.memory": "448Mi",
  pods: "7",
  "requests.cpu": "350m",
  "requests.memory": "224Mi",
};
const LIVE_TEMPLATE = "limits.cpu 8\nlimits.memory 3Gi\nrequests.cpu 1500m\nrequests.memory 2Gi\n";

function quotaTemplate(used: Record<string, string> = {}): string {
  const rows = (kind: string, m: Record<string, string>): string =>
    Object.entries(m)
      .map(([k, v]) => `${kind} ${k} ${v}\n`)
      .join("");
  return rows("hard", LIVE_HARD) + rows("used", { ...LIVE_USED, ...used });
}

function preflight(
  n: string,
  opts: {
    used?: Record<string, string>;
    pods?: string[];
    replicas?: number;
    fake?: Record<string, string>;
  } = {},
): RunResult {
  const pods = opts.pods ?? [];
  const replicas = opts.replicas ?? pods.length;
  return runBotCtl(["preflight", n], {
    fake: {
      FAKE_STS: `${replicas} ${replicas} videocall-bots-new 4 4\n${LIVE_TEMPLATE}`,
      FAKE_QUOTA: quotaTemplate(opts.used),
      FAKE_FLEET_PODS: pods.map((p) => `${p}\n`).join(""),
      FAKE_LIVE_ENV: COMMITTED_ENV,
      FAKE_COMMITTED_ENV: COMMITTED_ENV,
      ...opts.fake,
    },
  });
}

describe("k8s/bot-ctl preflight — quota admission", () => {
  it("passes when N fits the live headroom", () => {
    const res = preflight("10");
    expect(res.status).toBe(0);
    expect(res.stdout).toContain(
      "✓ 10 new pod(s) fit resourcequota/bot-load-quota; tightest is limits.cpu: need 80, headroom 175300m (at most 21 new)",
    );
  });

  it("parses a millicore `used` — 21 fit, 22 do not", () => {
    expect(preflight("21").status).toBe(0);
    const res = preflight("22");
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ 22 new pod(s) do not fit resourcequota/bot-load-quota; limits.cpu binds: need 176, headroom 175300m (at most 21 new)",
    );
  });

  it("names pods, not cpu, when pods binds first", () => {
    const res = preflight("22", { used: { pods: "45" } });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ 22 new pod(s) do not fit resourcequota/bot-load-quota; pods binds: need 22, headroom 3 (at most 3 new)",
    );
  });

  it("parses Mi against Gi — 20 fit, 21 do not", () => {
    const used = { "limits.memory": "36352Mi" };
    expect(preflight("20", { used }).status).toBe(0);
    const res = preflight("21", { used });
    expect(res.stderr).toContain(
      "limits.memory binds: need 63Gi, headroom 61952Mi (at most 20 new)",
    );
  });

  it("charges requests.* with the template's requests, not its limits", () => {
    const used = { "requests.cpu": "45" };
    expect(preflight("2", { used }).status).toBe(0);
    const res = preflight("3", { used });
    expect(res.stderr).toContain("requests.cpu binds: need 4500m, headroom 3 (at most 2 new)");
  });

  it("charges only the pods a scale would create", () => {
    const res = preflight("24", {
      pods: [
        "videocall-bots-0 Running videocall-bots-new",
        "videocall-bots-1 Running videocall-bots-new",
        "videocall-bots-2 Running videocall-bots-new",
      ],
    });
    expect(res.status).toBe(0);
    expect(res.stdout).toContain("✓ 21 new pod(s) fit");
  });

  it("fails closed when the quota cannot be read", () => {
    const cases: Record<string, string>[] = [{ FAKE_QUOTA_FAIL: "1" }, { FAKE_QUOTA: "" }];
    for (const fake of cases) {
      const res = preflight("1", { fake });
      expect(res.status).not.toBe(0);
      expect(res.stderr).toMatch(
        /✗ could not read status\.hard of resourcequota\/bot-load-quota \(exit \d\); admission unchecked/,
      );
      expect(res.stdout).not.toContain("fit resourcequota");
    }
  });

  it("fails closed on a quota key it does not model", () => {
    const res = preflight("1", {
      fake: { FAKE_QUOTA: `${quotaTemplate()}hard persistentvolumeclaims 20\n` },
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "constrains persistentvolumeclaims, which preflight does not check",
    );
  });

  it("fails closed on a template with initContainers", () => {
    const res = preflight("1", {
      fake: { FAKE_STS: `0 0 videocall-bots-new 4 4\ninitContainers\n${LIVE_TEMPLATE}` },
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ the pod template has initContainers, which preflight does not model; admission unchecked",
    );
    expect(res.stdout).not.toContain("fit resourcequota");
  });

  it("fails closed on a quantity it cannot parse, in the template or the quota", () => {
    const tmpl = preflight("1", {
      fake: {
        FAKE_STS: `0 0 videocall-bots-new 4 4\n${LIVE_TEMPLATE.replace("limits.cpu 8", "limits.cpu 1.5")}`,
      },
    });
    expect(tmpl.status).not.toBe(0);
    expect(tmpl.stderr).toContain("✗ unparseable quantity for limits.cpu in the pod template");
    expect(tmpl.stderr).toContain("admission incomplete");
    expect(tmpl.stdout).not.toContain("fit resourcequota");

    const quota = preflight("1", {
      fake: { FAKE_QUOTA: quotaTemplate().replace("hard limits.cpu 176", "hard limits.cpu 1e3") },
    });
    expect(quota.status).not.toBe(0);
    expect(quota.stderr).toContain(
      "✗ unparseable hard or used quantity for limits.cpu in resourcequota/bot-load-quota",
    );
    expect(quota.stdout).not.toContain("fit resourcequota");
  });

  it("fails closed when the StatefulSet cannot be read", () => {
    const res = preflight("1", { fake: { FAKE_STS_FAIL: "1" } });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("could not read statefulset/videocall-bots");
  });

  it("rejects a non-numeric count before calling kubectl", () => {
    const res = preflight("ten");
    expect(res.status).not.toBe(0);
    expect(res.env.filter((l) => l.startsWith("KUBECONFIG="))).toEqual([]);
  });
});

describe("k8s/bot-ctl preflight — drift and rollout", () => {
  it("diffs the committed StatefulSet with only replicas set to the live value", () => {
    const committed = /^ {2}replicas: (\d+)$/m.exec(STS_YAML)?.[1];
    const res = preflight("1");
    expect(res.status).toBe(0);
    const sent = res.diffed.STS.split("\n");
    const orig = STS_YAML.split("\n");
    expect(sent).toHaveLength(orig.length);
    const changed = sent.filter((l, i) => l !== orig[i]);
    expect(changed).toEqual(committed === "0" ? [] : ["  replicas: 0"]);
    expect(res.diffed.NS).toContain("kind: ResourceQuota");
    expect(res.env.filter((l) => l.startsWith("DIFF_ARGS="))).toEqual([
      "DIFF_ARGS=diff -n bot-load -f -",
      "DIFF_ARGS=diff -n bot-load -f -",
    ]);
    expect(res.stdout).toContain(
      `✓ statefulset/videocall-bots (replicas ignored: live 0, committed ${committed}) matches k8s/statefulset.yaml`,
    );
    expect(res.stdout).toContain(
      "✓ statefulset/videocall-bots pod template env and envFrom names match k8s/statefulset.yaml",
    );
  });

  it("fails on env the live template has and the manifest lacks, though kubectl diff is clean", () => {
    const res = preflight("1", {
      fake: {
        FAKE_LIVE_ENV: `${[...COMMITTED_ENV.trimEnd().split("\n").reverse(), "container bot env BOT_NETEM_PROFILE"].join("\n")}\n`,
      },
    });
    expect(res.status).not.toBe(0);
    expect(res.stdout).toMatch(/✓ statefulset\/videocall-bots .* matches k8s\/statefulset\.yaml/);
    expect(res.stderr).toContain(
      "✗ statefulset/videocall-bots pod template has container bot env BOT_NETEM_PROFILE, which k8s/statefulset.yaml does not",
    );
    expect(res.stdout).not.toContain("env and envFrom names match");
    expect(res.stderr.match(/^✗ .*(pod template has|k8s\/statefulset\.yaml has)/gm)).toHaveLength(
      1,
    );
    expect(res.diffed.CREATE).toBe(STS_YAML);
    const tmpl = (key: string): string | undefined =>
      res.env.find((l) => l.startsWith(key))?.split("go-template=")[1];
    expect(tmpl("ENV_GET=")).toBe(ENV_TEMPLATE);
    expect(tmpl("ENV_CREATE=")).toBe(ENV_TEMPLATE);
  });

  it("fails on manifest env or envFrom the live template lacks", () => {
    const live = COMMITTED_ENV.replace("container bot env BOT_CTL_BIND\n", "").replace(
      "secret bot-accounts",
      "secret other-accounts",
    );
    const res = preflight("1", { fake: { FAKE_LIVE_ENV: live } });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ k8s/statefulset.yaml has container bot env BOT_CTL_BIND, which statefulset/videocall-bots pod template does not",
    );
    expect(res.stderr).toContain(
      "✗ k8s/statefulset.yaml has container bot envFrom secret bot-accounts prefix=, which",
    );
    expect(res.stderr).toContain("pod template has container bot envFrom secret other-accounts");
  });

  it("fails closed when either env set cannot be read", () => {
    const live = preflight("1", { fake: { FAKE_LIVE_ENV_RC: "1" } });
    expect(live.status).not.toBe(0);
    expect(live.stderr).toContain("env drift unknown");
    const render = preflight("1", { fake: { FAKE_COMMITTED_ENV_RC: "1" } });
    expect(render.status).not.toBe(0);
    expect(render.stderr).toContain(
      "could not render the env of k8s/statefulset.yaml (exit 1), env drift unknown",
    );
    const committed = preflight("1", { fake: { FAKE_COMMITTED_ENV: "" } });
    expect(committed.status).not.toBe(0);
    expect(committed.stderr).toContain(
      "could not render the env of k8s/statefulset.yaml (exit 0), env drift unknown",
    );
  });

  it("reports drift and a failed diff, and exits non-zero", () => {
    const res = preflight("1", {
      fake: {
        FAKE_DIFF_STS_RC: "1",
        FAKE_DIFF_STS_OUT: '-              cpu: "4"\n+              cpu: "8"\n',
        FAKE_DIFF_NS_RC: "2",
        FAKE_DIFF_NS_OUT: "error: forbidden",
      },
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ statefulset/videocall-bots (replicas ignored: live 0, committed",
    );
    expect(res.stderr).toContain('+              cpu: "8"');
    expect(res.stderr).toContain(
      "✗ kubectl diff of k8s/namespace.yaml failed (exit 2), drift unknown:",
    );
  });

  it("checks every pod's revision, not just ordinal 0", () => {
    const res = preflight("3", {
      pods: [
        "videocall-bots-0 Running videocall-bots-new",
        "videocall-bots-1 Running videocall-bots-old",
        "videocall-bots-2 Running videocall-bots-new",
      ],
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ pod videocall-bots-1 is on revision videocall-bots-old, not the StatefulSet's update revision videocall-bots-new",
    );
    expect(res.stderr).not.toContain("videocall-bots-0 is on");
    expect(res.stderr).not.toContain("videocall-bots-2 is on");
  });

  it("withholds the per-pod verdict until the controller observes the generation", () => {
    const res = preflight("3", {
      pods: [
        "videocall-bots-0 Running videocall-bots-new",
        "videocall-bots-1 Running videocall-bots-new",
        "videocall-bots-2 Running videocall-bots-new",
      ],
      fake: { FAKE_STS: `3 3 videocall-bots-new 5 4\n${LIVE_TEMPLATE}` },
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "✗ statefulset/videocall-bots generation 5 not yet observed by the controller (observed 4); re-run preflight",
    );
    expect(res.stdout).not.toMatch(/all .* on update revision/);
  });

  it("fails when the pod list disagrees with the StatefulSet's pod count", () => {
    const res = preflight("3", {
      pods: ["videocall-bots-0 Running videocall-bots-new"],
      replicas: 3,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain(
      "found 1 pod(s) labelled app.kubernetes.io/instance=videocall-bots but the StatefulSet reports 3",
    );
  });
});
