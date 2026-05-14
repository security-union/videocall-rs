import { existsSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline/promises";

import { Command } from "commander";
import { chromium } from "@playwright/test";

import {
  type AuthBackend,
  chooseAuthBackend,
  defaultSsoStatePath,
  storageStatePath,
} from "./auth/storage-state";
import { captureSsoStateInteractive } from "./auth/sso-capture";
import { writeFileSync } from "node:fs";

import { registerCtlCommands } from "./control/ctl";
import { generateBotId } from "./control/registry";
import { defaultTokenFilePath, generateToken, writeTokenFile } from "./control/auth";
import { prepareParticipantCostume } from "./costumes";
import { firstNParticipantNames, loadManifest, type Manifest } from "./manifest";
import {
  emitMeetingConfigYaml,
  generateMeetingConfig,
  loadMeetingConfig,
  NETSIM_PRESETS,
} from "./meeting-config";
import { runBotsToCompletion, type BotTask } from "./orchestrator";
import { prepareParticipantAudio } from "./stitcher";
import { parseDuration, Ttl } from "./ttl";

const program = new Command();

program
  .name("bots-app")
  .description("Browser-driven bot CLI for videocall meetings")
  .version("0.1.0");

program
  .command("run")
  .description(
    "Launch one or more browser bots that join the meeting concurrently and hold the session",
  )
  .option(
    "--meeting-url <url>",
    "Full meeting URL (e.g. https://app.videocall.fnxlabs.com/meeting/TonyBots). Required unless --config is set (the file carries it).",
  )
  .option(
    "--participant <name>",
    'Single-bot mode: participant handle (e.g. "alice") or full email. Mutually exclusive with --users / --config.',
  )
  .option(
    "--users <N>",
    "Multi-bot mode: launch N bots, picking the first N named participants from the manifest in order (alice, bob, carol, ...). Mutually exclusive with --participant.",
  )
  .option(
    "--max-users <N>",
    "Resource cap when --users is set — refuses to launch more than this many bots. Default 10.",
    "10",
  )
  .option(
    "--config <path>",
    "Path to a meeting-config YAML emitted by `bots-app gen` (or hand-rolled). Provides meeting_url + bots[] from the file; CLI flags still override individual fields. Mutually exclusive with --participant and --users.",
  )
  .option("--display-name <name>", "Display name shown in the meeting", undefined)
  .option("--headless", "Run Chrome headless (default: headed)", false)
  .option(
    "--ttl <duration>",
    'Bot lifespan — "<int>s|m|h" or "infinite". On expiry the bot leaves the meeting and exits. Shared across all bots in --users mode.',
    "5m",
  )
  .option(
    "--manifest <path>",
    "Path to bot/conversation/manifest.yaml. When set together with --assets-dir, the bot uses the prep'd WAV + y4m for this participant via Chrome's --use-file-for-fake-*-capture flags. Pass an empty string to skip and fall back to Chrome's default fake devices.",
    join(repoRoot(), "bot/conversation/manifest.yaml"),
  )
  .option(
    "--assets-dir <dir>",
    "Directory containing audio/<name>.wav and costumes/<name>.y4m (the output of `bots-app prep-assets`).",
    join(repoRoot(), "e2e/bots-app/run"),
  )
  .option(
    "--auth <backend>",
    'Auth backend override: "jwt" (cookie injection; for local + HCL + previews), "storage-state" (replay a captured Google OAuth session from `bots-app login`; for app.videocall.rs), or "none" (skip auth entirely; for meetings that allow guest joining). When omitted, picks automatically by hostname.',
  )
  .option(
    "--storage-state-file <path>",
    "Explicit path to the captured storage-state JSON. Defaults to <assets-dir>/auth/<participant>.json when --auth=storage-state is in effect.",
  )
  .option(
    "--sso-state-file <path>",
    'Path to a captured HCL SSO storage-state JSON (from `bots-app sso-login`). Loaded in addition to JWT cookie injection when --auth=jwt. Pass "" to skip. Defaults to <assets-dir>/auth/hcl-sso.json — loaded only if the file exists.',
  )
  .option(
    "--network <profile>",
    `Netsim profile applied to the bot's outbound media (one of: ${NETSIM_PRESETS.join(", ")}). Appends ?netsim=<profile> to the meeting URL — only takes effect when the served videocall-client is built with --features netsim. In --config mode this acts as a default that per-bot config entries override. See discussion #793 phase 3.`,
  )
  .option(
    "--ctl-port <port>",
    'Phase 4: bind a local HTTP control API so `bots-app ctl <cmd>` can introspect and mutate the running fleet. Pass an integer port, "auto" to let the kernel pick a free port, or omit to disable the control surface entirely. The token is written to run/ctl-<pid>.token (mode 0600). See discussion #793 phase 4.',
  )
  .action(async (opts: RunCommandOptions) => {
    // Mutual exclusion / required-arg checks ──────────────────────────
    const modeCount = [opts.participant, opts.users, opts.config].filter(Boolean).length;
    if (modeCount > 1) {
      console.error("bots-app: --participant, --users, and --config are mutually exclusive");
      process.exit(2);
    }
    if (modeCount === 0) {
      console.error("bots-app: one of --participant, --users, or --config is required");
      process.exit(2);
    }

    // Validate --network up-front (before reading the config file)
    // so a typo on the CLI fails fast with the same error a bad config
    // would produce.
    if (opts.network !== undefined && !NETSIM_PRESETS.includes(opts.network)) {
      console.error(
        `bots-app: --network must be one of: ${NETSIM_PRESETS.join(", ")} (got "${opts.network}")`,
      );
      process.exit(2);
    }

    // Load the config file first (if any) — it can supply meeting_url
    // + per-bot list + a default ttl that --ttl can override.
    let configMeetingUrl: string | null = null;
    let configBots: { participant: string; ttl?: string; network?: string; auth?: string }[] = [];
    let configTtl: string | null = null;
    let configNetwork: string | null = null;
    let configAuth: string | null = null;
    if (opts.config) {
      try {
        const cfg = loadMeetingConfig(opts.config);
        configMeetingUrl = cfg.meetingUrl;
        configBots = cfg.bots;
        configTtl = cfg.ttl ?? null;
        configNetwork = cfg.network ?? null;
        configAuth = cfg.auth ?? null;
        console.log(
          `bots-app: loaded meeting config from ${opts.config} (${cfg.bots.length} bot(s)` +
            (cfg.meta?.seed !== undefined ? `, seed=${cfg.meta.seed}` : "") +
            `)`,
        );
      } catch (e) {
        console.error(`bots-app: failed to read config ${opts.config}:`, (e as Error).message);
        process.exit(2);
      }
    }

    const meetingUrl = opts.meetingUrl ?? configMeetingUrl;
    if (!meetingUrl) {
      console.error("bots-app: --meeting-url is required (or set it in the --config file)");
      process.exit(2);
    }

    // TTL resolution: CLI flag wins over config file; config file wins
    // over the implicit default.
    const ttlRaw = opts.ttl !== "5m" ? opts.ttl : (configTtl ?? opts.ttl);
    let ttl: Ttl;
    try {
      ttl = parseDuration(ttlRaw);
    } catch (e) {
      console.error(`bots-app: ${(e as Error).message}`);
      process.exit(2);
    }

    let manifest: Manifest | null = null;
    if (opts.manifest && opts.manifest !== "") {
      if (!existsSync(opts.manifest)) {
        console.warn(
          `bots-app: manifest not found at ${opts.manifest} — proceeding without fake-device wiring (Chrome will use its default fake pattern). Run \`bots-app prep-assets\` to fix.`,
        );
      } else {
        manifest = loadManifest(opts.manifest).manifest;
      }
    }

    // Resolve the participant list:
    //   --config  → bots[] from the file
    //   --users N → first N from the manifest
    //   --participant <name> → single-bot
    let participants: string[];
    if (opts.config) {
      participants = configBots.map((b) => b.participant);
    } else if (opts.users) {
      const n = Number.parseInt(opts.users, 10);
      const maxUsers = Number.parseInt(opts.maxUsers, 10);
      if (!Number.isFinite(n) || n <= 0) {
        console.error(`bots-app: --users must be a positive integer, got "${opts.users}"`);
        process.exit(2);
      }
      if (Number.isFinite(maxUsers) && maxUsers > 0 && n > maxUsers) {
        console.error(
          `bots-app: --users ${n} exceeds --max-users ${maxUsers}; raise --max-users to override`,
        );
        process.exit(2);
      }
      if (!manifest) {
        console.error(
          `bots-app: --users requires a manifest (at ${opts.manifest}). Run \`bots-app prep-assets\` first or pass --manifest <path>.`,
        );
        process.exit(2);
      }
      const namedCount = manifest.participants.length;
      if (n > namedCount) {
        console.error(
          `bots-app: --users ${n} exceeds the manifest's ${namedCount} named participants`,
        );
        process.exit(2);
      }
      participants = firstNParticipantNames(manifest, n);
    } else {
      participants = [opts.participant as string];
    }

    let authOverride: AuthBackend | undefined;
    if (opts.auth) {
      if (opts.auth !== "jwt" && opts.auth !== "storage-state" && opts.auth !== "none") {
        console.error(
          `bots-app: --auth must be "jwt", "storage-state", or "none", got "${opts.auth}"`,
        );
        process.exit(2);
      }
      authOverride = opts.auth;
    }
    // Config-file auth is consulted only when there's no explicit
    // --auth CLI override. The meeting-config loader already validated
    // any `auth:` against `AUTH_BACKEND_NAMES`, so the assertion below
    // narrows safely.
    const effectiveAuthOverride: AuthBackend | undefined =
      authOverride ?? (configAuth ? (configAuth as AuthBackend) : undefined);
    const hostname = new URL(meetingUrl).hostname;
    const authBackend = chooseAuthBackend(hostname, effectiveAuthOverride);
    const ssoStateFile =
      authBackend === "jwt" ? (opts.ssoStateFile ?? defaultSsoStatePath(opts.assetsDir)) : null;

    const tasks: BotTask[] = participants.map((participant) => {
      const displayName =
        opts.displayName && participants.length === 1
          ? opts.displayName
          : defaultDisplayName(participant);
      // Per-bot auth override from --config trumps the meeting-level
      // resolution. The loader validated the value against
      // `AUTH_BACKEND_NAMES`, so the cast is safe.
      const configEntry = configBots.find((b) => b.participant === participant);
      const perBotAuth = configEntry?.auth ? (configEntry.auth as AuthBackend) : undefined;
      const effectiveAuthBackend: AuthBackend = perBotAuth ?? authBackend;
      const storageStateFile =
        effectiveAuthBackend === "storage-state"
          ? (opts.storageStateFile ?? storageStatePath(opts.assetsDir, participant))
          : null;
      // SSO-state is only consulted for JWT auth. Bots downgraded to
      // `"none"` get a null here so the launcher never tries to load
      // the file.
      const effectiveSsoStateFile = effectiveAuthBackend === "jwt" ? ssoStateFile : null;
      // Per-bot ttl override from --config wins over the shared TTL.
      let botTtl: Ttl = ttl;
      const perBotTtl = configEntry?.ttl;
      if (perBotTtl) {
        try {
          botTtl = parseDuration(perBotTtl);
        } catch (e) {
          console.warn(
            `bots-app: invalid per-bot ttl "${perBotTtl}" for ${participant}; falling back to shared ttl. (${(e as Error).message})`,
          );
        }
      }
      // Network resolution: per-bot config entry > meeting-level config > CLI flag.
      // The parser already validated every value it returned, so no
      // re-check needed here. The CLI flag was validated up-front
      // before the config was loaded.
      const network = configEntry?.network ?? configNetwork ?? opts.network ?? undefined;
      return {
        botId: generateBotId(),
        meetingURL: meetingUrl,
        participant,
        displayName,
        headless: opts.headless,
        authBackend: effectiveAuthBackend,
        storageStateFile,
        ssoStateFile: effectiveSsoStateFile,
        manifest,
        runDir: opts.assetsDir,
        ttl: botTtl,
        network,
      };
    });

    // Parse --ctl-port. The flag is opt-in: omitting it preserves the
    // pre-Phase 4 byte-for-byte behavior. Accepts a literal integer or
    // "auto" (which becomes port=0 — the kernel picks a free one).
    let ctlPort: number | null = null;
    if (opts.ctlPort !== undefined) {
      if (opts.ctlPort === "auto") {
        ctlPort = 0;
      } else {
        const n = Number.parseInt(opts.ctlPort, 10);
        if (!Number.isFinite(n) || n < 0 || n > 65535) {
          console.error(
            `bots-app: --ctl-port must be a port number (0-65535) or "auto", got "${opts.ctlPort}"`,
          );
          process.exit(2);
        }
        ctlPort = n;
      }
    }

    if (ctlPort === null) {
      await runBotsToCompletion(tasks);
    } else {
      const token = generateToken();
      const tokenFilePath = defaultTokenFilePath(opts.assetsDir);
      await runBotsToCompletion({
        tasks,
        control: {
          port: ctlPort,
          token,
          tokenFilePath,
          runDir: opts.assetsDir,
          onListen: async ({ port, token: t }) => {
            await writeTokenFile(tokenFilePath, {
              port,
              token: t,
              startedAt: new Date().toISOString(),
              pid: process.pid,
            });
            console.log(
              `bots-app: ctl token written to ${tokenFilePath} (mode 0600) — port ${port}`,
            );
          },
        },
      });
    }
    process.exit(0);
  });

interface RunCommandOptions {
  meetingUrl?: string;
  participant?: string;
  users?: string;
  maxUsers: string;
  config?: string;
  displayName?: string;
  headless: boolean;
  ttl: string;
  manifest: string;
  assetsDir: string;
  auth?: string;
  storageStateFile?: string;
  ssoStateFile?: string;
  network?: string;
  ctlPort?: string;
}

function defaultDisplayName(participant: string): string {
  if (participant.includes("@")) {
    return participant.split("@", 1)[0];
  }
  return participant.charAt(0).toUpperCase() + participant.slice(1);
}

program
  .command("login")
  .description(
    "One-time interactive Google OAuth login to capture a Playwright storage state for use against app.videocall.rs. Opens a headed Chrome — the operator logs in normally, then presses Enter in the terminal to save the captured session.",
  )
  .argument(
    "<account>",
    'Account handle that names the captured session file (e.g. "alice" → <assets-dir>/auth/alice.json). The same handle is later passed to `bots-app run --participant <account>` to reuse the session.',
  )
  .option(
    "--start-url <url>",
    "Where to navigate the headed Chrome before the operator logs in.",
    "https://app.videocall.rs/",
  )
  .option(
    "--assets-dir <dir>",
    "Directory under which auth/<account>.json is written.",
    join(repoRoot(), "e2e/bots-app/run"),
  )
  .action(async (account: string, opts: LoginCommandOptions) => {
    const outPath = storageStatePath(opts.assetsDir, account);
    mkdirSync(dirname(outPath), { recursive: true });

    console.log(`bots-app login: opening headed Chrome at ${opts.startUrl}`);
    console.log(`bots-app login: log in normally, then press Enter here to save the session.`);
    console.log(
      `bots-app login: the captured file at ${outPath} contains real session tokens — do NOT commit or share it.`,
    );

    const browser = await chromium.launch({ headless: false });
    const context = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await context.newPage();
    await page.goto(opts.startUrl, { waitUntil: "domcontentloaded" });

    const rl = createInterface({ input: process.stdin, output: process.stdout });
    try {
      await rl.question("Press Enter once logged in to capture the session... ");
    } finally {
      rl.close();
    }

    await context.storageState({ path: outPath });
    await context.close();
    await browser.close();
    console.log(`bots-app login: captured session → ${outPath}`);
    console.log(
      `bots-app login: reuse with \`bots-app run --participant ${account} --meeting-url <url>\`.`,
    );
  });

interface LoginCommandOptions {
  startUrl: string;
  assetsDir: string;
}

program
  .command("sso-login")
  .description(
    "One-time interactive HCL SSO login to capture a Playwright storage state for use against HCL-gated targets (e.g. *.videocall.fnxlabs.com). Opens a headed Chrome — the operator authenticates against HCL SSO normally, then presses Enter in the terminal to save the captured cookies. The result is shared across all participants for the lifetime of the SSO session.",
  )
  .option(
    "--start-url <url>",
    "Where to navigate the headed Chrome before the operator logs in. Default redirects through HCL SSO.",
    "https://app.videocall.fnxlabs.com/",
  )
  .option(
    "--assets-dir <dir>",
    "Directory under which auth/hcl-sso.json is written.",
    join(repoRoot(), "e2e/bots-app/run"),
  )
  .option(
    "--out-file <path>",
    "Override the output file location (default: <assets-dir>/auth/hcl-sso.json).",
  )
  .action(async (opts: SsoLoginCommandOptions) => {
    const outPath = opts.outFile ?? defaultSsoStatePath(opts.assetsDir);
    mkdirSync(dirname(outPath), { recursive: true });

    console.log(`bots-app sso-login: opening headed Chrome at ${opts.startUrl}`);
    console.log(
      `bots-app sso-login: complete the HCL SSO challenge in the browser, then press Enter here to save the session.`,
    );
    console.log(
      `bots-app sso-login: the captured file at ${outPath} contains real SSO cookies — do NOT commit or share it.`,
    );

    // Delegate the actual browser dance to the shared helper. Same code
    // path the dashboard's `POST /api/sso/recapture` endpoint uses;
    // here we just wrap it with a terminal prompt instead of a UI
    // round-trip.
    await captureSsoStateInteractive({
      startUrl: opts.startUrl,
      outPath,
      waitForOperator: async (): Promise<void> => {
        const rl = createInterface({ input: process.stdin, output: process.stdout });
        try {
          await rl.question("Press Enter once SSO auth is complete to capture cookies... ");
        } finally {
          rl.close();
        }
      },
    });
    console.log(`bots-app sso-login: captured SSO session → ${outPath}`);
    console.log(
      `bots-app sso-login: subsequent \`bots-app run\` invocations against HCL-gated hosts will pick this up automatically.`,
    );
  });

interface SsoLoginCommandOptions {
  startUrl: string;
  assetsDir: string;
  outFile?: string;
}

program
  .command("gen")
  .description(
    "Generate a meeting-config YAML for `bots-app run --config <path>`. Deterministic given the same --seed. Default output is stdout; pass --out to write to a file.",
  )
  .requiredOption("--count <N>", "Number of bots to include (must be ≤ manifest participants)")
  .requiredOption(
    "--meeting-url <url>",
    "Meeting URL that gets baked into the generated config's top-level `meeting_url`",
  )
  .option("--seed <S>", "Seed for the RNG (integer; default: random per run)")
  .option(
    "--ttl <duration>",
    "Shared TTL written to the generated config (top-level). Per-bot TTLs are not randomized today.",
  )
  .option(
    "--manifest <path>",
    "Path to bot/conversation/manifest.yaml",
    join(repoRoot(), "bot/conversation/manifest.yaml"),
  )
  .option("--out <path>", "Write the generated YAML to this file instead of stdout")
  .option(
    "--include-observers",
    "Allow the shuffle to pick observer-NN slots (no costume, no audio). Default is to draw only from costumed participants.",
    false,
  )
  .option(
    "--network <profile>",
    `Meeting-level netsim profile to bake into the generated config (one of: ${NETSIM_PRESETS.join(", ")}). Bots inherit it unless they specify their own. Requires the served videocall-client to be built with --features netsim to take effect.`,
  )
  .action((opts: GenCommandOptions) => {
    const count = Number.parseInt(opts.count, 10);
    if (!Number.isFinite(count) || count <= 0) {
      console.error(`bots-app: --count must be a positive integer, got "${opts.count}"`);
      process.exit(2);
    }
    const seed =
      opts.seed !== undefined
        ? Number.parseInt(opts.seed, 10)
        : Math.floor(Math.random() * 2 ** 31);
    if (!Number.isFinite(seed)) {
      console.error(`bots-app: --seed must be an integer, got "${opts.seed}"`);
      process.exit(2);
    }

    if (!existsSync(opts.manifest)) {
      console.error(
        `bots-app: manifest not found at ${opts.manifest} — run \`python3 bot/generate-conversation-edge.py\` first`,
      );
      process.exit(2);
    }
    const { manifest } = loadManifest(opts.manifest);

    let config;
    try {
      config = generateMeetingConfig({
        manifest,
        count,
        seed,
        meetingUrl: opts.meetingUrl,
        ttl: opts.ttl,
        network: opts.network,
        includeObservers: opts.includeObservers,
      });
    } catch (e) {
      console.error(`bots-app: gen failed: ${(e as Error).message}`);
      process.exit(2);
    }
    const yaml = emitMeetingConfigYaml(config);
    if (opts.out) {
      writeFileSync(opts.out, yaml, "utf8");
      console.error(
        `bots-app gen: wrote ${config.bots.length} bot(s) → ${opts.out} (seed=${seed})`,
      );
    } else {
      process.stdout.write(yaml);
    }
  });

interface GenCommandOptions {
  count: string;
  meetingUrl: string;
  seed?: string;
  ttl?: string;
  manifest: string;
  out?: string;
  includeObservers: boolean;
  network?: string;
}

program
  .command("prep-assets")
  .description(
    "One-shot prepare per-participant audio (stitched WAV) and costume video (y4m) for Chrome's fake-device input",
  )
  .option(
    "--manifest <path>",
    "Path to bot/conversation/manifest.yaml",
    join(repoRoot(), "bot/conversation/manifest.yaml"),
  )
  .option(
    "--costume-source <dir>",
    "Directory containing <name>/talking.mp4 per costume",
    join(repoRoot(), "bot/assets/costumes"),
  )
  .option(
    "--output-dir <dir>",
    "Where to write run/audio/<name>.wav and run/costumes/<name>.y4m",
    join(repoRoot(), "e2e/bots-app/run"),
  )
  .option(
    "--participants <list>",
    "Comma-separated participants to prep (default: every named entry in the manifest)",
  )
  .action(async (opts: PrepAssetsOptions) => {
    if (!existsSync(opts.manifest)) {
      console.error(
        `bots-app: manifest not found at ${opts.manifest} — run \`python3 bot/generate-conversation-edge.py\` first`,
      );
      process.exit(2);
    }
    const { manifest, manifestDir } = loadManifest(opts.manifest);
    const audioDir = join(opts.outputDir, "audio");
    const costumesOutDir = join(opts.outputDir, "costumes");

    const requested =
      opts.participants
        ?.split(",")
        .map((s) => s.trim())
        .filter(Boolean) ?? manifest.participants.map((p) => p.name);

    let audioPrepped = 0;
    let costumesPrepped = 0;
    for (const participant of requested) {
      try {
        const audio = prepareParticipantAudio(manifest, manifestDir, participant, audioDir);
        if (audio.lineCount > 0) {
          audioPrepped += 1;
          console.log(
            `[${participant}] audio ${audio.rebuilt ? "stitched" : "cached"} (${audio.lineCount} lines) → ${audio.path}`,
          );
        }
        if (!existsSync(opts.costumeSource)) {
          console.warn(
            `bots-app: costume source ${opts.costumeSource} not found — skipping y4m conversion`,
          );
          continue;
        }
        const costume = prepareParticipantCostume(
          manifest,
          participant,
          opts.costumeSource,
          costumesOutDir,
        );
        if (costume.path !== null) {
          costumesPrepped += 1;
          console.log(
            `[${participant}] costume ${costume.rebuilt ? "converted" : "cached"} (${costume.costumeName}) → ${costume.path}`,
          );
        }
      } catch (e) {
        console.error(`[${participant}] prep failed:`, (e as Error).message);
      }
    }
    console.log(`prep-assets done — ${audioPrepped} audio file(s), ${costumesPrepped} costume(s)`);
  });

interface PrepAssetsOptions {
  manifest: string;
  costumeSource: string;
  outputDir: string;
  participants?: string;
}

/**
 * Resolve the repo root (one level above `e2e/`) from this file's location.
 * Lets the `prep-assets` defaults work no matter the cwd.
 */
function repoRoot(): string {
  // import.meta.url is bots-app/src/cli.ts at runtime via tsx; the repo
  // root is three directories up: src → bots-app → e2e → repo.
  const here = new URL(".", import.meta.url).pathname;
  return resolve(here, "..", "..", "..");
}

// Phase 4: register the `ctl` family. Token discovery defaults to
// scanning the same `e2e/bots-app/run/` directory the orchestrator
// writes to.
registerCtlCommands(program, join(repoRoot(), "e2e/bots-app/run"));

// Phase 5: the dashboard subcommand. Spins up a small Node HTTP
// sidecar that proxies the browser-facing UI to a phase-4
// orchestrator's ctl API, attaching the bearer token server-side so
// the token never reaches the browser. By default the orchestrator
// + ctl server are spawned IN-PROCESS alongside the dashboard, so a
// single `bots-app dashboard` invocation is self-contained — no
// separate `bots-app run` terminal needed.
//
// For headless / scripted / "attach to an already-running daemon"
// flows, pass `--ctl-port` + `--ctl-token` (or `--ctl-token-file`)
// and the dashboard skips the in-process spawn.
program
  .command("dashboard")
  .description(
    "Phase 5: serve a browser-based UX dashboard for launching and managing bots. " +
      "By default the dashboard is self-contained: the orchestrator + ctl server " +
      "are spawned in-process and accept launch requests over the dashboard's " +
      "form. Pass --ctl-port/--ctl-token (or --ctl-token-file) to attach to an " +
      "externally-managed `bots-app run --ctl-port auto` daemon instead.",
  )
  .option("--port <port>", "Port to bind the dashboard HTTP server to (default 5174)", "5174")
  .option(
    "--ctl-token-file <path>",
    "Attach to an existing daemon via its ctl-<pid>.token file. When unset (and no --ctl-port/--ctl-token), the dashboard spawns the orchestrator in-process.",
  )
  .option("--ctl-port <port>", "Attach to an existing daemon on this port (use with --ctl-token).")
  .option("--ctl-token <token>", "Bearer token for the attached daemon (use with --ctl-port).")
  .option(
    "--run-dir <dir>",
    "Override the directory scanned for ctl-*.token files (and the asset directories).",
    join(repoRoot(), "e2e/bots-app/run"),
  )
  .option("--no-open", "Skip auto-opening the dashboard URL in the operator's default browser")
  .option(
    "--dist-dir <dir>",
    "Override the location of the dashboard's built `dist/` directory.",
    join(repoRoot(), "e2e/bots-app/dashboard/dist"),
  )
  .action(async (opts: DashboardCommandOptions) => {
    const { startDashboardServer, resolveCtlConfig, spawnViteDev } = await import("./dashboard");
    const port = Number.parseInt(opts.port, 10);
    if (!Number.isFinite(port) || port < 0 || port > 65535) {
      console.error(
        `bots-app dashboard: --port must be a port number (0-65535), got "${opts.port}"`,
      );
      process.exit(2);
    }
    let ctlPortNum: number | undefined;
    if (opts.ctlPort !== undefined) {
      ctlPortNum = Number.parseInt(opts.ctlPort, 10);
      if (!Number.isFinite(ctlPortNum) || ctlPortNum <= 0 || ctlPortNum > 65535) {
        console.error(
          `bots-app dashboard: --ctl-port must be a port number (1-65535), got "${opts.ctlPort}"`,
        );
        process.exit(2);
      }
    }

    // Decide between attach-mode and self-hosted mode. Attach-mode is
    // selected when any of the three "attach" flags is set; otherwise
    // we self-host the orchestrator in-process.
    const attachRequested =
      opts.ctlPort !== undefined || opts.ctlToken !== undefined || opts.ctlTokenFile !== undefined;

    let ctl: { port: number; token: string };
    let daemonMode: "self-hosted" | "attached";
    let orchestratorTask: Promise<void> | null = null;

    if (attachRequested) {
      try {
        const resolved = await resolveCtlConfig({
          port: ctlPortNum,
          token: opts.ctlToken,
          tokenFile: opts.ctlTokenFile,
          runDir: opts.runDir,
        });
        ctl = { port: resolved.port, token: resolved.token };
        daemonMode = "attached";
        console.log(
          `bots-app dashboard: attached to ctl daemon at 127.0.0.1:${resolved.port} (pid ${resolved.pid || "?"}, started ${resolved.startedAt})`,
        );
      } catch (e) {
        console.error(`bots-app dashboard: ${(e as Error).message}`);
        process.exit(2);
        return;
      }
    } else {
      // Self-hosted mode: spawn the orchestrator + ctl server in this
      // same Node process. Zero initial bots; the dashboard's launch
      // form adds them on demand. The orchestrator's own
      // SIGINT/SIGTERM handlers run the clean-leave path on every bot
      // before resolving — our cleanup hook below awaits that.
      const token = generateToken();
      const tokenFilePath = defaultTokenFilePath(opts.runDir);
      const ctlReady = new Promise<{ port: number; token: string }>((resolveReady) => {
        orchestratorTask = runBotsToCompletion({
          tasks: [],
          control: {
            port: 0,
            token,
            tokenFilePath,
            runDir: opts.runDir,
            onListen: async ({ port: resolvedPort, token: t }) => {
              await writeTokenFile(tokenFilePath, {
                port: resolvedPort,
                token: t,
                startedAt: new Date().toISOString(),
                pid: process.pid,
              });
              console.log(
                `bots-app dashboard: self-hosted ctl daemon listening on 127.0.0.1:${resolvedPort} (token at ${tokenFilePath}, mode 0600)`,
              );
              resolveReady({ port: resolvedPort, token: t });
            },
          },
        });
      });
      ctl = await ctlReady;
      daemonMode = "self-hosted";
    }

    const dashboardDir = join(repoRoot(), "e2e/bots-app/dashboard");
    const distDir = opts.distDir;
    const builtMode = existsSync(distDir) && existsSync(join(distDir, "index.html"));
    if (builtMode) {
      console.log(`bots-app dashboard: serving built UI from ${distDir}`);
    } else {
      console.log(
        `bots-app dashboard: no built UI at ${distDir} — falling back to Vite dev mode (spawning \`npm run dev\` in ${dashboardDir}). Run \`npm run build\` there to produce a static bundle.`,
      );
    }

    const handle = await startDashboardServer({
      port,
      ctl,
      distDir: builtMode ? distDir : undefined,
      assetsDir: opts.runDir,
      daemonMode,
      onListen: ({ port: actual }) => {
        const url = `http://127.0.0.1:${actual}/`;
        if (builtMode) {
          console.log(`bots-app dashboard: listening on ${url}`);
        } else {
          console.log(`bots-app dashboard: backend on ${url} (Vite dev UI will follow on :5173)`);
        }
        if (opts.open !== false) {
          // In dev mode, prefer to open the Vite URL once it's up.
          // We don't have a reliable readiness signal short of probing
          // the Vite port, so we log a tip instead.
          openInBrowser(builtMode ? url : "http://127.0.0.1:5173/");
        }
      },
    });

    if (!builtMode) {
      // Spawn Vite in dev mode AFTER the Node sidecar is listening
      // so Vite's proxy targets a port that already accepts connections.
      spawnViteDev({ dashboardDir, backendPort: handle.port });
    }
    // Coordinated shutdown: SIGINT/SIGTERM tear down the dashboard
    // sidecar; the orchestrator's own SIGINT/SIGTERM handlers (in
    // self-hosted mode) take care of its bots. We just await its
    // resolution so the process doesn't exit before the browsers
    // have actually quit.
    let cleaning = false;
    const cleanup = async (): Promise<void> => {
      if (cleaning) return;
      cleaning = true;
      console.log("bots-app dashboard: shutting down");
      await handle.close().catch(() => {});
      if (orchestratorTask) {
        await orchestratorTask.catch(() => {});
      }
      process.exit(0);
    };
    process.on("SIGINT", () => void cleanup());
    process.on("SIGTERM", () => void cleanup());
  });

interface DashboardCommandOptions {
  port: string;
  ctlPort?: string;
  ctlToken?: string;
  ctlTokenFile?: string;
  runDir: string;
  open?: boolean;
  distDir: string;
}

/**
 * Best-effort open `url` in the operator's default browser. Failure is
 * non-fatal — the operator can copy the URL out of the listen-line log.
 */
function openInBrowser(url: string): void {
  let cmd: string;
  let args: string[];
  if (process.platform === "darwin") {
    cmd = "open";
    args = [url];
  } else if (process.platform === "win32") {
    cmd = "cmd";
    args = ["/c", "start", "", url];
  } else {
    cmd = "xdg-open";
    args = [url];
  }
  try {
    const child = spawnDetached(cmd, args);
    child.unref();
  } catch (e) {
    console.warn(`bots-app dashboard: could not auto-open ${url}: ${(e as Error).message}`);
  }
}

function spawnDetached(cmd: string, args: string[]): import("node:child_process").ChildProcess {
  // Lazy import to avoid a top-level require for a cold path.
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const { spawn } = require("node:child_process") as typeof import("node:child_process");
  return spawn(cmd, args, { detached: true, stdio: "ignore" });
}

program.parseAsync(process.argv).catch((err: unknown) => {
  console.error("bots-app fatal:", err);
  process.exit(1);
});
