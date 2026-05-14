import { describe, it, expect } from "vitest";

import { parseManifestText } from "./manifest";
import {
  emitMeetingConfigYaml,
  generateMeetingConfig,
  NETSIM_PRESETS,
  parseMeetingConfigText,
  seededRng,
  shuffleSeeded,
} from "./meeting-config";

const MANIFEST_FIXTURE = `
participants:
- name: alice
  costume_dir: assets/costumes/pirate
- name: bob
  costume_dir: assets/costumes/bunny
- name: carol
  costume_dir: assets/costumes/cat
- name: dave
  costume_dir: assets/costumes/cowboy
- name: eve
  costume_dir: assets/costumes/cyberspace
pause_ms: 0
lines: []
`;

describe("parseMeetingConfigText", () => {
  it("parses a minimal config", () => {
    const cfg = parseMeetingConfigText(`
meeting_url: https://app.videocall.fnxlabs.com/meeting/Test
bots:
- participant: alice
- participant: bob
`);
    expect(cfg.meetingUrl).toBe("https://app.videocall.fnxlabs.com/meeting/Test");
    expect(cfg.bots).toEqual([{ participant: "alice" }, { participant: "bob" }]);
  });

  it("parses per-bot ttl overrides", () => {
    const cfg = parseMeetingConfigText(`
meeting_url: https://x/y
ttl: 5m
bots:
- participant: alice
  ttl: 30s
- participant: bob
`);
    expect(cfg.ttl).toBe("5m");
    expect(cfg.bots[0].ttl).toBe("30s");
    expect(cfg.bots[1].ttl).toBeUndefined();
  });

  it("preserves meta block", () => {
    const cfg = parseMeetingConfigText(`
meeting_url: https://x/y
bots:
- participant: alice
meta:
  seed: 42
  generated_at: "2026-05-13T12:00:00Z"
`);
    expect(cfg.meta).toEqual({ seed: 42, generatedAt: "2026-05-13T12:00:00Z" });
  });

  it("rejects an empty bots array", () => {
    expect(() => parseMeetingConfigText(`meeting_url: https://x/y\nbots: []`)).toThrow(
      /bots must not be empty/,
    );
  });

  it("rejects a bot entry without participant", () => {
    expect(() => parseMeetingConfigText(`meeting_url: https://x/y\nbots:\n- ttl: 5m`)).toThrow(
      /bots\[0\].participant/,
    );
  });

  it("rejects non-mapping input", () => {
    expect(() => parseMeetingConfigText(`- alice\n- bob`)).toThrow(/not a YAML mapping/);
  });

  it("rejects missing meeting_url", () => {
    expect(() => parseMeetingConfigText(`bots:\n- participant: alice`)).toThrow(
      /meeting_url must be a non-empty string/,
    );
  });

  it("parses a meeting-level network field", () => {
    const cfg = parseMeetingConfigText(`
meeting_url: https://x/y
network: lossy_mobile
bots:
- participant: alice
`);
    expect(cfg.network).toBe("lossy_mobile");
    expect(cfg.bots[0].network).toBeUndefined();
  });

  it("parses a per-bot network field", () => {
    const cfg = parseMeetingConfigText(`
meeting_url: https://x/y
bots:
- participant: alice
  network: satellite
- participant: bob
`);
    expect(cfg.network).toBeUndefined();
    expect(cfg.bots[0].network).toBe("satellite");
    expect(cfg.bots[1].network).toBeUndefined();
  });

  it("accepts network: none (the passthrough sentinel)", () => {
    const cfg = parseMeetingConfigText(`
meeting_url: https://x/y
network: none
bots:
- participant: alice
  network: none
`);
    expect(cfg.network).toBe("none");
    expect(cfg.bots[0].network).toBe("none");
  });

  it("rejects an unknown meeting-level network profile", () => {
    expect(() =>
      parseMeetingConfigText(`
meeting_url: https://x/y
network: nonsense
bots:
- participant: alice
`),
    ).toThrow(/meeting\.network must be one of: .*good_wifi.*satellite.*dialup.*got "nonsense"/s);
  });

  it("rejects an unknown per-bot network profile", () => {
    expect(() =>
      parseMeetingConfigText(`
meeting_url: https://x/y
bots:
- participant: alice
  network: not-a-real-profile
`),
    ).toThrow(/bots\[0\]\.network must be one of: .*got "not-a-real-profile"/s);
  });

  it("rejects a non-string network value", () => {
    expect(() =>
      parseMeetingConfigText(`
meeting_url: https://x/y
network: 42
bots:
- participant: alice
`),
    ).toThrow(/meeting\.network, when present, must be a string/);
  });
});

