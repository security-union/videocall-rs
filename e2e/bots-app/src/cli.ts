import { Command } from "commander";

import { launchBot } from "./bot";

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
  .action(async (opts: RunCommandOptions) => {
    const displayName = opts.displayName ?? defaultDisplayName(opts.participant);

    const bot = await launchBot({
      meetingURL: opts.meetingUrl,
      participant: opts.participant,
      displayName,
      headless: opts.headless,
    });
    console.log(`[${opts.participant}] joined; holding session until SIGINT/SIGTERM`);

    let shuttingDown = false;
    const onSignal = async (signal: NodeJS.Signals): Promise<void> => {
      if (shuttingDown) return;
      shuttingDown = true;
      console.log(`[${opts.participant}] received ${signal}, shutting down`);
      await bot.shutdown();
      process.exit(0);
    };
    process.on("SIGINT", () => void onSignal("SIGINT"));
    process.on("SIGTERM", () => void onSignal("SIGTERM"));

    // Wait indefinitely; PR-1b adds the TTL scheduler that races against this.
    await new Promise<void>(() => {});
  });

interface RunCommandOptions {
  meetingUrl: string;
  participant: string;
  displayName?: string;
  headless: boolean;
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
