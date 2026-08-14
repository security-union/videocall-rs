import type { ChildProcess } from "node:child_process";

/**
 * Shared child-process reaping for bounded outbound `ssh` invocations
 * (issue 2133 and its sibling in `resource/session.ts`).
 *
 * Both call sites bound a spawned `ssh` on a timer and must then actually
 * REAP the child rather than abandon it — otherwise repeated attempts
 * against a wedged host accumulate orphaned `ssh` processes on the
 * bots-app host, which is the substance of the bug in both places.
 *
 * The escalation is the part worth sharing: it is small but every line of
 * it is a footgun, so a second hand-rolled copy is how the next bug gets
 * in. It lives in its own module rather than in `ssh-hosts.ts` because
 * both consumers (`control/` and `resource/`) are peers — neither owns it.
 *
 * Deliberately NOT shared: the surrounding `settle`-once guards and the
 * deadline semantics. Those legitimately differ (absolute for the probe,
 * inactivity for the CSV transfer), and forcing one generic over two
 * different result types would obscure both.
 */

/**
 * Handle returned by {@link reapChild}.
 */
export interface ReapHandle {
  /**
   * Cancel the pending escalation. The caller MUST call this from the
   * child's `exit` handler: a child that DOES exit on the `SIGTERM` must
   * not be signalled again, because the OS may have recycled its pid by
   * then and the queued `SIGKILL` could land on an unrelated process.
   * Cancelling also clears the timer, which is what releases the event
   * loop, and resolves {@link settled}.
   */
  cancel(): void;
  /**
   * Resolves once the reap has run its course — either the `SIGKILL` fired
   * or {@link cancel} was called because the child died on the `SIGTERM`.
   *
   * A caller that is about to terminate the process MUST await this (see
   * the timer-lifetime note on {@link reapChild}); a long-lived caller can
   * ignore it.
   */
  settled: Promise<void>;
}

/**
 * Signal `child` to die, escalating if it will not.
 *
 * Sends `SIGTERM` immediately, then schedules a `SIGKILL` for `graceMs`
 * later. The escalation is not optional: an `ssh` blocked in a syscall
 * can ignore `SIGTERM`, and that is precisely the wedged-host case that
 * leaks the process.
 *
 * ## Why the grace timer is deliberately NOT `unref`'d
 *
 * `unref()` removes a timer from the event-loop refcount, so it fires only
 * if something ELSE keeps the process alive. That is fatal for an exiting
 * caller: `retrieve`'s chain (`finalizeAll` → `orchestrator.ts:596` →
 * `cli.ts:508`) reaches `process.exit(0)` immediately after awaiting, and
 * `process.exit` runs no pending timers — so an unref'd grace timer meant
 * the escalation NEVER happened and a SIGTERM-ignoring `ssh` was orphaned
 * (reproduced with a `trap "" TERM; exec sleep` child, which survived the
 * parent's exit).
 *
 * An earlier revision made this a per-caller `unref` option, on the theory
 * that the long-lived ctl-server caller wanted it so a pending reap could
 * never delay an idle shutdown. That option is gone, because it was dead
 * complexity in the precise way footguns rot back in — forcing it on
 * unconditionally left the whole suite green, i.e. NEITHER branch was
 * covered. It is unnecessary for two independent reasons:
 *
 *   1. {@link ReapHandle.settled} + the caller's `await` already bound the
 *      window to `graceMs` (2s), so a ref'd timer cannot hang anything.
 *   2. Both callers cancel via {@link ReapHandle.cancel} on the child's
 *      `exit`, which clears the timer outright — and the probe's tests
 *      assert zero pending timers after every settle path.
 *
 * So: always ref, and let `cancel`/`settled` bound it. A ref'd timer nobody
 * cancels delays an idle exit by at most `graceMs`; an unref'd one silently
 * drops the kill. The former is the far safer failure mode.
 *
 * Reaping is cleanup, so callers should have ALREADY resolved whatever
 * promise their consumer awaits — with one exception: a caller about to
 * `process.exit` must also await {@link ReapHandle.settled}, or the
 * escalation is cut off mid-flight.
 */
export function reapChild(
  child: Pick<ChildProcess, "kill">,
  opts: { graceMs: number },
): ReapHandle {
  try {
    child.kill("SIGTERM");
  } catch {
    // Already reaped by the OS — nothing to do.
  }
  let finish!: () => void;
  const settled = new Promise<void>((resolveFn) => {
    finish = resolveFn;
  });
  const killTimer = setTimeout(() => {
    try {
      child.kill("SIGKILL");
    } catch {
      // ditto
    }
    finish();
  }, opts.graceMs);
  return {
    cancel: (): void => {
      clearTimeout(killTimer);
      finish();
    },
    settled,
  };
}
