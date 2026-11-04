import {
  ACTION_BAR_SELECTOR,
  cameraControlSelector,
  type ControlState,
  MIC_UNMUTE_SELECTOR,
  micControlSelector,
} from "./control-buttons";
import { MEDIA_SETTLE_TIMEOUT_MS } from "./meeting-join";
import { type ControlTarget, MIN_TOGGLE_GAP_MS } from "./persona";

export type MediaKind = "mic" | "camera";
const KINDS: readonly MediaKind[] = ["mic", "camera"];

const SELECTOR: Record<MediaKind, Record<ControlState, string>> = {
  mic: { on: micControlSelector("on"), off: MIC_UNMUTE_SELECTOR },
  camera: { on: cameraControlSelector("on"), off: cameraControlSelector("off") },
};

export const HOST_RENDER =
  /^Host render: video=(true|false) prev=(true|false) mic=(true|false) prev=(true|false) /;

/** Media whose state changed on a `Host render:` line, with the new state. */
export function parseHostRender(text: string): Array<{ kind: MediaKind; on: boolean }> {
  const m = HOST_RENDER.exec(text);
  if (m === null) return [];
  const edges: Array<{ kind: MediaKind; on: boolean }> = [];
  if (m[1] !== m[2]) edges.push({ kind: "camera", on: m[1] === "true" });
  if (m[3] !== m[4]) edges.push({ kind: "mic", on: m[3] === "true" });
  return edges;
}

export interface PersonaTally {
  ok: number;
  failed: number;
  /** Planned toggles the control already showed when read, so no click was made. */
  already: number;
  /** Clicked toggles whose `Host render:` edge arrived, and those whose edge never did. */
  rendered: number;
  unrendered: number;
  /** Median |render edge − planned edge| over rendered toggles planned after the driver started; null before one. */
  medianAbsMs: number | null;
  /** Shortest time between two `Host render:` edges of one control; null before a second edge. */
  minGapMs: number | null;
}

export interface DriverLocator {
  first(): DriverLocator;
  hover(options?: { timeout?: number }): Promise<void>;
  isVisible(options?: { timeout?: number }): Promise<boolean>;
  click(options?: { timeout?: number }): Promise<void>;
  waitFor(options: { state: "visible"; timeout?: number }): Promise<void>;
}

export interface DriverPage {
  locator(selector: string): DriverLocator;
}

export interface PersonaDriver {
  onConsole(text: string): void;
  /** Idempotent. */
  stop(): PersonaTally;
}

interface Pending {
  on: boolean;
  plannedMs: number;
  timed: boolean;
}