describe("NETSIM_PRESETS", () => {
  it("matches the Rust source-of-truth list (videocall-netsim/src/profiles.rs PRESET_NAMES)", () => {
    expect([...NETSIM_PRESETS]).toEqual([
      "none",
      "good_wifi",
      "good_4g",
      "congested_wifi",
      "lossy_mobile",
      "satellite",
      "dialup",
    ]);
  });
});

describe("emitMeetingConfigYaml", () => {
  it("round-trips through parse → emit → parse", () => {
    const original = parseMeetingConfigText(`
meeting_url: https://x/y
ttl: 5m
bots:
- participant: alice
  ttl: 30s
- participant: bob
meta:
  seed: 42
  generated_at: "2026-05-13T12:00:00Z"
`);
    const yaml = emitMeetingConfigYaml(original);
    const reparsed = parseMeetingConfigText(yaml);
    expect(reparsed).toEqual(original);
  });

  it("omits empty/undefined fields", () => {
    const yaml = emitMeetingConfigYaml({
      meetingUrl: "https://x/y",
      bots: [{ participant: "alice" }],
    });
    expect(yaml).toContain("meeting_url:");
    expect(yaml).toContain("alice");
    expect(yaml).not.toContain("ttl:");
    expect(yaml).not.toContain("meta:");
    expect(yaml).not.toContain("network:");
  });

  it("round-trips a meeting-level network field through parse → emit → parse", () => {
    const original = parseMeetingConfigText(`
meeting_url: https://x/y
network: congested_wifi
bots:
- participant: alice
`);
    const yaml = emitMeetingConfigYaml(original);
    expect(yaml).toContain("network: congested_wifi");
    const reparsed = parseMeetingConfigText(yaml);
    expect(reparsed.network).toBe("congested_wifi");
    expect(reparsed).toEqual(original);
  });

  it("round-trips a per-bot network field through parse → emit → parse", () => {
    const original = parseMeetingConfigText(`
meeting_url: https://x/y
bots:
- participant: alice
  network: dialup
- participant: bob
  network: good_4g
`);
    const yaml = emitMeetingConfigYaml(original);
    expect(yaml).toContain("network: dialup");
    expect(yaml).toContain("network: good_4g");
    const reparsed = parseMeetingConfigText(yaml);
    expect(reparsed.bots[0].network).toBe("dialup");
    expect(reparsed.bots[1].network).toBe("good_4g");
    expect(reparsed).toEqual(original);
  });
});

describe("seededRng", () => {
  it("is deterministic for a given seed", () => {
    const a = seededRng(42);
    const b = seededRng(42);
    const seq1 = [a(), a(), a(), a(), a()];
    const seq2 = [b(), b(), b(), b(), b()];
    expect(seq1).toEqual(seq2);
  });

  it("produces different sequences for different seeds", () => {
    const a = seededRng(42);
    const b = seededRng(43);
    const seq1 = [a(), a(), a()];
    const seq2 = [b(), b(), b()];
    expect(seq1).not.toEqual(seq2);
  });

  it("produces values in [0, 1)", () => {
    const rng = seededRng(7);
    for (let i = 0; i < 100; i++) {
      const v = rng();
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThan(1);
    }
  });
});

describe("shuffleSeeded", () => {
  it("is deterministic with the same seed", () => {
    const items = ["a", "b", "c", "d", "e"];
    const out1 = shuffleSeeded(items, seededRng(42));
    const out2 = shuffleSeeded(items, seededRng(42));
    expect(out1).toEqual(out2);
  });

  it("preserves the set of items (just reorders)", () => {
    const items = ["a", "b", "c", "d", "e"];
    const out = shuffleSeeded(items, seededRng(42));
    expect([...out].sort()).toEqual([...items].sort());
  });

  it("doesn't mutate the input", () => {
    const items = ["a", "b", "c"];
    const copy = [...items];
    shuffleSeeded(items, seededRng(1));
    expect(items).toEqual(copy);
  });
});

