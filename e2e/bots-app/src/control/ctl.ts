import { Command } from "commander";

import { NETSIM_PRESETS } from "../meeting-config";
import { findLatestTokenFile, readTokenFile } from "./auth";
import { type CtlClientConfig, ctlRequest } from "./client";
import { type BotSnapshot } from "./registry";

/**
 * Wire the Phase 4 `ctl` subcommand family onto the supplied
 * commander program. `defaultRunDir` is where the client looks for
 * token files unless `--state-file` overrides it.
 */
export function registerCtlCommands(program: Command, defaultRunDir: string): void {
  const ctl = program
    .command("ctl")
    .description(
      "Phase 4: introspect and mutate a running `bots-app run --ctl-port <port>` orchestrator. Auto-discovers the most recently-started orchestrator's token file under run/ctl-*.token.",
    );

  const sharedConnOptions = (cmd: Command): Command =>
    cmd
      .option(
        "--state-file <path>",
        "Explicit path to a ctl-<pid>.token file. Overrides token-file auto-discovery.",
      )
      .option("--port <port>", "Override the port from the token file (use with --token).")
      .option("--token <token>", "Override the bearer token from the token file (use with --port).")
      .option(
        "--run-dir <dir>",
        "Override the directory scanned for ctl-*.token files.",
        defaultRunDir,
      );

  sharedConnOptions(ctl.command("list"))
    .description("List every bot in the orchestrator's registry as a table.")
    .action(async (opts: ConnOptions) => {
      const cfg = await resolveConfig(opts);
      const res = await ctlRequest<{ bots: BotSnapshot[] }>(cfg, "GET", "/bots");
      printBotsTable(res.bots);
    });

  sharedConnOptions(ctl.command("status"))
    .description("Print one bot's full detail as JSON. Machine-parseable.")
    .argument("<botId>")
    .action(async (botId: string, opts: ConnOptions) => {
      const cfg = await resolveConfig(opts);
      const res = await ctlRequest<BotSnapshot>(cfg, "GET", `/bots/${encodeURIComponent(botId)}`);
      process.stdout.write(JSON.stringify(res, null, 2) + "\n");
    });

  sharedConnOptions(ctl.command("leave"))
    .description("Trigger graceful leave on a bot.")
    .argument("<botId>")
    .action(async (botId: string, opts: ConnOptions) => {
      const cfg = await resolveConfig(opts);
      const res = await ctlRequest<{ botId: string; action: string }>(
        cfg,
        "POST",
        `/bots/${encodeURIComponent(botId)}/leave`,
      );
      console.log(`leave: ${res.botId} (${res.action})`);
    });

  sharedConnOptions(ctl.command("kill"))
    .description("Force-kill a bot — skips graceful leaveMeeting. For tests + emergencies.")
    .argument("<botId>")
    .action(async (botId: string, opts: ConnOptions) => {
      const cfg = await resolveConfig(opts);
      const res = await ctlRequest<{ botId: string; action: string }>(
        cfg,
        "DELETE",
        `/bots/${encodeURIComponent(botId)}`,
      );
      console.log(`kill: ${res.botId} (${res.action})`);
    });

  sharedConnOptions(ctl.command("ttl"))
    .description("Mutate a bot's TTL — either set absolute (--set) or extend (--extend).")
    .argument("<botId>")
    .option("--set <duration>", 'Set TTL to <duration> (e.g. "10m", "infinite")')
    .option("--extend <duration>", "Extend remaining TTL by <duration>")
    .action(async (botId: string, opts: ConnOptions & { set?: string; extend?: string }) => {
      if (!opts.set && !opts.extend) {
        console.error("ctl ttl: --set <duration> or --extend <duration> is required");
        process.exit(2);
      }
      if (opts.set && opts.extend) {
        console.error("ctl ttl: --set and --extend are mutually exclusive");
        process.exit(2);
      }
      const body = opts.set ? { ttl: opts.set } : { extendBy: opts.extend };
      const cfg = await resolveConfig(opts);
      const res = await ctlRequest<{ botId: string; ttl: string }>(
        cfg,
        "POST",
        `/bots/${encodeURIComponent(botId)}/ttl`,
        body,
      );
      console.log(`ttl: ${res.botId} → ${res.ttl}`);
    });

  sharedConnOptions(ctl.command("tune"))
    .description(
      "Change a bot's netsim profile mid-flight. Forces a reconnect (the bot rejoins with the new shim).",
    )
    .argument("<botId>")
    .requiredOption("--network <profile>", `One of: ${NETSIM_PRESETS.join(", ")}`)
    .action(async (botId: string, opts: ConnOptions & { network: string }) => {
      if (!NETSIM_PRESETS.includes(opts.network)) {
        console.error(
          `ctl tune: --network must be one of: ${NETSIM_PRESETS.join(", ")} (got "${opts.network}")`,
        );
        process.exit(2);
      }
      const cfg = await resolveConfig(opts);
      const res = await ctlRequest<{ botId: string; network: string; note?: string }>(
        cfg,
        "POST",
        `/bots/${encodeURIComponent(botId)}/network`,
        { network: opts.network },
      );
      console.log(`tune: ${res.botId} → network=${res.network}${res.note ? ` (${res.note})` : ""}`);
    });

  sharedConnOptions(ctl.command("mute"))
    .description("Toggle a bot's microphone. `mute` mutes; `mute --off` unmutes.")
    .argument("<botId>")
    .option("--off", "Unmute instead of mute", false)
    .action(async (botId: string, opts: ConnOptions & { off: boolean }) => {
      const cfg = await resolveConfig(opts);
      const mic = !opts.off;
      const res = await ctlRequest<{ botId: string; mic: boolean }>(
        cfg,
        "POST",
        `/bots/${encodeURIComponent(botId)}/mute`,
        { mic },
      );
      console.log(`mute: ${res.botId} → mic ${res.mic ? "OFF (muted)" : "ON (unmuted)"}`);
    });

  sharedConnOptions(ctl.command("video"))
    .description("Toggle a bot's camera. `video` turns it off; `video --on` turns it on.")
    .argument("<botId>")
    .option("--on", "Turn the camera on instead of off", false)
    .action(async (botId: string, opts: ConnOptions & { on: boolean }) => {
      const cfg = await resolveConfig(opts);
      const camera = !opts.on;
      const res = await ctlRequest<{ botId: string; camera: boolean }>(
        cfg,
        "POST",
        `/bots/${encodeURIComponent(botId)}/video`,
        { camera },
      );
      console.log(`video: ${res.botId} → camera ${res.camera ? "OFF" : "ON"}`);
    });

  sharedConnOptions(ctl.command("share"))
    .description("Toggle a bot's screen share. `share` starts; `share --off` stops.")
    .argument("<botId>")
    .option("--off", "Stop sharing instead of starting", false)
    .action(async (botId: string, opts: ConnOptions & { off: boolean }) => {
      const cfg = await resolveConfig(opts);
      const share = !opts.off;
      const res = await ctlRequest<{ botId: string; share: boolean }>(
        cfg,
        "POST",
        `/bots/${encodeURIComponent(botId)}/share`,
        { share },
      );
      console.log(`share: ${res.botId} → ${res.share ? "ON" : "OFF"}`);
    });

  sharedConnOptions(ctl.command("duplicate"))
    .description(
      "Clone a bot's config and launch the clone. Override participant / ttl / network individually.",
    )
    .argument("<botId>")
    .option("--participant <name>", "Override the participant handle on the duplicate")
    .option("--ttl <duration>", "Override the duplicate's TTL")
    .option(
      "--network <profile>",
      `Override the duplicate's network (${NETSIM_PRESETS.join(", ")})`,
    )
    .action(
      async (
        botId: string,
        opts: ConnOptions & { participant?: string; ttl?: string; network?: string },
      ) => {
        if (opts.network !== undefined && !NETSIM_PRESETS.includes(opts.network)) {
          console.error(
            `ctl duplicate: --network must be one of: ${NETSIM_PRESETS.join(", ")} (got "${opts.network}")`,
          );
          process.exit(2);
        }
        const body: Record<string, unknown> = {};
        if (opts.participant !== undefined) body.participant = opts.participant;
        if (opts.ttl !== undefined) body.ttl = opts.ttl;
        if (opts.network !== undefined) body.network = opts.network;
        const cfg = await resolveConfig(opts);
        const res = await ctlRequest<{ botId: string }>(
          cfg,
          "POST",
          `/bots/${encodeURIComponent(botId)}/duplicate`,
          body,
        );
        console.log(`duplicate: source=${botId} → newBotId=${res.botId}`);
      },
    );
}

