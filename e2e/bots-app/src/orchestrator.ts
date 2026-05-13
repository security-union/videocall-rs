import { launchBot, type BotRunOptions } from "./bot";
import { formatDuration, type Ttl, waitForTtl } from "./ttl";

export interface BotTask extends BotRunOptions {
  ttl: Ttl;
}

/**
 * Spin up N bots concurrently, wait for each to finish its TTL or for a
 * shared SIGINT/SIGTERM, then return once all have cleanly left the
 * meeting and torn down their browsers.
 *
 * Multi-bot here means **N in-process Playwright `Browser` instances**
 * inside one Node process — cheaper than N subprocesses, but with a
 * single point of failure: if the Node parent dies hard, all bots are
 * orphaned. SIGINT/SIGTERM handlers below run the clean-leave path on
 * every bot before exit; an uncaught exception in one bot's task is
 * caught + logged so it does not propagate to the others.
 *
 * Per-bot logs are tagged with the participant handle by `bot.ts` and
 * `meeting-join.ts`, so the merged stdout is readable when several
 * bots are running side-by-side.
 */
export async function runBotsToCompletion(tasks: readonly BotTask[]): Promise<void> {
  if (tasks.length === 0) {
    console.warn("[orchestrator] no tasks supplied; nothing to do");
    return;
  }

  let resolveShutdown!: () => void;
  const shutdownRequested = new Promise<void>((resolve) => {
    resolveShutdown = resolve;
  });

  let shuttingDown = false;
  const requestShutdown = (signal: string): void => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log(`[orchestrator] received ${signal}; signaling ${tasks.length} bot(s) to leave`);
    resolveShutdown();
  };
  process.on("SIGINT", () => requestShutdown("SIGINT"));
  process.on("SIGTERM", () => requestShutdown("SIGTERM"));

  console.log(`[orchestrator] launching ${tasks.length} bot(s)`);

  const results = await Promise.allSettled(
    tasks.map((task) => runSingleBotTask(task, shutdownRequested)),
  );

  const failed = results.filter((r) => r.status === "rejected").length;
  if (failed > 0) {
    console.warn(`[orchestrator] ${failed}/${tasks.length} bot(s) ended with an error`);
  }
  console.log(`[orchestrator] all ${tasks.length} bot(s) finished`);
}

async function runSingleBotTask(task: BotTask, shutdownRequested: Promise<void>): Promise<void> {
  const { ttl, ...launchOpts } = task;
  let bot;
  try {
    bot = await launchBot(launchOpts);
  } catch (err) {
    console.error(`[${task.participant}] launch failed:`, (err as Error).message);
    throw err;
  }
  console.log(`[${task.participant}] joined; ttl=${formatDuration(ttl)}`);

  const ttlTimer = waitForTtl(ttl);
  let reason = "ttl expired";
  await Promise.race([
    ttlTimer.done,
    shutdownRequested.then(() => {
      reason = "shutdown signal";
    }),
  ]);
  ttlTimer.cancel();

  console.log(`[${task.participant}] shutting down (${reason})`);
  try {
    await bot.leaveMeeting();
  } catch (e) {
    console.error(`[${task.participant}] leaveMeeting failed:`, (e as Error).message);
  }
  await bot.shutdown();
}