describe("generateMeetingConfig", () => {
  it("is deterministic for a given seed", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    const a = generateMeetingConfig({
      manifest: m,
      count: 3,
      seed: 42,
      meetingUrl: "https://x/y",
      ttl: "5m",
    });
    const b = generateMeetingConfig({
      manifest: m,
      count: 3,
      seed: 42,
      meetingUrl: "https://x/y",
      ttl: "5m",
    });
    expect(a.bots).toEqual(b.bots);
  });

  it("picks `count` unique participants from the manifest", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    const cfg = generateMeetingConfig({
      manifest: m,
      count: 3,
      seed: 1,
      meetingUrl: "https://x/y",
    });
    expect(cfg.bots.length).toBe(3);
    const names = cfg.bots.map((b) => b.participant);
    expect(new Set(names).size).toBe(3); // no duplicates
    for (const n of names) {
      expect(["alice", "bob", "carol", "dave", "eve"]).toContain(n);
    }
  });

  it("rejects count > manifest size", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    expect(() =>
      generateMeetingConfig({ manifest: m, count: 99, seed: 1, meetingUrl: "https://x/y" }),
    ).toThrow(/exceeds the manifest's 5 costumed participants/);
  });

  it("excludes participants without a costume_dir by default", () => {
    const m = parseManifestText(`
participants:
- name: alice
  costume_dir: assets/costumes/pirate
- name: bob
  costume_dir: assets/costumes/bunny
- name: tina
- name: observer-01
- name: observer-02
pause_ms: 0
lines: []
`);
    // 5 manifest entries, only 2 with costume_dir → count=3 must fail.
    expect(() =>
      generateMeetingConfig({ manifest: m, count: 3, seed: 1, meetingUrl: "https://x/y" }),
    ).toThrow(/exceeds the manifest's 2 costumed participants/);

    // count=2 works and excludes tina + observers.
    const cfg = generateMeetingConfig({
      manifest: m,
      count: 2,
      seed: 1,
      meetingUrl: "https://x/y",
    });
    const names = cfg.bots.map((b) => b.participant);
    expect([...names].sort()).toEqual(["alice", "bob"]);
  });

  it("includes observers when includeObservers is true", () => {
    const m = parseManifestText(`
participants:
- name: alice
  costume_dir: assets/costumes/pirate
- name: tina
- name: observer-01
- name: observer-02
pause_ms: 0
lines: []
`);
    const cfg = generateMeetingConfig({
      manifest: m,
      count: 4,
      seed: 1,
      meetingUrl: "https://x/y",
      includeObservers: true,
    });
    const names = cfg.bots.map((b) => b.participant);
    expect([...names].sort()).toEqual(["alice", "observer-01", "observer-02", "tina"]);
  });

  it("rejects count <= 0", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    expect(() =>
      generateMeetingConfig({ manifest: m, count: 0, seed: 1, meetingUrl: "https://x/y" }),
    ).toThrow(/positive integer/);
  });

  it("populates meta.seed for reproducibility", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    const cfg = generateMeetingConfig({
      manifest: m,
      count: 2,
      seed: 42,
      meetingUrl: "https://x/y",
    });
    expect(cfg.meta?.seed).toBe(42);
    expect(cfg.meta?.generatedAt).toBeDefined();
  });

  it("populates the meeting-level network field when supplied", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    const cfg = generateMeetingConfig({
      manifest: m,
      count: 2,
      seed: 42,
      meetingUrl: "https://x/y",
      network: "lossy_mobile",
    });
    expect(cfg.network).toBe("lossy_mobile");
    // Per-bot networks are NOT randomized today.
    for (const bot of cfg.bots) {
      expect(bot.network).toBeUndefined();
    }
  });

  it("rejects an unknown network profile", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    expect(() =>
      generateMeetingConfig({
        manifest: m,
        count: 2,
        seed: 42,
        meetingUrl: "https://x/y",
        network: "nonsense",
      }),
    ).toThrow(/meeting\.network must be one of: .*got "nonsense"/s);
  });

  it("omits the network field when not supplied", () => {
    const m = parseManifestText(MANIFEST_FIXTURE);
    const cfg = generateMeetingConfig({
      manifest: m,
      count: 2,
      seed: 42,
      meetingUrl: "https://x/y",
    });
    expect(cfg.network).toBeUndefined();
  });
});
