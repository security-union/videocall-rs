import { existsSync } from "node:fs";
import { join, resolve } from "node:path";

import { Command } from "commander";

import { launchBot } from "./bot";
import { prepareParticipantCostume } from "./costumes";
import { loadManifest } from "./manifest";
import { prepareParticipantAudio } from "./stitcher";
import { formatDuration, parseDuration, Ttl, waitForTtl } from "./ttl";

const program = new Command();

program
  .name("bots-app")
  .description("Browser-driven bot CLI for videocall meetings")
  .version("0.1.0");

program
  .command("run")
  .description("Launch one browser bot that joins a meeting and holds the session")
  .requiredOption(
    "--meeting-url <url>",
    "Full meeting URL (e.g. https://app.videocall.fnxlabs.com/meeting/TonyBots)",
  )
  .requiredOption("--participant <name>", 'Participant handle (e.g. "alice") or full email')
  .option("--display-name <name>", "Display name shown in the meeting", undefined)
  .option("--headless", "Run Chrome headless (default: headed)", false)
  .option(
    "--ttl <duration>",
    'Bot lifespan — "<int>s|m|h" or "infinite". On expiry the bot leaves the meeting and exits.',
    "5m",
  )
  .action(async (opts: RunCommandOptions) => {
    const displayName = opts.displayName ?? defaultDisplayName(opts.participant);
    let ttl: Ttl;
    try {
      ttl = parseDuration(opts.ttl);
    } catch (e) {
      console.error(`bots-app: ${(e as Error).message}`);
      process.exit(2);
    }

    const bot = await launchBot({
      meetingURL: opts.meetingUrl,
      participant: opts.participant,
      displayName,
      headless: opts.headless,
    });
    console.log(`[${opts.participant}] joined; ttl=${formatDuration(ttl)}`);

    const ttlTimer = waitForTtl(ttl);
    let shuttingDown = false;
    const cleanLeaveAndExit = async (reason: string): Promise<void> => {
      if (shuttingDown) return;
      shuttingDown = true;
      console.log(`[${opts.participant}] shutting down (${reason})`);
      ttlTimer.cancel();
      await bot.leaveMeeting();
      await bot.shutdown();
      process.exit(0);
    };
    process.on("SIGINT", () => void cleanLeaveAndExit("SIGINT"));
    process.on("SIGTERM", () => void cleanLeaveAndExit("SIGTERM"));

    await ttlTimer.done;
    await cleanLeaveAndExit("ttl expired");
  });

interface RunCommandOptions {
  meetingUrl: string;
  participant: string;
  displayName?: string;
  headless: boolean;
  ttl: string;
}

function defaultDisplayName(participant: string): string {
  if (participant.includes("@")) {
    return participant.split("@", 1)[0];
  }
  return participant.charAt(0).toUpperCase() + participant.slice(1);
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
