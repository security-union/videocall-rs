import { Command } from "commander";

import { launchBot } from "./bot";
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

program.parseAsync(process.argv).catch((err: unknown) => {
  console.error("bots-app fatal:", err);
  process.exit(1);
});
