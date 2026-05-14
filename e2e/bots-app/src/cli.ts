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
import { writeFileSync } from "node:fs";

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
    'Auth backend override: "jwt" (cookie injection; for local + HCL + previews) or "storage-state" (replay a captured Google OAuth session from `bots-app login`; for app.videocall.rs). When omitted, picks automatically by hostname.',
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
    let configBots: { participant: string; ttl?: string; network?: string }[] = [];
    let configTtl: string | null = null;
    let configNetwork: string | null = null;
    if (opts.config) {
      try {
        const cfg = loadMeetingConfig(opts.config);
        configMeetingUrl = cfg.meetingUrl;
        configBots = cfg.bots;
        configTtl = cfg.ttl ?? null;
        configNetwork = cfg.network ?? null;
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
      if (opts.auth !== "jwt" && opts.auth !== "storage-state") {
        console.error(`bots-app: --auth must be "jwt" or "storage-state", got "${opts.auth}"`);
        process.exit(2);
      }
      authOverride = opts.auth;
    }
    const hostname = new URL(meetingUrl).hostname;
    const authBackend = chooseAuthBackend(hostname, authOverride);
    const ssoStateFile =
      authBackend === "jwt" ? (opts.ssoStateFile ?? defaultSsoStatePath(opts.assetsDir)) : null;

    const tasks: BotTask[] = participants.map((participant) => {
      const displayName =
        opts.displayName && participants.length === 1
          ? opts.displayName
          : defaultDisplayName(participant);
      const storageStateFile =
        authBackend === "storage-state"
          ? (opts.storageStateFile ?? storageStatePath(opts.assetsDir, participant))
          : null;
      // Per-bot ttl override from --config wins over the shared TTL.
      let botTtl: Ttl = ttl;
      const configEntry = configBots.find((b) => b.participant === participant);
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
        meetingURL: meetingUrl,
        participant,
        displayName,
        headless: opts.headless,
        authBackend,
        storageStateFile,
        ssoStateFile,
        manifest,
        runDir: opts.assetsDir,
        ttl: botTtl,
        network,
      };
    });

    await runBotsToCompletion(tasks);
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

    const browser = await chromium.launch({ headless: false });
    const context = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await context.newPage();
    await page.goto(opts.startUrl, { waitUntil: "domcontentloaded" });

    const rl = createInterface({ input: process.stdin, output: process.stdout });
    try {
      await rl.question("Press Enter once SSO auth is complete to capture cookies... ");
    } finally {
      rl.close();
    }

    await context.storageState({ path: outPath });
    await context.close();
    await browser.close();
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

program.parseAsync(process.argv).catch((err: unknown) => {
  console.error("bots-app fatal:", err);
  process.exit(1);
});
