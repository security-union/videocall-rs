import { type BotExitReason, launchBot, type BotRunOptions } from "./bot";
import {
  type BotRegistryEntry,
  generateBotId,
  newRegistryEntry,
  shortBotId,
} from "./control/registry";
import {
  startControlServer,
  type ControlServerHandle,
  type LaunchSpec,
  type OrchestratorControlSurface,
} from "./control/server";
import { JoinRejectedError, MeetingNavigatedAwayError, WaitingRoomError } from "./meeting-join";
import { formatDuration, parseDuration, type Ttl } from "./ttl";

export interface BotTask extends BotRunOptions {
  /**
   * Stable per-bot identifier — a UUID v4 generated at task-build time
   * by the CLI (or by the control server when handling a `duplicate`).
   * Surfaces in log prefixes and in every control-API response so
   * humans can correlate stdout with `ctl list` rows.
   */
  botId: string;
  ttl: Ttl;
}

/**
 * Options accepted by {@link runBotsToCompletion}. The legacy single-arg
 * form (`runBotsToCompletion(tasks)`) is still supported — it's
 * equivalent to passing `{ tasks }` here. The `control` block opts in
 * to the Phase 4 control server: when set, the orchestrator binds an
 * HTTP server on `control.port` (0 ⇒ pick free port) and writes
 * `control.tokenFilePath` so the `ctl` client can find it.
 */
