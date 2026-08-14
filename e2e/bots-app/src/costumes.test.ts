import {
  closeSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  rmSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  COSTUME_HEIGHT,
  COSTUME_WIDTH,
  prepareParticipantCostume,
  y4mMatchesTargetGeometry,
} from "./costumes";

const spawnSyncMock = vi.hoisted(() => vi.fn());
vi.mock("node:child_process", () => ({ spawnSync: spawnSyncMock }));

/**
 * The costume y4m cache is keyed on mtime, which cannot see a geometry change:
 * the source MP4 is untouched when the target size changes, so an existing 720p
 * y4m would be reused and #2171 would ship inert on every machine that had
 * already run `prep-assets`.
 */
describe("y4m cache geometry guard", () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "y4m-geom-"));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  function writeY4m(name: string, header: string): string {
    const p = join(dir, name);
    writeFileSync(p, `${header}\nFRAME\n`);
    return p;
  }

  it("accepts a cached file already at the target geometry", () => {
    const p = writeY4m("ok.y4m", `YUV4MPEG2 W${COSTUME_WIDTH} H${COSTUME_HEIGHT} F30:1 Ip A1:1`);
    expect(y4mMatchesTargetGeometry(p)).toBe(true);
  });

  it("rejects a stale 1280x720 cache — the case that would ship the fix inert", () => {
    const p = writeY4m("stale.y4m", "YUV4MPEG2 W1280 H720 F30:1 Ip A1:1");
    expect(y4mMatchesTargetGeometry(p)).toBe(false);
  });

  it("rejects a file whose width merely contains the target digits", () => {
    // `W6400` must not satisfy a `W640` target — a substring match would.
    const p = writeY4m("wide.y4m", `YUV4MPEG2 W${COSTUME_WIDTH}0 H${COSTUME_HEIGHT}0 F30:1`);
    expect(y4mMatchesTargetGeometry(p)).toBe(false);
  });

  it("rejects a cache with the right width but the wrong height", () => {
    // Height was unpinned: a `W640 H360` cache satisfied a width-only check,
    // which is exactly what a letterboxing filter change could leave behind.
    const p = writeY4m("wrongh.y4m", `YUV4MPEG2 W${COSTUME_WIDTH} H360 F30:1 Ip A1:1`);
    expect(y4mMatchesTargetGeometry(p)).toBe(false);
  });

  it("rejects a file that is not y4m at all", () => {
    // Without the magic check, any 64-byte prefix containing the digits passes.
    const p = join(dir, "noty4m.y4m");
    writeFileSync(p, `some other format W${COSTUME_WIDTH} H${COSTUME_HEIGHT}\n`);
    expect(y4mMatchesTargetGeometry(p)).toBe(false);
  });

  it("rejects a missing file rather than throwing", () => {
    expect(y4mMatchesTargetGeometry(join(dir, "absent.y4m"))).toBe(false);
  });

  it("rejects a truncated header", () => {
    const p = join(dir, "trunc.y4m");
    const fd = openSync(p, "w");
    writeSync(fd, "YUV4M");
    closeSync(fd);
    expect(y4mMatchesTargetGeometry(p)).toBe(false);
  });
});

/**
 * Guards the CALL SITE. `y4mMatchesTargetGeometry` has its own tests above, but
 * removing it from the cache check left every one of them green — the rebuild
 * decision is where the fix either lands or ships inert.
 */
describe("costume cache rebuild decision", () => {
  let dir: string;
  let srcDir: string;

  const manifest = {
    participants: [{ name: "alice", costumeDir: "pirate" }],
  } as unknown as Parameters<typeof prepareParticipantCostume>[0];

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "costume-cache-"));
    srcDir = join(dir, "src");
    mkdirSync(join(srcDir, "pirate"), { recursive: true });
    writeFileSync(join(srcDir, "pirate", "talking.mp4"), "x");
    spawnSyncMock.mockReset();
    spawnSyncMock.mockReturnValue({ status: 0 });
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  function seedCache(header: string): string {
    const out = join(dir, "out");
    mkdirSync(out, { recursive: true });
    // Written after the source, so the mtime check alone would call it fresh.
    writeFileSync(join(out, "pirate.y4m"), `${header}\nFRAME\n`);
    return out;
  }

  it("REBUILDS when the cached y4m is a stale 1280x720", () => {
    const out = seedCache("YUV4MPEG2 W1280 H720 F30:1 Ip A1:1");
    const res = prepareParticipantCostume(manifest, "alice", srcDir, out);
    expect(spawnSyncMock).toHaveBeenCalledTimes(1);
    expect(res.rebuilt).toBe(true);
    // ...and it rebuilds at the new geometry.
    const filter = (spawnSyncMock.mock.calls[0][1] as string[]).join(" ");
    expect(filter).toContain(`scale=${COSTUME_WIDTH}:${COSTUME_HEIGHT}`);
    // Square pixels, not just the right raster: `scale` alone on a 16:9 source
    // emits SAR 4:3 (a squashed face at the correct pixel count), and Chrome
    // passes that through. The crop matches a 4:3 sensor's field of view.
    expect(filter).toContain("setsar=1");
    expect(filter).toContain(`crop=${COSTUME_WIDTH}:${COSTUME_HEIGHT}`);
    // Aspect-agnostic: a bare `crop=ih*4/3:ih` derives a width from the height and
    // vf_crop rejects out_w > in_w, hard-failing on a narrower-than-4:3 source.
    expect(filter).toContain("force_original_aspect_ratio=increase");
    expect(filter).not.toContain("crop=ih*4/3:ih");
  });

  it("reuses a cache already at the target geometry", () => {
    const out = seedCache(`YUV4MPEG2 W${COSTUME_WIDTH} H${COSTUME_HEIGHT} F30:1 Ip A1:1`);
    const res = prepareParticipantCostume(manifest, "alice", srcDir, out);
    expect(spawnSyncMock).not.toHaveBeenCalled();
    expect(res.rebuilt).toBe(false);
  });
});
