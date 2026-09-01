import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

import { isNetemProfileName, NETEM_PROFILES } from "./netem";

/**
 * Locks the README's shaped-run classification to the product's real ladders.
 * Only the "below the ceiling" side is asserted, and the README must call the
 * complement unclassified: the offer is a sum of targets, measured nowhere.
 */
const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..", "..");

/** Same strings the CI trigger below must carry, so the two cannot diverge. */
const AQ_CONSTANTS_REL = "videocall-aq/src/constants.rs";
const MIC_ENCODER_REL = "videocall-client/src/encode/microphone_encoder.rs";
const HOST_REL = "dioxus-ui/src/components/host.rs";
const UI_CONSTANTS_REL = "dioxus-ui/src/constants.rs";
const AQ_CONSTANTS = resolve(REPO_ROOT, AQ_CONSTANTS_REL);
const MIC_ENCODER = resolve(REPO_ROOT, MIC_ENCODER_REL);
const HOST = resolve(REPO_ROOT, HOST_REL);
const UI_CONSTANTS = resolve(REPO_ROOT, UI_CONSTANTS_REL);
const CLOCK_SOURCE = resolve(import.meta.dirname, "..", "clock-source.js");
const PR_CHECK = resolve(REPO_ROOT, ".github", "workflows", "pr-check-e2e-lint-hcl.yaml");

const README = resolve(import.meta.dirname, "..", "..", "README.md");

const sum = (xs: number[]): number => xs.reduce((a, b) => a + b, 0);
const aq = (): string => readFileSync(AQ_CONSTANTS, "utf8");

/** The `&[VideoQualityTier…]` initialiser for one const. */
function tierBlock(constName: string): string {
  const block = new RegExp(`pub const ${constName}:[^=]*=\\s*&\\[([\\s\\S]*?)\\];`).exec(aq());
  expect(block, `${constName} must be findable in ${AQ_CONSTANTS_REL}`).not.toBeNull();
  return block![1];
}

/** Rung labels in ladder order. Readable even where the budget is a computed const. */
function tierLabels(constName: string): string[] {
  const out = [...tierBlock(constName).matchAll(/label:\s*"([^"]+)"/g)].map((m) => m[1]);
  expect(out.length, `${constName} must parse to at least one rung`).toBeGreaterThan(0);
  return out;
}

/** Per-rung `ideal_bitrate_kbps`. Only ladders whose targets are literals. */
function cameraLadderKbit(constName: string): number[] {
  const block = tierBlock(constName);
  const out = [...block.matchAll(/label:\s*"[^"]+"[\s\S]*?ideal_bitrate_kbps:\s*(\d+)/g)].map((m) =>
    Number(m[1]),
  );
  expect(out, `${constName}'s targets must all be numeric literals`).toHaveLength(
    tierLabels(constName).length,
  );
  return out;
}

const cam = (): number[] => cameraLadderKbit("SIMULCAST_VIDEO_LAYERS");

function aqInt(name: string): number {
  const m = new RegExp(`pub const ${name}: u(?:32|64|size) = (\\d+);`).exec(aq());
  expect(m, `${name} must be findable in ${AQ_CONSTANTS_REL}`).not.toBeNull();
  return Number(m![1]);
}

/** Mirrors `screen_bitrate_kbps_for`, floor included. Cross-checked below against
 * the Rust suite's own literal pin, so a formula change cannot pass silently. */
function screenKbitFor(w: number, h: number, fps: number): number {
  const bits = (w * h * fps * aqInt("SCREEN_BITS_PER_PIXEL_MILLI")) / 1000;
  return Math.max(Math.floor(bits / 1000), aqInt("SCREEN_MIN_BITRATE_KBPS"));
}

const screenCeilingKbit = (): number =>
  screenKbitFor(
    aqInt("SCREEN_MAX_ENCODE_WIDTH"),
    aqInt("SCREEN_MAX_ENCODE_HEIGHT"),
    aqInt("SCREEN_TARGET_FPS"),
  );