/** One serialized queue: at most one control read or click in flight per page. */
export function startPersonaDriver(args: {
  page: DriverPage;
  targets: (tMs: number) => Record<MediaKind, ControlTarget>;
  joined: Record<MediaKind, boolean | null>;
  log: (message: string) => void;
  error: (message: string) => void;
  onTally?: (tally: PersonaTally) => void;
  now?: () => number;
  tickMs?: number;
  settleMs?: number;
}): PersonaDriver {
  const now = args.now ?? Date.now;
  const tickMs = args.tickMs ?? 250;
  const settleMs = args.settleMs ?? MEDIA_SETTLE_TIMEOUT_MS;
  const commanded: Record<MediaKind, boolean | null> = { ...args.joined };
  const lastClick: Record<MediaKind, number> = { mic: -Infinity, camera: -Infinity };
  const lastEdge: Record<MediaKind, number | null> = { mic: null, camera: null };
  const failedEdge: Record<MediaKind, number | null> = { mic: null, camera: null };
  const pending: Record<MediaKind, Pending | null> = { mic: null, camera: null };
  const startedAt = now();
  const offsets: number[] = [];
  const tally: PersonaTally = {
    ok: 0,
    failed: 0,
    already: 0,
    rendered: 0,
    unrendered: 0,
    medianAbsMs: null,
    minGapMs: null,
  };
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const report = (): void => {
    const sorted = [...offsets].sort((a, b) => a - b);
    tally.medianAbsMs = sorted.length === 0 ? null : sorted[Math.floor((sorted.length - 1) / 2)];
    try {
      args.onTally?.({ ...tally });
    } catch (e) {
      args.error(`persona: tally sink failed (${String(e)})`);
    }
  };
  const abandon = (kind: MediaKind): void => {
    if (pending[kind] === null) return;
    pending[kind] = null;
    tally.unrendered += 1;
  };
  const stamp = (ms: number): string => new Date(ms).toISOString().slice(11, 23);

  async function toggle(kind: MediaKind, target: ControlTarget): Promise<void> {
    const t = now();
    lastClick[kind] = t;
    const word = `${kind} ${target.on ? "on" : "off"}`;
    const sel = SELECTOR[kind];
    const fail = (reason: string): void => {
      failedEdge[kind] = target.since;
      tally.failed += 1;
      args.error(`persona: FAILED ${word} (${reason})`);
    };
    await args.page
      .locator(ACTION_BAR_SELECTOR)
      .first()
      .hover({ timeout: 2_000 })
      .catch(() => {});
    const visible = (s: string): Promise<boolean> =>
      args.page
        .locator(s)
        .isVisible({ timeout: 2_000 })
        .catch(() => false);
    const isOn = await visible(sel.on);
    const shown = isOn || (await visible(sel.off));
    if (stopped) return;
    if (!shown) return fail("no visible control");
    if (isOn === target.on) {
      commanded[kind] = target.on;
      tally.already += 1;
      return;
    }
    abandon(kind);
    const timed = Number.isFinite(target.since) && target.since >= startedAt;
    pending[kind] = { on: target.on, plannedMs: target.since, timed };
    try {
      await args.page.locator(target.on ? sel.off : sel.on).click({ timeout: 2_000 });
      await args.page
        .locator(target.on ? sel.on : sel.off)
        .waitFor({ state: "visible", timeout: settleMs });
    } catch (e) {
      if (stopped) return;
      pending[kind] = null;
      commanded[kind] = null;
      return fail((e as Error).message.split("\n")[0]);
    }
    if (stopped) return;
    commanded[kind] = target.on;
    tally.ok += 1;
    const plan = timed ? `planned ${stamp(target.since)}` : "initial";
    args.log(`persona: ${word} ${plan} clicked ${stamp(t)}`);
  }

  const schedule = (): void => {
    if (stopped) return;
    timer = setTimeout(tick, tickMs);
    timer.unref?.();
  };
  function tick(): void {
    if (stopped) return;
    const t = now();
    const want = args.targets(t);
    const kind = KINDS.find(
      (k) =>
        commanded[k] !== want[k].on &&
        t - lastClick[k] >= MIN_TOGGLE_GAP_MS &&
        failedEdge[k] !== want[k].since,
    );
    if (kind === undefined) return schedule();
    void toggle(kind, want[kind])
      .catch((e: unknown) => args.error(`persona: INTERNAL fault (${String(e)})`))
      .finally(() => {
        if (stopped) return;
        report();
        schedule();
      });
  }
  schedule();

  return {
    onConsole(text: string): void {
      for (const edge of parseHostRender(text)) {
        const t = now();
        const prev = lastEdge[edge.kind];
        if (prev !== null) tally.minGapMs = Math.min(tally.minGapMs ?? Infinity, t - prev);
        lastEdge[edge.kind] = t;
        const p = pending[edge.kind];
        if (p === null || p.on !== edge.on) continue;
        pending[edge.kind] = null;
        tally.rendered += 1;
        if (p.timed) offsets.push(Math.abs(t - p.plannedMs));
        args.log(
          `persona: ${edge.kind} ${edge.on ? "on" : "off"} rendered ${stamp(t)}${p.timed ? ` (${t - p.plannedMs} ms after plan)` : ""}`,
        );
      }
    },
    stop(): PersonaTally {
      if (!stopped) {
        stopped = true;
        if (timer !== undefined) clearTimeout(timer);
        for (const k of KINDS) abandon(k);
        report();
      }
      return { ...tally };
    },
  };
}

/** One shutdown line; `degraded` when a toggle failed, so the persona's duty cycle is not the planned one. */
export function personaReceipt(
  label: string,
  tally: PersonaTally,
): { degraded: boolean; line: string } {
  const degraded = tally.failed > 0;
  const median = tally.medianAbsMs === null ? "n/a" : `${tally.medianAbsMs}`;
  const gap = tally.minGapMs === null ? "n/a" : `${tally.minGapMs}`;
  return {
    degraded,
    line:
      `${degraded ? "PERSONA_DEGRADED" : "PERSONA_APPLIED"} ${label} ` +
      `toggles=ok:${tally.ok}/failed:${tally.failed}/already:${tally.already} ` +
      `rendered=${tally.rendered}/unrendered:${tally.unrendered} median_abs_ms=${median} min_gap_ms=${gap}`,
  };
}
