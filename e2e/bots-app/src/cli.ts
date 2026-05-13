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
import { prepareParticipantCostume } from "./costumes";
import { firstNParticipantNames, loadManifest, type Manifest } from "./manifest";
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
  .requiredOption(
    "--meeting-url <url>",
    "Full meeting URL (e.g. https://app.videocall.fnxlabs.com/meeting/TonyBots)",
  )
  .option(
    "--participant <name>",
    'Single-bot mode: participant handle (e.g. "alice") or full email. Mutually exclusive with --users.',
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
  .action(async (opts: RunCommandOptions) => {
    let ttl: Ttl;
    try {
      ttl = parseDuration(opts.ttl);
    } catch (e) {
      console.error(`bots-app: ${(e as Error).message}`);
      process.exit(2);
    }

    if (opts.participant && opts.users) {
      console.error("bots-app: --participant and --users are mutually exclusive");
      process.exit(2);
    }
    if (!opts.participant && !opts.users) {
      console.error("bots-app: one of --participant or --users is required");
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

    // Resolve the participant list — single-bot via --participant, or
    // multi-bot via --users picking from manifest order.
    let participants: string[];
    if (opts.users) {
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
    const hostname = new URL(opts.meetingUrl).hostname;
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
      return {
        meetingURL: opts.meetingUrl,
        participant,
        displayName,
        headless: opts.headless,
        authBackend,
        storageStateFile,
        ssoStateFile,
        manifest,
        runDir: opts.assetsDir,
        ttl,
      };
    });

    await runBotsToCompletion(tasks);
    process.exit(0);
  });

interface RunCommandOptions {
  meetingUrl: string;
  participant?: string;
  users?: string;
  maxUsers: string;
  displayName?: string;
  headless: boolean;
  ttl: string;
  manifest: string;
  assetsDir: string;
  auth?: string;
  storageStateFile?: string;
  ssoStateFile?: string;
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