/** Camera + audio + the one screen rung, all published concurrently. */
const sharingCeilingKbit = (): number => sum(cam()) + audioCeilingKbit() + screenCeilingKbit();

/** Audio layers the publisher emits, which the README's one-tier term depends on. */
function publishedAudioLayers(): number {
  const src = readFileSync(UI_CONSTANTS, "utf8");
  const m = /pub fn audio_published_layer_count\(\) -> u32 \{\s*(\d+)\s*\}/.exec(src);
  expect(
    m,
    `audio_published_layer_count must return a literal in ${UI_CONSTANTS_REL}`,
  ).not.toBeNull();
  return Number(m![1]);
}

/** The ONE published audio layer inits at the top AQ tier and the AQ walk only
 * lowers it, so the audio term is one tier value, never a ladder sum. */
function audioCeilingKbit(): number {
  expect(
    publishedAudioLayers(),
    "the publisher emits more than one audio layer, so the README's audio term is a sum again",
  ).toBe(1);
  const src = readFileSync(MIC_ENCODER, "utf8");
  expect(
    src,
    `the single-layer mic path must init from AUDIO_QUALITY_TIERS[0] in ${MIC_ENCODER_REL}`,
  ).toMatch(
    /let initial_tier = &AUDIO_QUALITY_TIERS\[0\];[\s\S]*?let base_bitrate_bps = if audio_simulcast \{[\s\S]*?\} else \{\s*initial_tier\.bitrate_kbps \* 1000\s*\};/,
  );
  expect(src, "audio simulcast must still mean more than one layer").toContain(
    "let audio_simulcast = n_audio_layers > 1;",
  );
  const kbps = /bitrate_kbps:\s*(\d+)/.exec(tierBlock("AUDIO_QUALITY_TIERS"));
  expect(kbps, "AUDIO_QUALITY_TIERS[0] must carry a literal bitrate_kbps").not.toBeNull();
  return Number(kbps![1]);
}

const cameraCeilingKbit = (videoConst: string): number => sum(cameraLadderKbit(videoConst));

type ShapingProfile = NonNullable<(typeof NETEM_PROFILES)[string]>;

const SHAPING: Array<[string, ShapingProfile]> = Object.entries(NETEM_PROFILES).filter(
  (entry): entry is [string, ShapingProfile] => entry[1] !== null,
);

const rateOf = ([, p]: [string, ShapingProfile]): number => p.rateKbit ?? Infinity;
const namesOf = (xs: Array<[string, ShapingProfile]>): string[] => xs.map(([n]) => n).sort();

/** Strictly under the nominal total — the only side the README may classify. */
const cannotCarry = (kbit: number): string[] => namesOf(SHAPING.filter((e) => rateOf(e) < kbit));

const readme = (): string => readFileSync(README, "utf8");

const rateCaveat = (): string => {
  const bullet = /- \*\*A `rate` under the publisher's offer[\s\S]*?(?=\n- \*\*)/.exec(readme());
  expect(bullet, "the README's rate caveat bullet must be findable").not.toBeNull();
  return bullet![0];
};

const quoted = (text: string): number[] => [...text.matchAll(/~(\d+)/g)].map((m) => Number(m[1]));

/**
 * Profile names in ONE labelled clause, ended at the next bold marker or line
 * break so a clause cannot capture the prose that follows it.
 */
function clauseProfiles(label: string): string[] {
  const m = new RegExp(`\\*\\*${label}:\\*\\*([^\\n*]*)`).exec(rateCaveat());
  expect(m, `the README's caveat must carry a '${label}:' clause`).not.toBeNull();
  return [...m![1].matchAll(/`([a-z0-9_]+)`/g)]
    .map((x) => x[1])
    .filter(isNetemProfileName)
    .sort();
}

