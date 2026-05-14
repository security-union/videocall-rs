import { type BotExitReason, launchBot, type BotRunOptions } from "./bot";
import { MeetingNavigatedAwayError } from "./meeting-join";
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
 *
 * Exit accounting: only `launch-error` reasons count toward the
 * "ended with an error" tally. A user-initiated hang-up (clicking the
 * in-browser HangUp button) is a graceful exit and is reported as
 * such — not as a failure.
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

  const results = await Promise.all(tasks.map((task) => runSingleBotTask(task, shutdownRequested)));

  const failed = results.filter((r) => r.kind === "launch-error").length;
  if (failed > 0) {
    console.warn(`[orchestrator] ${failed}/${tasks.length} bot(s) ended with an error`);
  }
  console.log(`[orchestrator] all ${tasks.length} bot(s) finished`);
}

async function runSingleBotTask(
  task: BotTask,
  shutdownRequested: Promise<void>,
): Promise<BotExitReason> {
  const { ttl, ...launchOpts } = task;
  let bot;
  try {
    bot = await launchBot(launchOpts);
  } catch (err) {
    if (err instanceof MeetingNavigatedAwayError) {
      // The user dismissed the bot via the browser hang-up button
      // before the join even completed. The bot already tore its
      // own context + browser down inside `launchBot`. Treat this as
      // a graceful exit — do NOT log a "launch failed" line and do
      // NOT count it toward the failure tally.
      console.log(`[${task.participant}] exited cleanly: user dismissed via browser hang-up`);
      return { kind: "user-hangup" };
    }
    console.error(`[${task.participant}] launch failed:`, (err as Error).message);
    return { kind: "launch-error", cause: err };
  }
  console.log(`[${task.participant}] joined; ttl=${formatDuration(ttl)}`);

  const ttlTimer = waitForTtl(ttl);
  // Track which signal completed the bot's lifetime — TTL, an
  // out-of-band SIGINT/SIGTERM, or a manual in-browser hang-up.
  // Explicit `as BotExitReason` cast: without it TS narrows to the
  // initializer's singleton literal and the post-race
  // `kind !== "user-hangup"` check becomes "always true" at the type
  // level.
  let exitReason: BotExitReason = { kind: "ttl-expired" } as BotExitReason;
  await Promise.race([
    ttlTimer.done.then(() => {
      exitReason = { kind: "ttl-expired" };
    }),
    shutdownRequested.then(() => {
      exitReason = { kind: "shutdown-signal" };
    }),
    bot.userHangupDetected.then(() => {
      exitReason = { kind: "user-hangup" };
    }),
  ]);
  ttlTimer.cancel();

  console.log(`[${task.participant}] shutting down (${exitReason.kind})`);
  // Skip `leaveMeeting` when the user already did it from the browser
  // — clicking the (now non-existent) hang-up button is a no-op
  // and just delays shutdown.
  if (exitReason.kind !== "user-hangup") {
    try {
      await bot.leaveMeeting();
    } catch (e) {
      console.error(`[${task.participant}] leaveMeeting failed:`, (e as Error).message);
    }
  }
  await bot.shutdown();
  return exitReason;
}