interface ConnOptions {
  stateFile?: string;
  port?: string;
  token?: string;
  runDir: string;
}

/**
 * Resolve the connection config from the supplied CLI options.
 * Priority:
 *   1. `--port` + `--token` (both required when used)
 *   2. `--state-file <path>` (explicit token-file path)
 *   3. Auto-discovery: most-recent `ctl-*.token` under `--run-dir`
 *
 * Throws — and the CLI exits non-zero — if no token file is found.
 */
async function resolveConfig(opts: ConnOptions): Promise<CtlClientConfig> {
  if (opts.port !== undefined || opts.token !== undefined) {
    if (opts.port === undefined || opts.token === undefined) {
      throw new Error("ctl: --port and --token must be supplied together");
    }
    const port = Number.parseInt(opts.port, 10);
    if (!Number.isFinite(port) || port <= 0 || port > 65535) {
      throw new Error(`ctl: --port must be a positive integer (got "${opts.port}")`);
    }
    return { port, token: opts.token };
  }

  let tokenFilePath: string | null = opts.stateFile ?? null;
  if (tokenFilePath === null) {
    tokenFilePath = await findLatestTokenFile(opts.runDir);
    if (tokenFilePath === null) {
      throw new Error(
        `ctl: no token file found under ${opts.runDir}. ` +
          "Start an orchestrator with `bots-app run --ctl-port auto`, or pass --state-file / --port + --token explicitly.",
      );
    }
  }
  const contents = await readTokenFile(tokenFilePath);
  return { port: contents.port, token: contents.token };
}

/**
 * Render the `ctl list` table. Plain ASCII so it pipes cleanly into
 * `grep` / `awk` and renders identically across terminals.
 */
function printBotsTable(bots: BotSnapshot[]): void {
  if (bots.length === 0) {
    console.log("(no bots in registry)");
    return;
  }
  const rows = bots.map((b) => [
    b.botId,
    b.participant,
    b.status,
    b.ttlRemainingMs === null ? "infinite" : `${Math.ceil(b.ttlRemainingMs / 1_000)}s`,
    b.network ?? "-",
    b.meetingURL,
  ]);
  const headers = ["BOT_ID", "PARTICIPANT", "STATUS", "TTL_REMAINING", "NETWORK", "MEETING_URL"];
  const widths = headers.map((h, i) => Math.max(h.length, ...rows.map((r) => r[i].length)));
  const fmt = (cols: string[]): string => cols.map((c, i) => c.padEnd(widths[i])).join("  ");
  console.log(fmt(headers));
  console.log(widths.map((w) => "-".repeat(w)).join("  "));
  for (const r of rows) console.log(fmt(r));
}