const CAMERA_FIGURES = /sum to ~(\d+) kbit \([^:]*`SIMULCAST_VIDEO_LAYERS`: ((?:~\d+(?: \+ )?)+)\)/;
const AUDIO_FIGURE = /~(\d+) kbit of audio \(`videocall-aq`'s `AUDIO_QUALITY_TIERS\[0\]`/;

describe("NETEM_PROFILES vs the publisher's offer (#2354)", () => {
  it("shapes at least one profile", () => {
    expect(SHAPING.length).toBeGreaterThan(0);
  });

  it.each([AQ_CONSTANTS_REL, MIC_ENCODER_REL, HOST_REL, UI_CONSTANTS_REL])(
    "re-runs on a %s change, or a ladder edit lands with this lock unread",
    (path) => {
      const wf = parseYaml(readFileSync(PR_CHECK, "utf8")) as Record<string, unknown>;
      const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
      expect(on?.pull_request?.paths ?? []).toContain(path);
    },
  );

  it("pins the camera figures the README quotes to the real ladder", () => {
    const m = CAMERA_FIGURES.exec(readme());
    expect(m, "the README's camera sentence must be findable").not.toBeNull();
    const rungs = cam();
    expect(quoted(m![2]), "the README's per-rung camera figures have drifted").toEqual(rungs);
    expect(Number(m![1]), "the README's camera total ≠ its rungs").toBe(sum(rungs));
  });

  it("pins the README's audio term to the one layer a publisher emits", () => {
    const m = AUDIO_FIGURE.exec(readme());
    expect(m, "the README's audio sentence must be findable").not.toBeNull();
    expect(Number(m![1]), "the README's audio term has drifted").toBe(audioCeilingKbit());
  });

  it("pins every profile rate the README quotes to NETEM_PROFILES", () => {
    const rates = [...rateCaveat().matchAll(/`([a-z0-9_]+)` \((\d+) kbit/g)];
    expect(rates.length, "the caveat must quote at least one profile's rate").toBeGreaterThan(0);
    for (const [, name, kbit] of rates) {
      expect(isNetemProfileName(name), `the README quotes a rate for unknown '${name}'`).toBe(true);
      expect(Number(kbit), `the README's ${name} rate has drifted`).toBe(
        NETEM_PROFILES[name]!.rateKbit,
      );
    }
  });

  it.each([["SIMULCAST_VIDEO_LAYERS", "Cannot carry the default camera ladder"]] as const)(
    "classifies every profile against %s exactly as the README does",
    (c, cannot) => {
      const ceiling = cameraCeilingKbit(c);
      expect(
        clauseProfiles(cannot),
        `a shaped run on these under-represents room-wide decode load (${c} ceiling ${ceiling} kbit) — update the README's caveat`,
      ).toEqual(cannotCarry(ceiling));
    },
  );

  it("makes the README call every unlisted profile unclassified, not faithful", () => {
    const listed = new Set(
      ["Cannot carry the default camera ladder", "Cannot carry a SHARING bot's total"].flatMap(
        clauseProfiles,
      ),
    );
    const rest = namesOf(SHAPING).filter((n) => !listed.has(n));
    expect(rest, "with nothing unlisted the disclaimer would be vacuous").not.toEqual([]);
    expect(
      rateCaveat(),
      "silence about the unlisted profiles is the false-green risk the caveat exists to close",
    ).toContain("**Every profile NOT on those lists is unclassified, NOT faithful.**");
    for (const n of rest) {
      expect(rateCaveat(), `the README must account for unlisted '${n}'`).toContain(`\`${n}\``);
    }
  });

  it("keeps the README off a screen ladder the product no longer has", () => {
    const max = /pub const SCREEN_SIMULCAST_MAX_LAYERS: usize = (\d+);/.exec(aq());
    expect(max, "SCREEN_SIMULCAST_MAX_LAYERS must be findable").not.toBeNull();
    expect(Number(max![1]), "a screen ladder is back; the README quantifies no screen offer").toBe(
      1,
    );
    expect(
      tierLabels("SCREEN_QUALITY_TIERS"),
      "one rung, so the cap cannot deepen a share",
    ).toEqual(["native"]);
    expect(readme(), "the cap must be stated as camera-only").toMatch(
      /It governs CAMERA depth only/,
    );
    expect(readme(), "a screen-depth claim for the cap is back").not.toMatch(
      /deepens? the screen ladder|screen ladder'?s ceiling|raises the screen ceiling/i,
    );
  });

  it("mirrors screen_bitrate_kbps_for against the Rust suite's own literal pin", () => {
    const m =
      /assert_eq!\(\s*screen_bitrate_kbps_for\((\d+),\s*(\d+),\s*SCREEN_TARGET_FPS\),\s*(\d+)\)/.exec(
        aq(),
      );
    expect(m, `no screen_bitrate_kbps_for literal pin in ${AQ_CONSTANTS_REL}`).not.toBeNull();
    expect(screenKbitFor(Number(m![1]), Number(m![2]), aqInt("SCREEN_TARGET_FPS"))).toBe(
      Number(m![3]),
    );
  });

  it("pins the share's quoted ceiling to screen_bitrate_kbps_for's own inputs", () => {
    const screen = /~(\d+) kbit for a capture fitted to the tier-0 encode box/.exec(readme());
    expect(screen, "the README's screen-ceiling figure must be findable").not.toBeNull();
    expect(Number(screen![1]), "the README's screen ceiling has drifted").toBe(screenCeilingKbit());
    const sharing = /a sharing bot's ~(\d+) kbit is a REFERENCE TOTAL/.exec(readme());
    expect(sharing, "the README's sharing-ceiling figure must be findable").not.toBeNull();
    expect(Number(sharing![1]), "the README's sharing ceiling ≠ camera + audio + screen").toBe(
      sharingCeilingKbit(),
    );
  });

  it("classifies every profile against a sharing bot exactly as the README does", () => {
    const ceiling = sharingCeilingKbit();
    expect(
      clauseProfiles("Cannot carry a SHARING bot's total"),
      `a sharing bot on these under-represents the room's decode load (ceiling ${ceiling} kbit)`,
    ).toEqual(cannotCarry(ceiling));
  });

  it("pins where congested_wifi falls against the camera ladder", () => {
    const camera = cameraCeilingKbit("SIMULCAST_VIDEO_LAYERS");
    const audio = audioCeilingKbit();
    const rate = NETEM_PROFILES.congested_wifi!.rateKbit!;
    expect(
      [rate > camera, rate < camera + audio],
      `congested_wifi (${rate}) clears the camera ladder (${camera}) by under the audio layer (${audio}) — a ladder move must reclassify it`,
    ).toEqual([true, true]);
  });

  it("classifies the profiles that cannot carry even the base rung", () => {
    const base = cameraLadderKbit("SIMULCAST_VIDEO_LAYERS")[0];
    expect(
      cannotCarry(base),
      `these cannot sustain the base rung (${base} kbit), so the bot may publish nothing`,
    ).toEqual(["dialup"]);
  });
});

/** RHS of the sole `let <name> = …;` in host.rs, asserted never rebound. */
function hostLet(name: string): string {
  const src = readFileSync(HOST, "utf8");
  const m = new RegExp(`let (?:mut )?${name} =([\\s\\S]*?);\\n`).exec(src);
  expect(m, `\`let ${name}\` must be findable in ${HOST_REL}`).not.toBeNull();
  expect(
    [...src.matchAll(new RegExp(`(?<![.\\w])${name}\\s*=(?!=)`, "g"))].length,
    `${name} is assigned more than once; this lock reads only its \`let\``,
  ).toBe(1);
  return m![1];
}

describe("BOT_HW_CONCURRENCY vs the audio a bot publishes (#2359)", () => {
  /** The one function the `navigator.hardwareConcurrency` spoof reaches. */
  const SPOOFED = "capability_max_simulcast_layers";
  const CAMERA = /\beffective_max_layers\b/;
  /** The runtime flag, and the RECEIVE ladder depth: since #2279 neither is an audio input. */
  const FLAG = "experimental_simulcast_max_layers";
  const RECEIVE_LADDER = "max_layers_for_kind";
  /** The publisher's own count, which both SEND readouts must read back. */
  const PUBLISHED = "effective_audio_layers()";

  it("keeps the camera cap on the check the spoof feeds", () => {
    expect(
      hostLet("effective_max_layers"),
      "the spoof no longer reaches the camera ladder, so the README's cap table is fiction",
    ).toContain(SPOOFED);
  });

  it("keeps the flag ceiling both ladders share off the spoofed core count", () => {
    expect(hostLet("flag_max_layers").trim()).toBe("experimental_simulcast_max_layers()");
  });

  it("keeps the mic encoder's audio layer count off the flag and the spoofed core count", () => {
    const mic = hostLet("microphone");
    expect(mic, "the publisher no longer takes the pinned audio count").toContain(
      "audio_published_layer_count()",
    );
    // The two SEND readouts derive the depth FROM the publisher, so neither can name
    // a layer count the mic encoder does not emit.
    for (const readout of ["audio_layer_max", "audio_published"]) {
      expect(hostLet(readout), `${readout} stopped reading the publisher's count`).toContain(
        PUBLISHED,
      );
    }
    for (const name of ["microphone", "audio_layer_max", "audio_published"]) {
      const src = hostLet(name);
      const capped = `${name} follows the capability cap, so the README's claim that BOT_HW_CONCURRENCY leaves audio alone is fiction`;
      expect(src, capped).not.toMatch(CAMERA);
      expect(src, capped).not.toContain(SPOOFED);
      expect(src, `${name} derives the audio count from the runtime flag again`).not.toContain(
        FLAG,
      );
      expect(
        src,
        `${name} derives the audio count from the RECEIVE ladder depth again`,
      ).not.toContain(RECEIVE_LADDER);
    }
  });

  it("states in the README that the cap leaves audio alone, naming the real symbol", () => {
    const m =
      /Audio is outside it too — `host\.rs` pins the mic encoder to ONE layer via `(\w+)\(\)`/.exec(
        readme(),
      );
    expect(m, "the README's audio-cap sentence must be findable").not.toBeNull();
    expect(
      hostLet("microphone"),
      `the README names \`${m![1]}\`, which the publisher does not call`,
    ).toContain(`${m![1]}()`);
  });
});

describe("a clock bot's audio term is nominal, not measured (#2359)", () => {
  it("keeps the clock source's audio track at a gain of 0, exclusively", () => {
    const src = readFileSync(CLOCK_SOURCE, "utf8");
    const node = /const (\w+) = audioContext\.createGain\(\);/.exec(src);
    expect(node, "clock-source.js no longer creates a gain node").not.toBeNull();
    const gain = node![1];
    expect(src, `${gain}.gain.value must be 0`).toMatch(
      new RegExp(`${gain}\\.gain\\.value\\s*=\\s*0\\s*;`),
    );
    expect(src, "the muted gain must sit between the oscillator and the captured track").toMatch(
      new RegExp(
        `oscillator\\.connect\\(${gain}\\)[\\s\\S]*?${gain}\\.connect\\(audioDestination\\)`,
      ),
    );
    expect(
      [...src.matchAll(/oscillator\.connect\(/g)].length,
      "a second oscillator route would bypass the muted gain",
    ).toBe(1);
    expect(
      [...src.matchAll(new RegExp(`${gain}\\.gain\\.value\\s*=`, "g"))].length,
      "a later write could unmute the track this lock reads as silent",
    ).toBe(1);
  });

  it("states in the README that a clock bot's audio term is nominal", () => {
    expect(rateCaveat()).toMatch(
      /a clock-mode bot's audio term is nominal because `src\/clock-source\.js` routes its oscillator through a gain of 0/,
    );
  });
});
