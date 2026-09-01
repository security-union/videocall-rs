// @vitest-environment jsdom
// Issue #2264: drives the real dioxus-ui/scripts/recording.js, loaded from disk.
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { beforeAll, describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..");
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const RECORDING_REL = "dioxus-ui/scripts/recording.js";

type TileEntry = { overflowCount?: number; [key: string]: unknown };

type FrameParticipant = {
  domTile: Element | null;
  videoEl: Element | null;
  sid: string | null;
  pending: boolean;
};

type RecordingInternals = {
  _readGridOverflowCount(grid: Element): number;
  _buildFrameParticipants(
    grid: Element,
    decoderCanvasMap: Record<string, HTMLCanvasElement>,
    overflowCount: number,
  ): FrameParticipant[];
  _composeTileOrder(
    peerTiles: TileEntry[],
    localTile: TileEntry,
    overflowCount: number,
  ): TileEntry[];
};

let recording: RecordingInternals;

beforeAll(() => {
  const source = readFileSync(join(REPO_ROOT, RECORDING_REL), "utf8");
  new Function(source)();
  recording = (window as unknown as { __vcRecording: RecordingInternals }).__vcRecording;
});

function mountGrid(inner: string): HTMLElement {
  document.body.innerHTML = `<div id="grid-container">${inner}</div>`;
  return document.getElementById("grid-container")!;
}

/** The markup `GridOverflowBadge` renders (dioxus-ui/src/components). */
function badge(count: string): string {
  return `<div class="grid-overflow-badge" data-overflow-count="${count}">+${count}<span>more in meeting</span></div>`;
}

/** A peer tile as `canvas_generator.rs` emits it, with no <canvas> mounted. */
function peerTile(sid: string): string {
  return `<div id="peer-video-${sid}-div" data-tile-root="true"></div>`;
}

/** Zero-sized, so `canvasHasContent` is never reached: jsdom has no 2D context. */
function registeredCanvas(): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  canvas.width = 0;
  canvas.height = 0;
  return canvas;
}

describe("recording.js drift lock", () => {
  it("re-runs on a recording.js change, or an edit lands with this suite unread", () => {
    const wf = parseYaml(readFileSync(join(REPO_ROOT, PR_CHECK_REL), "utf8")) as Record<
      string,
      unknown
    >;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    expect(paths, `pr-check-e2e-lint-hcl.yaml must trigger on ${RECORDING_REL}`).toContain(
      RECORDING_REL,
    );
  });
});

describe("readGridOverflowCount", () => {
  it("reads the count the live grid rendered", () => {
    expect(recording._readGridOverflowCount(mountGrid(badge("7")))).toBe(7);
  });

  it("returns 0 when the grid rendered no badge", () => {
    expect(recording._readGridOverflowCount(mountGrid(peerTile("225")))).toBe(0);
  });

  it("returns 0 for the pre-#2264 badge that carried no count attribute", () => {
    const stripped = `<div class="grid-overflow-badge">+7<span>more in meeting</span></div>`;
    expect(recording._readGridOverflowCount(mountGrid(stripped))).toBe(0);
  });

  it.each(["", "abc", "0", "-3", "NaN"])("returns 0 for the junk value %s", (junk) => {
    expect(recording._readGridOverflowCount(mountGrid(badge(junk)))).toBe(0);
  });
});

describe("buildFrameParticipants", () => {
  it("admits a decoder-only peer while the grid is NOT capacity-bound", () => {
    const grid = mountGrid(peerTile("225"));
    const participants = recording._buildFrameParticipants(
      grid,
      { "225": registeredCanvas(), "226": registeredCanvas() },
      recording._readGridOverflowCount(grid),
    );

    // 226 has no DOM tile: the mid-call-joiner path pass 2 must still serve.
    expect(participants.map((p) => p.sid)).toEqual(["225", "226"]);
    expect(participants.map((p) => p.pending)).toEqual([false, true]);
  });

  it("suppresses decoder-only peers once the grid overflows (issue #2264)", () => {
    const grid = mountGrid(peerTile("225") + badge("7"));
    const participants = recording._buildFrameParticipants(
      grid,
      { "225": registeredCanvas(), "226": registeredCanvas() },
      recording._readGridOverflowCount(grid),
    );

    // Drawing 226 would add a chrome-less tile AND double-count it against +7.
    expect(participants.map((p) => p.sid)).toEqual(["225"]);
  });

  it("keeps every DOM tile when the grid overflows", () => {
    const grid = mountGrid(peerTile("225") + peerTile("226") + badge("7"));
    const participants = recording._buildFrameParticipants(
      grid,
      {},
      recording._readGridOverflowCount(grid),
    );

    expect(participants.map((p) => p.sid)).toEqual(["225", "226"]);
  });
});

describe("composeTileOrder", () => {
  it("appends the +N cell exactly once, after the local tile", () => {
    const peers: TileEntry[] = [{ name: "a" }, { name: "b" }, { name: "c" }];
    const local: TileEntry = { name: "me" };

    const tiles = recording._composeTileOrder(peers, local, 7);

    expect(tiles).toHaveLength(peers.length + 2);
    expect(tiles[tiles.length - 1]).toEqual({ overflowCount: 7 });
    expect(tiles[tiles.length - 2]).toBe(local);
    expect(tiles.filter((t) => t.overflowCount !== undefined)).toHaveLength(1);
  });

  it("appends no cell when nothing overflowed", () => {
    const tiles = recording._composeTileOrder([{ name: "a" }, { name: "b" }], { name: "me" }, 0);

    expect(tiles).toHaveLength(3);
    expect(tiles.some((t) => t.overflowCount !== undefined)).toBe(false);
  });
});