export interface RunOptions {
  tasks: readonly BotTask[];
  control?: {
    port: number;
    token: string;
    tokenFilePath: string;
    /**
     * Run directory shared with the CLI's `--assets-dir`. Forwarded
     * to the control server so the `/profiles*` endpoints can read
     * and write `<runDir>/profiles/`. When omitted those endpoints
     * reply with 503 (the control surface still works for everything
     * else).
     */
    runDir?: string;
    /**
     * Optional hook fired once the control server is listening — used
     * by the CLI to write the token file and log the listen line.
     * Receiving the resolved port is useful when `port === 0` (auto).
     */
    onListen?: (info: { port: number; token: string }) => Promise<void>;
  };
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
 *
 * ### Phase 4: dynamic add + control server
 *
 * The classic `Promise.all(tasks.map(...))` pattern can't accept a
 * mid-flight `duplicate`, so we replaced it with an iterate-and-await
 * loop over a `Map<botId, Promise<BotExitReason>>`. New tasks added
 * by the control server land in the same map; the loop keeps spinning
 * as long as there's at least one in-flight bot.
 */
export async function runBotsToCompletion(arg: readonly BotTask[] | RunOptions): Promise<void> {
  const opts: RunOptions = Array.isArray(arg) ? { tasks: arg } : (arg as RunOptions);
  const initialTasks = opts.tasks;

  if (initialTasks.length === 0 && opts.control === undefined) {
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
    console.log(`[orchestrator] received ${signal}; signaling ${registry.size} bot(s) to leave`);
    resolveShutdown();
  };
  const sigintHandler = (): void => requestShutdown("SIGINT");
  const sigtermHandler = (): void => requestShutdown("SIGTERM");
  process.on("SIGINT", sigintHandler);
  process.on("SIGTERM", sigtermHandler);

  // The registry is the source of truth for everything the control API
  // shows; the in-flight promise map is the source of truth for "what
  // is still running." They are kept in lockstep: every task added to
  // `inFlight` has a matching entry in `registry`, and an entry
  // transitions to `done`/`failed` only when its promise resolves.
  const registry: Map<string, BotRegistryEntry> = new Map();
  const inFlight: Map<string, Promise<BotExitReason>> = new Map();
  // When the wait loop is idle (no in-flight bots, control server
  // attached), it parks itself on a promise that resolves the moment
  // `registerTask` wakes it. Each call to `registerTask` drains the
  // queue — the loop re-enters its dispatch race with the new bot
  // included. Without this, dashboard-launched bots would sit in the
  // registry but never have their exit promise observed.
  const inFlightWaiters: Array<() => void> = [];
  // Per-bot mutable scheduling state. Kept outside the registry entry
  // because none of it is safe to serialize over the control API.
  const ttlTimers: Map<string, { cancel: () => void; rearm: (ttl: Ttl) => void }> = new Map();
  // Per-bot "force shut down now" promises. Resolved by the control
  // server's `triggerLeave` / `forceKill` / `changeNetwork` to break
  // the wait loop in `runSingleBotTask` before the natural TTL fires.
  // Returns a leaf reason ("ctl-leave" / "ctl-kill" / "ctl-rejoin") so
  // the orchestrator can branch on it.
  const ctlSignals: Map<string, { trigger: (reason: CtlReason) => void }> = new Map();

  if (initialTasks.length > 0) {
    console.log(`[orchestrator] launching ${initialTasks.length} bot(s)`);
  } else {
    console.log("[orchestrator] starting with 0 bots; waiting for dashboard / ctl to add some");
  }

  // Build the control surface up front so we can hand it to the
  // server before any bot finishes — the server can be queried the
  // instant it's listening.
  const surface: OrchestratorControlSurface = {
    getRegistry: () => registry,
    triggerLeave: async (botId) => {
      const sig = ctlSignals.get(botId);
      if (sig === undefined) throw new Error(`bot ${botId} not in flight`);
      sig.trigger("ctl-leave");
    },
    forceKill: async (botId) => {
      const sig = ctlSignals.get(botId);
      if (sig === undefined) throw new Error(`bot ${botId} not in flight`);
      sig.trigger("ctl-kill");
    },
    applyTtl: (botId, newTtl) => {
      const entry = registry.get(botId);
      if (entry === undefined) throw new Error(`bot ${botId} not in registry`);
      entry.ttl = newTtl;
      entry.ttlDeadline = newTtl === "infinite" ? null : Date.now() + newTtl;
      const timer = ttlTimers.get(botId);
      if (timer !== undefined) timer.rearm(newTtl);
      console.log(
        `[${entry.task.participant}@${shortBotId(botId)}] ttl rearmed → ${formatDuration(newTtl)}`,
      );
    },
    changeNetwork: async (botId, network) => {
      const entry = registry.get(botId);
      if (entry === undefined) throw new Error(`bot ${botId} not in registry`);
      // Stash the new network on the task so the rejoin path below
      // sees it. The actual reconnect is performed by
      // `runSingleBotTask` after the ctl signal fires.
      entry.task = { ...entry.task, network };
      entry.network = network;
      const sig = ctlSignals.get(botId);
      if (sig === undefined) throw new Error(`bot ${botId} not in flight`);
      sig.trigger("ctl-rejoin");
    },
    setMicMuted: async (botId, micMuted) => {
      const entry = registry.get(botId);
      if (entry === undefined) throw new Error(`bot ${botId} not in registry`);
      if (entry.handle === null) throw new Error(`bot ${botId} is not yet in-meeting`);
      await toggleMicrophone(entry, micMuted);
    },
    setCameraOff: async (botId, cameraOff) => {
      const entry = registry.get(botId);
      if (entry === undefined) throw new Error(`bot ${botId} not in registry`);
      if (entry.handle === null) throw new Error(`bot ${botId} is not yet in-meeting`);
      await toggleCamera(entry, cameraOff);
    },
    setScreenShare: async (botId, share) => {
      const entry = registry.get(botId);
      if (entry === undefined) throw new Error(`bot ${botId} not in registry`);
      if (entry.handle === null) throw new Error(`bot ${botId} is not yet in-meeting`);
      await toggleScreenShare(entry, share);
    },
    launchOne: async (spec: LaunchSpec) => {
      const newTask: BotTask = {
        botId: generateBotId(),
        meetingURL: spec.meetingURL,
        participant: spec.participant,
        displayName: spec.displayName ?? defaultDisplayName(spec.participant),
        headless: spec.headless,
        authBackend: spec.authBackend,
        storageStateFile:
          spec.authBackend === "storage-state" ? (spec.storageStateFile ?? null) : null,
        // Use the same SSO-state convention as the CLI: only consulted
        // when authBackend === "jwt", and the path resolution mirrors
        // the legacy `--sso-state-file` default. We can't reach the
        // CLI's `assetsDir` from here without leaking state into the
        // surface, so we accept SSO files only at the path the CLI's
        // default scan covers (callers can pre-stage the file).
        ssoStateFile: null,
        manifest: null,
        runDir: null,
        ttl: spec.ttl,
        network: spec.network === "none" ? null : spec.network,
      };
      registerTask(newTask);
      console.log(
        `[orchestrator] dashboard-launch → ${newTask.participant}@${shortBotId(newTask.botId)}`,
      );
      return newTask.botId;
    },
    duplicateBot: async (sourceBotId, overrides) => {
      const src = registry.get(sourceBotId);
      if (src === undefined) throw new Error(`bot ${sourceBotId} not in registry`);
      const newTask: BotTask = {
        ...src.task,
        botId: generateBotId(),
        participant: overrides.participant ?? src.task.participant,
        ttl: overrides.ttl ?? src.task.ttl,
        network: overrides.network ?? src.task.network ?? null,
        displayName: overrides.participant
          ? defaultDisplayName(overrides.participant)
          : src.task.displayName,
      };
      registerTask(newTask);
      console.log(
        `[orchestrator] duplicate of ${shortBotId(sourceBotId)} → ${newTask.participant}@${shortBotId(newTask.botId)}`,
      );
      return newTask.botId;
    },
  };

  let controlHandle: ControlServerHandle | null = null;
  if (opts.control) {
    controlHandle = await startControlServer({
      port: opts.control.port,
      token: opts.control.token,
      surface,
      runDir: opts.control.runDir,
    });
    console.log(
      `[orchestrator] control server listening on http://127.0.0.1:${controlHandle.port}`,
    );
    if (opts.control.onListen) {
      await opts.control.onListen({ port: controlHandle.port, token: opts.control.token });
    }
  }

  function registerTask(task: BotTask): void {
    const entry = newRegistryEntry(task);
    registry.set(task.botId, entry);
    inFlight.set(
      task.botId,
      runSingleBotTask(task, entry, {
        shutdownRequested,
        registerTtlTimer: (botId, ctl) => ttlTimers.set(botId, ctl),
        registerCtlSignal: (botId, sig) => ctlSignals.set(botId, sig),
        clearMaps: (botId) => {
          ttlTimers.delete(botId);
          ctlSignals.delete(botId);
        },
      }),
    );
    // Wake the wait loop if it was parked waiting for new work
    // (dashboard mode with zero initial tasks). Draining the queue
    // here is intentional — every parked waiter resolves with the same
    // "size changed" signal.
    while (inFlightWaiters.length > 0) {
      const w = inFlightWaiters.shift();
      if (w) w();
    }
  }

  for (const task of initialTasks) {
    registerTask(task);
  }

  // Main wait loop: keep racing the in-flight map until it's empty
  // AND no control server is attached (or the shutdown signal has
  // fired). When the dashboard launches the orchestrator self-hosted
  // with zero initial bots, the loop body never enters until the
  // control server's `launchOne` populates `inFlight`; the
  // `shutdownRequested` race keeps the process awake in the
  // meantime.
  //
  // `Promise.race` returns the first-to-resolve, but we need to know
  // *which* promise it was — so we attach the botId to the value
  // before racing.
  while (true) {
    if (inFlight.size === 0) {
      // No work currently in flight. If we have a control server
      // attached, idle until either a new bot is enqueued (which sets
      // `inFlightChanged`) or a shutdown signal is delivered. When
      // there's no control server, exit immediately — the legacy
      // `bots-app run --participant alice` flow finishes the moment
      // its only bot finishes.
      if (controlHandle === null) break;
      if (shuttingDown) break;
      await Promise.race([
        shutdownRequested,
        new Promise<void>((res) => {
          inFlightWaiters.push(res);
        }),
      ]);
      continue;
    }
    const winner = await Promise.race(
      Array.from(inFlight, ([id, promise]) =>
        promise.then(
          (reason) => ({ id, reason, kind: "ok" as const }),
          (err) => ({ id, err, kind: "err" as const }),
        ),
      ),
    );
    inFlight.delete(winner.id);
    if (winner.kind === "err") {
      const entry = registry.get(winner.id);
      const label = entry ? `${entry.task.participant}@${shortBotId(winner.id)}` : winner.id;
      console.error(
        `[orchestrator] bot ${label} threw:`,
        (winner.err as Error)?.message ?? winner.err,
      );
    }
  }

  const failed = countFailed(registry);
  if (failed > 0) {
    console.warn(`[orchestrator] ${failed}/${registry.size} bot(s) ended with an error`);
  }
  console.log(`[orchestrator] all bot(s) finished`);

  if (controlHandle) {
    await controlHandle.close().catch((e: unknown) => {
      console.error(`[orchestrator] control server close failed:`, (e as Error).message);
    });
  }
  process.off("SIGINT", sigintHandler);
  process.off("SIGTERM", sigtermHandler);
}

type CtlReason = "ctl-leave" | "ctl-kill" | "ctl-rejoin";

interface SingleBotDeps {
  shutdownRequested: Promise<void>;
  registerTtlTimer: (botId: string, ctl: { cancel: () => void; rearm: (ttl: Ttl) => void }) => void;
  registerCtlSignal: (botId: string, sig: { trigger: (reason: CtlReason) => void }) => void;
  clearMaps: (botId: string) => void;
}

async function runSingleBotTask(
  task: BotTask,
  entry: BotRegistryEntry,
  deps: SingleBotDeps,
): Promise<BotExitReason> {
  const label = `${task.participant}@${shortBotId(task.botId)}`;

  // Outer loop is "launch, run, optionally rejoin with new netsim,
  // run again". A `ctl-rejoin` is the only event that takes us back
  // through `launchBot`; everything else exits the loop.
  while (true) {
    entry.status = "launching";
    // Strip the orchestrator-only fields (`ttl`, `botId`) before
    // handing the rest to `launchBot` — and inject `botIdShort` so
    // the bot's log prefix reads `[participant@<id>]`.
    const { ttl, botId, ...rest } = entry.task;
    void botId;
    const launchOpts = { ...rest, botIdShort: shortBotId(task.botId) };
    let bot;
    try {
      bot = await launchBot(launchOpts);
    } catch (err) {
      if (err instanceof MeetingNavigatedAwayError) {
        console.log(`[${label}] exited cleanly: user dismissed via browser hang-up`);
        entry.status = "done";
        entry.finishReason = "user-hangup";
        entry.finishedAt = Date.now();
        deps.clearMaps(task.botId);
        return { kind: "user-hangup" };
      }
      if (err instanceof WaitingRoomError) {
        // The bot DID join — the meeting just parked it in a waiting
        // room (or "host hasn't started yet" lobby). Not a failure;
        // surface as a graceful exit so the orchestrator's
        // failure-tally doesn't paint a misleading picture for runs
        // where the operator intentionally joins a meeting they can't
        // self-admit into.
        console.log(`[${label}] exited cleanly: ${err.message}`);
        entry.status = "done";
        entry.finishReason = `waiting-room:${err.variant}`;
        entry.finishedAt = Date.now();
        deps.clearMaps(task.botId);
        return { kind: "waiting-room", variant: err.variant, detail: err.message };
      }
      if (err instanceof JoinRejectedError) {
        // Real failure (host denied us or the server reported an
        // error) — but report it cleanly instead of the misleading
        // "join button reappeared after 3 attempts" diagnostic.
        console.error(`[${label}] join rejected: ${err.message}`);
        entry.status = "failed";
        entry.lastError = err.message;
        entry.finishReason = `meeting-rejected:${err.reason}`;
        entry.finishedAt = Date.now();
        deps.clearMaps(task.botId);
        return { kind: "meeting-rejected", reason: err.reason, detail: err.message };
      }
      console.error(`[${label}] launch failed:`, (err as Error).message);
      entry.status = "failed";
      entry.lastError = (err as Error).message;
      entry.finishReason = "launch-error";
      entry.finishedAt = Date.now();
      deps.clearMaps(task.botId);
      return { kind: "launch-error", cause: err };
    }
    entry.handle = bot;
    entry.status = "in-meeting";
    entry.ttl = ttl;
    entry.ttlDeadline = ttl === "infinite" ? null : Date.now() + ttl;
    console.log(`[${label}] joined; ttl=${formatDuration(ttl)}`);

    // Build the rearmable TTL timer. The control server's `applyTtl`
    // calls `rearm()` to swap the underlying timer; cancellation
    // happens once on natural exit.
    const ttlCtl = createRearmableTtl(ttl);
    deps.registerTtlTimer(task.botId, ttlCtl);

    // Build the ctl signal — a one-shot promise the orchestrator can
    // race against TTL + shutdown. Returns the `CtlReason` so the
    // post-race code can branch on it.
    let resolveCtl!: (reason: CtlReason) => void;
    const ctlSignal = new Promise<CtlReason>((res) => {
      resolveCtl = res;
    });
    let ctlTriggered = false;
    deps.registerCtlSignal(task.botId, {
      trigger: (reason) => {
        if (ctlTriggered) return;
        ctlTriggered = true;
        resolveCtl(reason);
      },
    });

    // Same `as BotExitReason` cast as the pre-Phase 4 code: without it
    // TS narrows the variable to the initializer's singleton literal
    // and the post-race `kind !== "user-hangup"` check becomes
    // "always true" at the type level.
    let exitReason: BotExitReason = { kind: "ttl-expired" } as BotExitReason;
    let ctlReason: CtlReason | null = null;
    await Promise.race([
      ttlCtl.done.then(() => {
        exitReason = { kind: "ttl-expired" };
      }),
      deps.shutdownRequested.then(() => {
        exitReason = { kind: "shutdown-signal" };
      }),
      bot.userHangupDetected.then(() => {
        exitReason = { kind: "user-hangup" };
      }),
      ctlSignal.then((reason) => {
        ctlReason = reason;
      }),
    ]);
    ttlCtl.cancel();

    if (ctlReason === "ctl-rejoin") {
      // The control server stamped `entry.task.network` already; tear
      // this browser down and loop back into `launchBot`. The
      // operator sees this as a brief reconnect — the meeting drops
      // and re-establishes with the new netsim shim active.
      entry.status = "leaving";
      console.log(`[${label}] rejoining with new netsim profile '${entry.task.network}'`);
      try {
        await bot.leaveMeeting();
      } catch (e) {
        console.error(`[${label}] leaveMeeting (rejoin) failed:`, (e as Error).message);
      }
      await bot.shutdown();
      entry.handle = null;
      continue;
    }

    // Resolve the final exit reason for ctl-leave / ctl-kill.
    if (ctlReason === "ctl-leave") {
      exitReason = { kind: "shutdown-signal" };
    } else if (ctlReason === "ctl-kill") {
      exitReason = { kind: "shutdown-signal" };
    }

    entry.status = "leaving";
    const finishReason =
      ctlReason === "ctl-leave"
        ? "ctl-leave"
        : ctlReason === "ctl-kill"
          ? "ctl-kill"
          : exitReason.kind;
    console.log(`[${label}] shutting down (${finishReason})`);

    // Skip leaveMeeting on user-hangup (the page already navigated
    // away — clicking a non-existent button is a no-op) and on
    // ctl-kill (force-kill semantics).
    if (exitReason.kind !== "user-hangup" && ctlReason !== "ctl-kill") {
      try {
        await bot.leaveMeeting();
      } catch (e) {
        console.error(`[${label}] leaveMeeting failed:`, (e as Error).message);
      }
    }
    await bot.shutdown();
    entry.handle = null;
    entry.status = "done";
    entry.finishReason = finishReason;
    entry.finishedAt = Date.now();
    deps.clearMaps(task.botId);
    return exitReason;
  }
}

/**
 * `waitForTtl`-style controller, but with a `rearm(newTtl)` method so
 * a control-API mutation can swap in a new duration mid-flight
 * without tearing the parent race down. `done` resolves the first
 * time *any* armed timer fires; subsequent `rearm` calls after that
 * are no-ops.
 */
function createRearmableTtl(initialTtl: Ttl): {
  done: Promise<void>;
  cancel: () => void;
  rearm: (ttl: Ttl) => void;
} {
  let resolveDone!: () => void;
  const done = new Promise<void>((res) => {
    resolveDone = res;
  });
  let timer: ReturnType<typeof setTimeout> | null = null;
  let resolved = false;
  const arm = (ttl: Ttl): void => {
    if (resolved) return;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    if (ttl === "infinite") return;
    timer = setTimeout(() => {
      timer = null;
      resolved = true;
      resolveDone();
    }, ttl);
  };
  arm(initialTtl);
  return {
    done,
    cancel: (): void => {
      resolved = true;
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
    },
    rearm: arm,
  };
}

async function toggleMicrophone(entry: BotRegistryEntry, mute: boolean): Promise<void> {
  if (entry.handle === null) return;
  const { page } = entry.handle;
  // The Dioxus action bar auto-hides; hover so the buttons render.
  await page
    .locator(".video-controls-container")
    .first()
    .hover()
    .catch(() => {});
  const tooltips = mute
    ? ["Mute", "Mute Microphone", "Stop microphone"]
    : ["Unmute", "Unmute Microphone", "Start microphone"];
  await clickFirstMatchingTooltip(entry, tooltips, mute ? "mute" : "unmute");
}

async function toggleCamera(entry: BotRegistryEntry, cameraOff: boolean): Promise<void> {
  if (entry.handle === null) return;
  const { page } = entry.handle;
  await page
    .locator(".video-controls-container")
    .first()
    .hover()
    .catch(() => {});
  const tooltips = cameraOff
    ? ["Stop Video", "Stop Camera", "Stop camera"]
    : ["Start Video", "Start Camera", "Start camera"];
  await clickFirstMatchingTooltip(entry, tooltips, cameraOff ? "camera-off" : "camera-on");
}

/**
 * Click the in-meeting screen-share toggle. Same pattern as mic/cam:
 * hover the action bar so the auto-hide doesn't get in the way, then
 * click the button matching the tooltip text. The matching tooltips
 * live in `dioxus-ui/src/components/video_control_buttons.rs` —
 * `"Share Screen"` (idle) and `"Stop Screen Share"` (active).
 *
 * Note: the browser's `getDisplayMedia()` prompt cannot be auto-confirmed
 * by a Playwright click. The bot relies on `--use-fake-ui-for-media-stream`
 * (already in `CHROME_ARGS`) to bypass the prompt entirely — Chrome
 * picks the first available source. This is acceptable for bots; for
 * the human-operator case the operator wouldn't be using a bot.
 */
async function toggleScreenShare(entry: BotRegistryEntry, share: boolean): Promise<void> {
  if (entry.handle === null) return;
  const { page } = entry.handle;
  await page
    .locator(".video-controls-container")
    .first()
    .hover()
    .catch(() => {});
  const tooltips = share ? ["Share Screen"] : ["Stop Screen Share"];
  await clickFirstMatchingTooltip(entry, tooltips, share ? "share-start" : "share-stop");
}

async function clickFirstMatchingTooltip(
  entry: BotRegistryEntry,
  tooltips: readonly string[],
  action: string,
): Promise<void> {
  if (entry.handle === null) return;
  const { page } = entry.handle;
  const label = `${entry.task.participant}@${shortBotId(entry.botId)}`;
  for (const tooltip of tooltips) {
    const btn = page.locator(
      `button.video-control-button:has(span.tooltip:has-text("${tooltip}"))`,
    );
    if (await btn.isVisible({ timeout: 1_000 }).catch(() => false)) {
      try {
        await btn.click({ timeout: 2_000 });
        console.log(`[${label}] ctl ${action} → clicked '${tooltip}'`);
        return;
      } catch (e) {
        console.warn(`[${label}] ctl ${action} click failed: ${(e as Error).message}`);
      }
    }
  }
  console.warn(
    `[${label}] ctl ${action}: no matching control button visible (tried: ${tooltips.join(", ")}) — the bot may not be in-meeting yet`,
  );
}

function defaultDisplayName(participant: string): string {
  if (participant.includes("@")) return participant.split("@", 1)[0];
  return participant.charAt(0).toUpperCase() + participant.slice(1);
}

/**
 * Number of registry entries that ended in the `failed` state. Drives
 * the post-run "N/M bot(s) ended with an error" tally. Exported so the
 * orchestrator's exit-classification rules are unit-testable without
 * spinning up Playwright — see `orchestrator.test.ts`.
 *
 * `done` entries (including the `waiting-room:*` finishReason variants)
 * are deliberately NOT counted: those bots joined successfully and
 * exited gracefully when the meeting parked them in a lobby they had
 * no admit rights for.
 */
export function countFailed(registry: Map<string, BotRegistryEntry>): number {
  let n = 0;
  for (const entry of registry.values()) {
    if (entry.status === "failed") n++;
  }
  return n;
}

// `parseDuration` is exported by `./ttl`; re-exporting here keeps the
// orchestrator's public surface convenient for the CLI.
export { parseDuration };
