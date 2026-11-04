import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { afterEach, describe, expect, it } from "vitest";

import { NETEM_PROFILES } from "./control/netem";
import { DECODE_BUDGET_WAIVER_NOTE } from "./decode-budget";
import { SKIP_CANVAS_PAINT_WAIVER_NOTE } from "./receiver-caps";
import { SPEAKING_GLOW_WAIVER_NOTE } from "./speaking-glow";
import type { BotTask } from "./orchestrator";
import { SD_SOURCE } from "./posture";
import { SPEECH_PLAN_DEFAULT } from "./speech-plan";
import {
  assignPersonas,
  createPersonaPlanner,
  personaFor,
  resolvePersonaSettings,
} from "./persona";
import type { PersonaTally } from "./persona-driver";
import {
  NODE_NAME_ENV,
  PARTICIPANT_RECORD_DIR,
  ParticipantRecorder,
  type PodContext,
  readPodContext,
  recordedNetwork,
  resolveJoinMedia,
  resolveRole,
} from "./run-record";

const POD: PodContext = {
  ordinal: null,
  node: null,
  staggerMs: null,
  staggerIncomplete: false,
  imageRevision: "0123abc",
};

const JWT_NOTE = {
  field: "user_id",
  reason: "the JWT subject; not checked against a relay label, which must carry it verbatim",
};

const dirs: string[] = [];
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

function runDir(): string {
  const d = mkdtempSync(join(tmpdir(), "bots-record-"));
  dirs.push(d);
  return d;
}

function task(overrides: Partial<BotTask> = {}): BotTask {
  return {
    botId: "11111111-2222-3333-4444-555555555555",
    meetingURL: "https://example.test/meeting/R",
    participant: "bot-3",
    displayName: "bot-3",
    headless: true,
    authBackend: "jwt",
    sourceGeometry: SD_SOURCE,
    cameraCycle: null,
    ttl: 60_000,
    network: null,
    ...overrides,
  };
}

function readRecord(dir: string, botId: string): Record<string, unknown> {
  return JSON.parse(readFileSync(join(dir, PARTICIPANT_RECORD_DIR, `${botId}.json`), "utf8"));
}

describe("recordedNetwork", () => {
  it("records a verified startup netem profile with both directions and its params", () => {
    expect(recordedNetwork(null, "lossy_mobile")).toEqual({
      network: {
        profile: "lossy_mobile",
        shaped: true,
        direction: "both",
        shaper: "netem",
        params: NETEM_PROFILES.lossy_mobile,
      },
      unverified: null,
    });
  });

  it.each(["none", "clean"])("records no shaping for kernel %j", (kernel) => {
    expect(recordedNetwork(null, kernel)).toEqual({
      network: { profile: "none", shaped: false, direction: "none", shaper: "none", params: {} },
      unverified: null,
    });
  });

  it.each([undefined, ""])(
    "records an unchecked link, not an unshaped one, for kernel %j",
    (kernel) => {
      const r = recordedNetwork(null, kernel);
      expect(r.network).toMatchObject({ profile: "unknown", shaped: null });
      expect(r.unverified).toContain("BOT_NETEM_APPLIED unset");
      expect(recordedNetwork("lossy_mobile", kernel).unverified).toContain(
        "the kernel qdisc was not checked",
      );
    },
  );

  it("never reports an inherited or unreadable qdisc as unshaped", () => {
    const r = recordedNetwork(null, "unknown");
    expect(r.network).toMatchObject({ profile: "unknown", shaped: null, direction: null });
    expect(r.unverified).toContain("BOT_NETEM_APPLIED=unknown");
  });

  it("records netsim as uplink-only and unverified", () => {
    const r = recordedNetwork("lossy_mobile", undefined);
    expect(r.network).toMatchObject({ shaped: true, direction: "egress", shaper: "netsim" });
    expect(r.unverified).toContain("--features netsim");
  });

  it("flags netsim stacked on kernel shaping instead of dropping either", () => {
    const r = recordedNetwork("dialup", "good_4g");
    expect(r.network).toMatchObject({ profile: "good_4g", shaper: "netem" });
    expect(r.unverified).toContain("netsim uplink dialup");
  });

  it("keeps a checked-but-unknown qdisc unknown when netsim is also requested", () => {
    const r = recordedNetwork("dialup", "unknown");
    expect(r.network).toMatchObject({ profile: "unknown", shaped: null, shaper: null });
    expect(r.unverified).toContain("BOT_NETEM_APPLIED=unknown");
    expect(r.unverified).toContain("netsim uplink dialup also requested");
  });
});

describe("resolveJoinMedia / resolveRole", () => {
  it.each([
    [{}, {}, { camera: true, mic: true }],
    [{ cameraOffFlag: true }, {}, { camera: false, mic: true }],
    [{}, { BOT_JOIN_MIC_MUTED: "1" }, { camera: true, mic: false }],
    [
      {},
      { BOT_JOIN_CAMERA_OFF: "TRUE", BOT_JOIN_MIC_MUTED: "false" },
      { camera: false, mic: true },
    ],
  ])("flags %j env %j -> %j", (flags, env, value) => {
    expect(resolveJoinMedia({ ...flags, env, cameraCycle: false })).toEqual({ kind: "ok", value });
  });

  it("rejects a malformed env value and a camera-off join with a duty cycle", () => {
    expect(resolveJoinMedia({ env: { BOT_JOIN_CAMERA_OFF: "yes" }, cameraCycle: false }).kind).toBe(
      "invalid",
    );
    expect(resolveJoinMedia({ cameraOffFlag: true, env: {}, cameraCycle: true }).kind).toBe(
      "invalid",
    );
    expect(resolveJoinMedia({ micMutedFlag: true, env: {}, cameraCycle: true }).kind).toBe("ok");
  });

  it("accepts a plain role, treats blank as unset, and rejects anything else", () => {
    expect(resolveRole("probe-mix")).toEqual({ kind: "ok", value: "probe-mix" });
    expect(resolveRole(" ")).toEqual({ kind: "ok", value: null });
    expect(resolveRole("probe\nmix").kind).toBe("invalid");
  });
});

describe("ParticipantRecorder", () => {
  it("writes the manifest participant fields at register, join and finish", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({
      runDir: dir,
      role: "probe-mix",
      kernelNetem: "none",
      pod: POD,
    });
    const t = task({ joinMedia: { camera: false, mic: false } });

    rec.register(t);
    expect(readRecord(dir, t.botId)).toMatchObject({
      schema: "bots-app-participant-record/v0",
      bot_id: t.botId,
      join_media_requested: { camera: false, mic: false },
      outcome: null,
      unverified: [JWT_NOTE],
      participant: {
        user_id: "bot-3@bots-app.local",
        fleet: "browser",
        role: "probe-mix",
        observer: true,
        talker: false,
        publishes: null,
        transport_intended: "auto",
        join_ts: null,
        leave_ts: null,
      },
    });

    rec.joined(t.botId, 1_759_201_005_250, { camera: false, mic: false }, null);
    rec.rejoining(t.botId, null);
    rec.joined(t.botId, 1_759_201_999_000, { camera: true, mic: true }, null);
    rec.finished(t.botId, 1_759_201_420_000, "shutdown-signal");
    const done = readRecord(dir, t.botId) as {
      outcome: string;
      unverified: unknown[];
      participant: Record<string, unknown>;
    };
    expect(done.outcome).toBe("shutdown-signal");
    expect(done.unverified).toEqual([
      JWT_NOTE,
      {
        field: "rejoin",
        reason:
          "the bot left and relaunched for a ctl network change: join_ts..leave_ts spans that gap, publishes describes the first join, and so does network until a POST /netem re-stamps it",
      },
    ]);
    expect(done.participant).toMatchObject({
      publishes: { camera: false, mic: false, screen: false },
      join_ts: 1_759_201_005.25,
      leave_ts: 1_759_201_420,
    });
    expect(readdirSync(join(dir, PARTICIPANT_RECORD_DIR))).toEqual([`${t.botId}.json`]);
  });

  it("marks a rejoin even when the relaunch never joins again", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    rec.register(task());
    rec.joined(task().botId, 1_000, { camera: true, mic: true }, null);
    rec.rejoining(task().botId, null);
    rec.finished(task().botId, 9_000, "launch-failed");
    const r = readRecord(dir, task().botId) as { unverified: Array<{ field: string }> };
    expect(r.unverified.map((u) => u.field)).toEqual(["user_id", "rejoin"]);
  });

  it("keeps a never-joined bot's timestamps null and names its outcome", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({
      runDir: dir,
      role: null,
      kernelNetem: undefined,
      pod: POD,
    });
    rec.register(task());
    rec.finished(task().botId, 1_000, "meeting-rejected:rejected");
    expect(readRecord(dir, task().botId)).toMatchObject({
      outcome: "meeting-rejected:rejected",
      participant: { join_ts: null, leave_ts: null, publishes: null },
    });
  });

  it("marks media it could not observe, or that contradicts the request, as unverified", () => {
    const rec = new ParticipantRecorder({
      runDir: runDir(),
      role: null,
      kernelNetem: undefined,
      pod: POD,
    });
    rec.register(task());
    rec.joined(task().botId, 1_000, { camera: null, mic: false }, null);
    expect(rec.snapshot(task().botId)?.unverified.map((u) => u.field)).toEqual([
      "network",
      "user_id",
      "publishes.camera",
      "publishes.mic",
    ]);
  });

  it("leaves user_id null outside jwt auth and says why", () => {
    const rec = new ParticipantRecorder({
      runDir: runDir(),
      role: null,
      kernelNetem: undefined,
      pod: POD,
    });
    rec.register(task({ authBackend: "form-login" }));
    const snap = rec.snapshot(task().botId);
    expect(snap?.participant.user_id).toBeNull();
    expect(snap?.unverified).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          field: "user_id",
          reason: expect.stringContaining("form-login"),
        }),
      ]),
    );
  });

  it("records the session user_id the join response named, rewriting the file", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    const form = task({ authBackend: "form-login" });
    rec.register(form);
    rec.joined(form.botId, 1_000, { camera: true, mic: true }, "bot-7@labs.example");
    const r = readRecord(dir, form.botId) as {
      participant: { user_id: string | null };
      unverified: Array<{ field: string; reason: string }>;
    };
    expect(r.participant.user_id).toBe("bot-7@labs.example");
    expect(r.unverified.filter((u) => u.field === "user_id")).toEqual([
      {
        field: "user_id",
        reason:
          "read from the meeting-api join response the client connects with; not checked against a relay label",
      },
    ]);

    const jwt = task({ botId: "99999999-2222-3333-4444-555555555555" });
    rec.register(jwt);
    rec.joined(jwt.botId, 1_000, null, "someone-else@example.test");
    expect(rec.snapshot(jwt.botId)?.participant.user_id).toBe("someone-else@example.test");
    expect(rec.snapshot(jwt.botId)?.unverified).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          field: "user_id",
          reason: expect.stringContaining("not the JWT subject"),
        }),
      ]),
    );
  });

  it("keeps user_id null and flagged when no join response named the session", () => {
    const rec = new ParticipantRecorder({
      runDir: runDir(),
      role: null,
      kernelNetem: "none",
      pod: POD,
    });
    rec.register(task({ authBackend: "form-login" }));
    rec.joined(task().botId, 1_000, null, null);
    const snap = rec.snapshot(task().botId);
    expect(snap?.participant.user_id).toBeNull();
    expect(snap?.unverified.filter((u) => u.field === "user_id")).toHaveLength(1);
  });

  it("warns instead of throwing when the record cannot be written", () => {
    const warnings: string[] = [];
    const rec = new ParticipantRecorder({
      runDir: runDir(),
      role: null,
      kernelNetem: undefined,
      pod: POD,
      write: () => {
        throw new Error("EROFS");
      },
      warn: (m) => warnings.push(m),
    });
    expect(() => rec.register(task())).not.toThrow();
    expect(warnings[0]).toContain("EROFS");
  });
});

describe("pod context in the participant record (#2358)", () => {
  it("reads ordinal, node, drawn stagger and image revision from the session env", () => {
    expect(
      readPodContext({
        BOT_POD_ORDINAL: "3",
        [NODE_NAME_ENV]: "node-a",
        BOT_JOIN_STAGGER_MS: "42000",
        BOT_JOIN_STAGGER_INCOMPLETE: "1",
        BOTS_IMAGE_REVISION: "deadbeef",
      }),
    ).toEqual({
      ordinal: 3,
      node: "node-a",
      staggerMs: 42_000,
      staggerIncomplete: true,
      imageRevision: "deadbeef",
    });
    expect(
      readPodContext({ BOT_POD_ORDINAL: "", BOT_JOIN_STAGGER_MS: "-5", [NODE_NAME_ENV]: " " }),
    ).toEqual({
      ordinal: null,
      node: null,
      staggerMs: null,
      staggerIncomplete: false,
      imageRevision: null,
    });
  });

  it("writes placement, stagger_ms and the image revision, and flags what it cannot vouch for", () => {
    const dir = runDir();
    const pod = {
      ordinal: 0,
      node: "node-a",
      staggerMs: 0,
      staggerIncomplete: true,
      imageRevision: null,
    };
    new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod }).register(task());
    const r = readRecord(dir, task().botId) as {
      code: unknown;
      unverified: Array<{ field: string }>;
      participant: Record<string, unknown>;
    };
    expect(r.participant).toMatchObject({
      placement: { ordinal: 0, node: "node-a" },
      stagger_ms: 0,
    });
    expect(r.code).toEqual({ images: { "bots-app": null } });
    expect(r.unverified.map((u) => u.field)).toEqual([
      "stagger_ms",
      "code.images.bots-app",
      "user_id",
    ]);

    const stamped = runDir();
    new ParticipantRecorder({
      runDir: stamped,
      role: null,
      kernelNetem: "none",
      pod: { ...POD, imageRevision: "unknown" },
    }).register(task());
    expect(
      (
        readRecord(stamped, task().botId) as { unverified: Array<{ field: string }> }
      ).unverified.map((u) => u.field),
    ).toContain("code.images.bots-app");

    const empty = runDir();
    new ParticipantRecorder({
      runDir: empty,
      role: null,
      kernelNetem: "none",
      pod: { ...POD, imageRevision: "" },
    }).register(task());
    expect(
      (readRecord(empty, task().botId) as { unverified: Array<{ field: string }> }).unverified.map(
        (u) => u.field,
      ),
    ).toContain("code.images.bots-app");

    const dirty = runDir();
    new ParticipantRecorder({
      runDir: dirty,
      role: null,
      kernelNetem: "none",
      pod: { ...POD, imageRevision: "0123abc-dirty" },
    }).register(task());
    expect(
      (readRecord(dirty, task().botId) as { unverified: Array<{ field: string }> }).unverified,
    ).toContainEqual({
      field: "code.images.bots-app",
      reason: "the image was built from an uncommitted tree",
    });

    const local = runDir();
    new ParticipantRecorder({
      runDir: local,
      role: null,
      kernelNetem: undefined,
      pod: POD,
    }).register(task());
    const l = readRecord(local, task().botId) as {
      code: unknown;
      participant: Record<string, unknown>;
    };
    expect(l.participant).toMatchObject({ placement: null, stagger_ms: null });
    expect(l.code).toEqual({ images: { "bots-app": "0123abc" } });
  });

  it("re-stamps network on a runtime POST /netem for running bots only", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({
      runDir: dir,
      role: null,
      kernelNetem: "good_4g",
      pod: POD,
    });
    const done = task({ botId: "99999999-2222-3333-4444-555555555555" });
    rec.register(task());
    rec.register(done);
    rec.finished(done.botId, 1_000, "ctl-leave");
    const params = NETEM_PROFILES.lossy_mobile!;
    rec.netemApplied({ op: "shape", label: "lossy_mobile", params }, Date.UTC(2026, 9, 1));
    const r = readRecord(dir, task().botId) as {
      unverified: Array<{ field: string; reason: string }>;
      participant: { network: unknown };
    };
    expect(r.participant.network).toEqual({
      profile: "lossy_mobile",
      shaped: true,
      direction: "both",
      shaper: "netem",
      params,
    });
    expect(r.unverified.filter((u) => u.field !== "user_id")).toEqual([
      {
        field: "network",
        reason:
          "set by POST /netem at 2026-10-01T00:00:00.000Z; the shaping params are not read back",
      },
    ]);
    expect(
      (readRecord(dir, done.botId) as { participant: { network: { profile: string } } }).participant
        .network.profile,
    ).toBe("good_4g");

    rec.netemApplied({ op: "clear", label: "clear" }, Date.UTC(2026, 9, 1, 0, 1));
    const c = readRecord(dir, task().botId) as {
      unverified: unknown[];
      participant: { network: unknown };
    };
    expect(c.participant.network).toMatchObject({ profile: "none", shaped: false });
    expect(c.unverified).toHaveLength(2);

    rec.netemApplied({ op: "clear", label: "clear" }, Date.UTC(2026, 9, 1, 0, 2), true);
    const f = readRecord(dir, task().botId) as {
      unverified: Array<{ field: string; reason: string }>;
      participant: { network: unknown };
    };
    expect(f.participant.network).toMatchObject({ profile: "unknown", shaped: null });
    expect(f.unverified.filter((u) => u.field !== "user_id")).toEqual([
      {
        field: "network",
        reason:
          "POST /netem clear failed partway at 2026-10-01T00:02:00.000Z; the kernel state is unknown",
      },
    ]);
  });

  it("records a raw POST /netem without the downlink pair as egress only", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    rec.register(task());
    rec.netemApplied(
      { op: "shape", label: "custom", params: { delayMs: 100 } },
      Date.UTC(2026, 9, 1),
    );
    const r = readRecord(dir, task().botId) as { participant: { network: unknown } };
    expect(r.participant.network).toMatchObject({ profile: "custom", direction: "egress" });
  });

  it("gives a bot registered after POST /netem the netem network, flagged", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    const params = NETEM_PROFILES.lossy_mobile!;
    rec.netemApplied({ op: "shape", label: "lossy_mobile", params }, Date.UTC(2026, 9, 1));
    rec.register(task());
    const r = readRecord(dir, task().botId) as {
      unverified: Array<{ field: string; reason: string }>;
      participant: { network: unknown };
    };
    expect(r.participant.network).toMatchObject({ profile: "lossy_mobile", direction: "both" });
    expect(r.unverified.filter((u) => u.field === "network")).toEqual([
      {
        field: "network",
        reason:
          "set by POST /netem at 2026-10-01T00:00:00.000Z; the shaping params are not read back",
      },
    ]);
  });

  it("keeps a netsim bot's uplink in the record across POST /netem shape and clear", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    const netsimTask = task({ network: "congested_wifi" });
    rec.register(netsimTask);
    const params = NETEM_PROFILES.lossy_mobile!;
    rec.netemApplied({ op: "shape", label: "lossy_mobile", params }, Date.UTC(2026, 9, 1));
    type Rec = {
      unverified: Array<{ field: string; reason: string }>;
      participant: { network: { profile: string; shaper: string | null } };
    };
    const s = readRecord(dir, netsimTask.botId) as Rec;
    expect(s.participant.network).toMatchObject({ profile: "lossy_mobile", shaper: "netem" });
    expect(s.unverified.find((u) => u.field === "network")?.reason).toContain(
      "netsim uplink congested_wifi also requested on top of it",
    );

    rec.netemApplied({ op: "clear", label: "clear" }, Date.UTC(2026, 9, 1, 0, 1));
    const c = readRecord(dir, netsimTask.botId) as Rec;
    expect(c.participant.network).toMatchObject({ profile: "congested_wifi", shaper: "netsim" });
    expect(c.unverified.filter((u) => u.field === "network")).toHaveLength(1);

    const late = task({ botId: "99999999-2222-3333-4444-555555555555", network: "dialup" });
    rec.register(late);
    const l = readRecord(dir, late.botId) as Rec;
    expect(l.participant.network).toMatchObject({ profile: "dialup", shaper: "netsim" });
  });

  it("records the netsim preset none as no shaping, at register and after POST /netem clear", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    const t = task({ network: "none" });
    type Rec = { unverified: Array<{ field: string }>; participant: { network: unknown } };
    rec.register(t);
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.participant.network).toMatchObject({ profile: "none", shaped: false, shaper: "none" });
    expect(r.unverified.map((u) => u.field)).not.toContain("network");

    rec.netemApplied({ op: "clear", label: "clear" }, Date.UTC(2026, 9, 1));
    const c = readRecord(dir, t.botId) as Rec;
    expect(c.participant.network).toMatchObject({ profile: "none", shaped: false, shaper: "none" });
  });

  it("re-stamps with the relaunch's netsim, not the first join's, after a ctl network change", () => {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    const a = task({ network: "congested_wifi" });
    const b = task({ botId: "99999999-2222-3333-4444-555555555555", network: "congested_wifi" });
    type Rec = { participant: { network: unknown } };
    for (const t of [a, b]) {
      rec.register(t);
      rec.joined(t.botId, 1_000, { camera: true, mic: true }, null);
    }
    rec.rejoining(a.botId, "none");
    rec.rejoining(b.botId, "dialup");
    rec.netemApplied({ op: "clear", label: "clear" }, Date.UTC(2026, 9, 1));
    expect((readRecord(dir, a.botId) as Rec).participant.network).toMatchObject({
      profile: "none",
      shaped: false,
    });
    expect((readRecord(dir, b.botId) as Rec).participant.network).toMatchObject({
      profile: "dialup",
      shaper: "netsim",
    });
  });

  it("flags publishes.camera for a bot with a camera duty cycle", () => {
    const rec = new ParticipantRecorder({
      runDir: runDir(),
      role: null,
      kernelNetem: "none",
      pod: POD,
    });
    rec.register(
      task({ cameraCycle: { onMinMs: 1_000, onMaxMs: 2_000, offMinMs: 1_000, offMaxMs: 2_000 } }),
    );
    expect(rec.snapshot(task().botId)?.unverified.map((u) => u.field)).toEqual([
      "user_id",
      "publishes.camera",
    ]);
  });

  it("the StatefulSet injects the node name the record reads, from the downward API", () => {
    const doc = parseYaml(
      readFileSync(resolve(import.meta.dirname, "..", "k8s", "statefulset.yaml"), "utf8"),
    ) as {
      spec: { template: { spec: { containers: Array<{ env?: Array<Record<string, unknown>> }> } } };
    };
    const env = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []);
    expect(env.find((e) => e.name === NODE_NAME_ENV)).toEqual({
      name: NODE_NAME_ENV,
      valueFrom: { fieldRef: { fieldPath: "spec.nodeName" } },
    });
  });
});

describe("diagnostics_packets in the participant record (#2970)", () => {
  type Rec = {
    diagnostics_packets: { requested: string; observed: string | null; source: string | null };
    unverified: Array<{ field: string; reason: string }>;
  };
  const diagEntries = (r: Rec): string[] =>
    r.unverified.filter((u) => u.field === "diagnostics_packets").map((u) => u.reason);

  function recorder(): { dir: string; rec: ParticipantRecorder } {
    const dir = runDir();
    return {
      dir,
      rec: new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }),
    };
  }

  it("stays unverified through join and finish when no line arrives", () => {
    const { dir, rec } = recorder();
    const t = task({ diagPackets: "off" });
    rec.register(t);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null);
    rec.finished(t.botId, 9_000, "ttl-expired");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.diagnostics_packets).toEqual({ requested: "off", observed: null, source: null });
    expect(diagEntries(r)).toEqual([
      "no 'diagnostics packets:' console line: the client predates #2970, logLevel is above info, or the bot never mounted the meeting",
    ]);
  });

  it("clears the entry once the client reports DISABLED", () => {
    const { dir, rec } = recorder();
    const t = task({ diagPackets: "off" });
    rec.register(t);
    rec.diagPacketsObserved(t.botId, { state: "DISABLED", source: "config" });
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.diagnostics_packets).toEqual({
      requested: "off",
      observed: "DISABLED",
      source: "config",
    });
    expect(diagEntries(r)).toEqual([]);
  });

  it("flags an off request the client did not honour", () => {
    const { dir, rec } = recorder();
    const t = task({ diagPackets: "off" });
    rec.register(t);
    rec.diagPacketsObserved(t.botId, { state: "ENABLED", source: "default" });
    expect(diagEntries(readRecord(dir, t.botId) as Rec)).toEqual([
      "requested off but the client logged ENABLED (source=default): this bot still sends DIAGNOSTICS",
    ]);
  });

  it.each([
    ["ENABLED", []],
    [
      "DISABLED",
      ["requested on but the client logged DISABLED (source=url): this bot sends no DIAGNOSTICS"],
    ],
  ] as const)("an on request that logs %s", (state, entries) => {
    const { dir, rec } = recorder();
    const t = task({ diagPackets: "on" });
    rec.register(t);
    rec.diagPacketsObserved(t.botId, { state, source: "url" });
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.diagnostics_packets).toEqual({ requested: "on", observed: state, source: "url" });
    expect(diagEntries(r)).toEqual(entries);
  });

  it("makes a ctl rejoin re-verify", () => {
    const { dir, rec } = recorder();
    const t = task({ diagPackets: "off" });
    rec.register(t);
    rec.diagPacketsObserved(t.botId, { state: "DISABLED", source: "config" });
    rec.rejoining(t.botId, "dialup");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.diagnostics_packets).toEqual({ requested: "off", observed: null, source: null });
    expect(diagEntries(r)).toHaveLength(1);
  });

  it("records a default bot's observation without an entry", () => {
    const { dir, rec } = recorder();
    rec.register(task());
    rec.diagPacketsObserved(task().botId, { state: "ENABLED", source: "default" });
    const r = readRecord(dir, task().botId) as Rec;
    expect(r.diagnostics_packets).toEqual({
      requested: "default",
      observed: "ENABLED",
      source: "default",
    });
    expect(diagEntries(r)).toEqual([]);
  });
});

describe("decode_budget in the participant record (#2914)", () => {
  type Rec = {
    decode_budget: { requested: string; observed: string | null };
    unverified: Array<{ field: string; reason: string }>;
  };
  const entries = (r: Rec): string[] =>
    r.unverified.filter((u) => u.field === "decode_budget").map((u) => u.reason);
  const PENDING = `${DECODE_BUDGET_WAIVER_NOTE} requested but vc_decode_budget_override was not read back after the latest join: that join has not happened yet, or the launch failed`;
  const WAIVED = `${DECODE_BUDGET_WAIVER_NOTE}: vc_decode_budget_override=all read back after join, which the client parses as DecodeBudgetOverride::All`;

  function recorder(): { dir: string; rec: ParticipantRecorder } {
    const dir = runDir();
    return {
      dir,
      rec: new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }),
    };
  }

  it.each([null, "auto"])("records a client bot reading %j without an entry", (readback) => {
    const { dir, rec } = recorder();
    rec.register(task());
    expect(entries(readRecord(dir, task().botId) as Rec)).toEqual([]);
    rec.joined(task().botId, 1_000, { camera: true, mic: true }, null, readback);
    const r = readRecord(dir, task().botId) as Rec;
    expect(r.decode_budget).toEqual({ requested: "client", observed: readback });
    expect(entries(r)).toEqual([]);
  });

  it("flags a client bot whose replayed storage carried an override", () => {
    const { dir, rec } = recorder();
    rec.register(task());
    rec.joined(task().botId, 1_000, { camera: true, mic: true }, null, "4");
    expect(entries(readRecord(dir, task().botId) as Rec)).toEqual([
      "client requested but vc_decode_budget_override=4 was in storage after join (a captured storage state?), so this bot ran under that override",
    ]);
  });

  it("flags an off bot as pending until join reads the override back", () => {
    const { dir, rec } = recorder();
    const t = task({ decodeBudget: "off" });
    rec.register(t);
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([PENDING]);
    rec.finished(t.botId, 9_000, "launch-error");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.decode_budget).toEqual({ requested: "off", observed: null });
    expect(entries(r)).toEqual([PENDING]);
  });

  it("records the waiver once join reads it back", () => {
    const { dir, rec } = recorder();
    const t = task({ decodeBudget: "off" });
    rec.register(t);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, "all");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.decode_budget).toEqual({ requested: "off", observed: "all" });
    expect(entries(r)).toEqual([WAIVED]);
  });

  it("re-verifies across a ctl rejoin, including the second join's readback", () => {
    const { dir, rec } = recorder();
    const t = task({ decodeBudget: "off" });
    rec.register(t);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, "all");
    rec.rejoining(t.botId, "dialup");
    expect(readRecord(dir, t.botId) as Rec).toMatchObject({
      decode_budget: { requested: "off", observed: null },
    });
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([PENDING]);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, "all");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.decode_budget.observed).toBe("all");
    expect(entries(r)).toEqual([WAIVED]);
  });
});

describe("speaking_glow in the participant record (#2914)", () => {
  type Rec = {
    speaking_glow: { requested: string; observed: string | null };
    unverified: Array<{ field: string; reason: string }>;
  };
  const entries = (r: Rec): string[] =>
    r.unverified.filter((u) => u.field === "speaking_glow").map((u) => u.reason);
  const PENDING = `${SPEAKING_GLOW_WAIVER_NOTE} requested but vc_appearance_glow_enabled was not read back after the latest join: that join has not happened yet, or the launch failed`;
  const WAIVED = `${SPEAKING_GLOW_WAIVER_NOTE}: vc_appearance_glow_enabled=false read back after join`;

  function recorder(): { dir: string; rec: ParticipantRecorder } {
    const dir = runDir();
    return {
      dir,
      rec: new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }),
    };
  }
  const join = (rec: ParticipantRecorder, botId: string, glow: string | null): void =>
    rec.joined(botId, 1_000, { camera: true, mic: true }, null, null, null, glow);

  it.each([null, "true"])(
    "records a default bot reading %j as the client's glow, no entry",
    (readback) => {
      const { dir, rec } = recorder();
      rec.register(task());
      expect(entries(readRecord(dir, task().botId) as Rec)).toEqual([]);
      join(rec, task().botId, readback);
      const r = readRecord(dir, task().botId) as Rec;
      expect(r.speaking_glow).toEqual({ requested: "client", observed: readback });
      expect(entries(r)).toEqual([]);
    },
  );

  it("flags a client bot whose replayed storage turned the glow off", () => {
    const { dir, rec } = recorder();
    rec.register(task());
    join(rec, task().botId, "false");
    expect(entries(readRecord(dir, task().botId) as Rec)).toEqual([
      "client requested but vc_appearance_glow_enabled=false was in storage after join (a captured storage state?), so this bot ran with the glow off",
    ]);
  });

  it("flags an off bot as pending until join reads the preference back", () => {
    const { dir, rec } = recorder();
    const t = task({ speakingGlow: "off" });
    rec.register(t);
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([PENDING]);
    rec.finished(t.botId, 9_000, "launch-error");
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([PENDING]);
  });

  it("records the waiver once join reads it back", () => {
    const { dir, rec } = recorder();
    const t = task({ speakingGlow: "off" });
    rec.register(t);
    join(rec, t.botId, "false");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.speaking_glow).toEqual({ requested: "off", observed: "false" });
    expect(entries(r)).toEqual([WAIVED]);
  });

  it("re-verifies across a ctl rejoin", () => {
    const { dir, rec } = recorder();
    const t = task({ speakingGlow: "off" });
    rec.register(t);
    join(rec, t.botId, "false");
    rec.rejoining(t.botId, "dialup");
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([PENDING]);
    join(rec, t.botId, "false");
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([WAIVED]);
  });
});

describe("video_noise in the participant record (issue 3089)", () => {
  function registered(overrides: Partial<BotTask>): {
    video_noise: unknown;
    unverified: Array<{ field: string }>;
  } {
    const dir = runDir();
    new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }).register(
      task(overrides),
    );
    return readRecord(dir, task().botId) as never;
  }

  it("records the SD default for a clock bot without an entry", () => {
    const r = registered({ videoMode: "clock" });
    expect(r.video_noise).toEqual({ requested: null, alpha: 0.1 });
    expect(r.unverified.map((u) => u.field)).not.toContain("video_noise");
  });

  it("records nothing for a costume bot, whose picture has no noise layer", () => {
    const r = registered({ videoMode: "costume", videoNoise: 0.3 });
    expect(r.video_noise).toBeNull();
    expect(r.unverified.map((u) => u.field)).not.toContain("video_noise");
  });
});

describe("speech in the participant record (issue 3090)", () => {
  const recorded = (t: BotTask): unknown => {
    const dir = runDir();
    new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }).register(t);
    return (readRecord(dir, t.botId) as { speech: unknown }).speech;
  };
  const weighted = { speechPlan: SPEECH_PLAN_DEFAULT };

  it("records a clock bot's plan and its own share of the fleet's talking", () => {
    const shares = [0, 1, 2, 3, 4, 5, 6, 7].map(
      (index) =>
        (
          recorded(
            task({ ...weighted, videoMode: "clock", talkingClock: { index, fleetSize: 8 } }),
          ) as {
            weight: number;
          }
        ).weight,
    );
    expect(shares.filter((w) => Math.abs(w - 0.35) < 1e-9)).toHaveLength(1);
    expect(shares.filter((w) => Math.abs(w - 0.075) < 1e-9)).toHaveLength(6);
    expect(
      recorded(task({ ...weighted, videoMode: "clock", talkingClock: { index: 0, fleetSize: 8 } })),
    ).toEqual({ ...SPEECH_PLAN_DEFAULT, weight: shares[0] });
  });

  it("records the rotation plan with the bot's 1/N, a lone bot's full share, and null off the clock", () => {
    const rotation = { speechPlan: { model: "rotation" as const } };
    expect(
      recorded(task({ ...rotation, videoMode: "clock", talkingClock: { index: 1, fleetSize: 4 } })),
    ).toEqual({ model: "rotation", weight: 0.25 });
    expect(recorded(task({ ...weighted, videoMode: "clock", talkingClock: null }))).toMatchObject({
      weight: 1,
    });
    expect(recorded(task({ ...weighted, videoMode: "costume" }))).toBeNull();
  });

  it("records the default plan for a launched bot, which carries none", () => {
    expect(recorded(task({ videoMode: "clock", talkingClock: null }))).toEqual({
      ...SPEECH_PLAN_DEFAULT,
      weight: 1,
    });
  });
});

describe("skip_canvas_paint in the participant record (issue 3086)", () => {
  type Rec = {
    skip_canvas_paint: { requested: boolean | null; observed: string | null };
    unverified: Array<{ field: string; reason: string }>;
  };
  const entries = (r: Rec): string[] =>
    r.unverified.filter((u) => u.field === "skip_canvas_paint").map((u) => u.reason);
  const pending = (observed: string | null): string =>
    `${SKIP_CANVAS_PAINT_WAIVER_NOTE} requested but __APP_CONFIG.skipCanvasPaint read back ${JSON.stringify(observed)} after the latest join: that join has not happened yet, the launch failed, or the injection did not land`;
  const WAIVED = `${SKIP_CANVAS_PAINT_WAIVER_NOTE}: skipCanvasPaint="true" read back from __APP_CONFIG after join`;

  function recorder(): { dir: string; rec: ParticipantRecorder } {
    const dir = runDir();
    return {
      dir,
      rec: new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }),
    };
  }

  it.each([
    [undefined, null, null],
    [false, false, "false"],
  ] as const)("records skipCanvasPaint %j without an entry", (asked, requested, readback) => {
    const { dir, rec } = recorder();
    const t = task({ skipCanvasPaint: asked });
    rec.register(t);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, null, readback);
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.skip_canvas_paint).toEqual({ requested, observed: readback });
    expect(entries(r)).toEqual([]);
  });

  it("records the waiver once for a false request whose override did not land", () => {
    const { dir, rec } = recorder();
    const t = task({ skipCanvasPaint: false });
    rec.register(t);
    for (let i = 0; i < 2; i++) {
      rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, null, "true");
      rec.rejoining(t.botId, null);
      const mid = readRecord(dir, t.botId) as Rec;
      expect(mid.skip_canvas_paint).toEqual({ requested: false, observed: null });
      expect(entries(mid)).toEqual([]);
    }
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, null, "true");
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.skip_canvas_paint).toEqual({ requested: false, observed: "true" });
    expect(entries(r)).toEqual([WAIVED]);
  });

  it("flags a skip-paint bot as pending until join reads the override back", () => {
    const { dir, rec } = recorder();
    const t = task({ skipCanvasPaint: true });
    rec.register(t);
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([pending(null)]);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, null, "false");
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([pending("false")]);
  });

  it("records the waiver once join reads it back, and re-verifies across a ctl rejoin", () => {
    const { dir, rec } = recorder();
    const t = task({ skipCanvasPaint: true });
    rec.register(t);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, null, "true");
    let r = readRecord(dir, t.botId) as Rec;
    expect(r.skip_canvas_paint).toEqual({ requested: true, observed: "true" });
    expect(entries(r)).toEqual([WAIVED]);
    rec.rejoining(t.botId, "dialup");
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([pending(null)]);
    rec.joined(t.botId, 1_000, { camera: true, mic: true }, null, null, "true");
    r = readRecord(dir, t.botId) as Rec;
    expect(entries(r)).toEqual([WAIVED]);
  });
});

describe("simulcast_max_layers in the participant record (#3246)", () => {
  type Rec = {
    simulcast_max_layers: { requested: number | null; observed: number | string | null };
    unverified: Array<{ field: string; reason: string }>;
  };
  const entries = (r: Rec): string[] =>
    r.unverified.filter((u) => u.field === "simulcast_max_layers").map((u) => u.reason);
  const capped = (n: number): string =>
    `__APP_CONFIG.experimentalSimulcastMaxLayers=${n} after join caps this bot at ${n} video layer(s); production omits the key, so a real client publishes up to 3`;
  const pending = (n: number, observed: number | null): string =>
    `requested ${n} but __APP_CONFIG.experimentalSimulcastMaxLayers=${JSON.stringify(observed)} after the latest join: that join has not happened yet, or the launch failed`;
  const join = (rec: ParticipantRecorder, id: string, readback: number | null): void =>
    rec.joined(id, 1_000, { camera: true, mic: true }, null, null, null, null, readback);

  function setup(simulcastMaxLayers?: number): {
    dir: string;
    rec: ParticipantRecorder;
    id: string;
  } {
    const dir = runDir();
    const rec = new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD });
    const t = task({ simulcastMaxLayers });
    rec.register(t);
    return { dir, rec, id: t.botId };
  }

  it("records the deployment's value and flags a ceiling below production's", () => {
    const { dir, rec, id } = setup();
    expect(entries(readRecord(dir, id) as Rec)).toEqual([]);
    join(rec, id, 1);
    const r = readRecord(dir, id) as Rec;
    expect(r.simulcast_max_layers).toEqual({ requested: null, observed: 1 });
    expect(entries(r)).toEqual([capped(1)]);
  });

  it.each([3, null])("adds no entry for an inherited %j (key at depth, or absent)", (readback) => {
    const { dir, rec, id } = setup();
    join(rec, id, readback);
    const r = readRecord(dir, id) as Rec;
    expect(r.simulcast_max_layers).toEqual({ requested: null, observed: readback });
    expect(entries(r)).toEqual([]);
  });

  it("flags a request as pending until join reads it back, and again across a ctl rejoin", () => {
    const { dir, rec, id } = setup(3);
    expect(entries(readRecord(dir, id) as Rec)).toEqual([pending(3, null)]);
    join(rec, id, 3);
    let r = readRecord(dir, id) as Rec;
    expect(r.simulcast_max_layers).toEqual({ requested: 3, observed: 3 });
    expect(entries(r)).toEqual([]);
    rec.rejoining(id, "dialup");
    r = readRecord(dir, id) as Rec;
    expect(r.simulcast_max_layers.observed).toBeNull();
    expect(entries(r)).toEqual([pending(3, null)]);
  });

  it("flags an unrequested bot whose read-back did not complete as unknown, not absent", () => {
    const { dir, rec, id } = setup();
    rec.joined(id, 1_000, { camera: true, mic: true }, null, null, null, null, "unread");
    const r = readRecord(dir, id) as Rec;
    expect(r.simulcast_max_layers).toEqual({ requested: null, observed: "unread" });
    expect(entries(r)).toEqual([
      "the after-join read-back of __APP_CONFIG.experimentalSimulcastMaxLayers timed out or failed, so this bot's video layer ceiling is unknown",
    ]);
  });

  it("still flags an honoured request below the ladder depth as a capped bot", () => {
    const { dir, rec, id } = setup(2);
    join(rec, id, 2);
    expect(entries(readRecord(dir, id) as Rec)).toEqual([capped(2)]);
  });
});

describe("transport in the participant record (issue 3087)", () => {
  type Rec = {
    transport_applied: unknown;
    transport_observed: string | null;
    transport_fallbacks: number;
    unverified: Array<{ field: string; reason: string }>;
    participant: { transport_intended: string };
  };
  const entries = (r: Rec): string[] =>
    r.unverified.filter((u) => u.field === "transport_intended").map((u) => u.reason);
  const applied = (preference: string, source = "default", wt_urls = 1) => ({
    kind: "applied" as const,
    applied: { preference, source, wt_urls, ws_urls: 1 },
  });
  const elected = (active: "websocket" | "webtransport" | "none") => ({
    kind: "elected" as const,
    active,
  });

  function recorder(): { dir: string; rec: ParticipantRecorder } {
    const dir = runDir();
    return {
      dir,
      rec: new ParticipantRecorder({ runDir: dir, role: null, kernelNetem: "none", pod: POD }),
    };
  }

  it("records the intended transport and stays unverified until the applied line", () => {
    const { dir, rec } = recorder();
    const t = task({ transport: "websocket" });
    rec.register(t);
    let r = readRecord(dir, t.botId) as Rec;
    expect(r.participant.transport_intended).toBe("websocket");
    expect(r.transport_applied).toBeNull();
    expect(entries(r)).toHaveLength(1);
    rec.transportObserved(t.botId, applied("websocket"));
    rec.transportObserved(t.botId, elected("websocket"));
    r = readRecord(dir, t.botId) as Rec;
    expect(entries(r)).toEqual([]);
    expect(r.transport_observed).toBe("websocket");
    expect(r.transport_fallbacks).toBe(0);
  });

  it("counts webtransport-to-websocket moves across none lines, not repeated lines", () => {
    const { dir, rec } = recorder();
    const t = task({ transport: "webtransport" });
    rec.register(t);
    rec.transportObserved(t.botId, applied("webtransport"));
    rec.transportObserved(t.botId, elected("webtransport"));
    rec.transportObserved(t.botId, elected("websocket"));
    rec.transportObserved(t.botId, elected("websocket"));
    rec.rejoining(t.botId, null);
    const rejoined = readRecord(dir, t.botId) as Rec;
    expect(rejoined.transport_applied).toBeNull();
    expect(rejoined.transport_observed).toBeNull();
    expect(entries(rejoined)).toEqual([
      expect.stringContaining("no 'Transport preference applied:'"),
    ]);
    rec.transportObserved(t.botId, applied("webtransport"));
    rec.transportObserved(t.botId, elected("websocket"));
    rec.transportObserved(t.botId, elected("webtransport"));
    rec.transportObserved(t.botId, elected("none"));
    rec.transportObserved(t.botId, elected("websocket"));
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.transport_fallbacks).toBe(2);
    expect(r.transport_observed).toBe("websocket");
  });

  it("does not count a first election that lands on websocket", () => {
    const { dir, rec } = recorder();
    const t = task({ transport: "webtransport" });
    rec.register(t);
    rec.transportObserved(t.botId, applied("webtransport"));
    rec.transportObserved(t.botId, elected("none"));
    rec.transportObserved(t.botId, elected("websocket"));
    expect((readRecord(dir, t.botId) as Rec).transport_fallbacks).toBe(0);
  });

  it("treats the first election after a ctl rejoin as a first election", () => {
    const { dir, rec } = recorder();
    const t = task({ transport: "webtransport" });
    rec.register(t);
    rec.transportObserved(t.botId, applied("webtransport"));
    rec.transportObserved(t.botId, elected("webtransport"));
    rec.rejoining(t.botId, null);
    rec.transportObserved(t.botId, applied("webtransport"));
    rec.transportObserved(t.botId, elected("websocket"));
    expect((readRecord(dir, t.botId) as Rec).transport_fallbacks).toBe(0);
  });

  it("does not count websocket as a fallback when webtransport was never offered", () => {
    const { dir, rec } = recorder();
    const t = task({ transport: "webtransport" });
    rec.register(t);
    rec.transportObserved(t.botId, applied("webtransport", "default", 0));
    rec.transportObserved(t.botId, elected("websocket"));
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.transport_fallbacks).toBe(0);
    expect(entries(r)).toEqual([expect.stringContaining("wt_urls=0")]);
  });

  it("flags a remembered choice on an auto bot without failing it", () => {
    const { dir, rec } = recorder();
    const t = task();
    rec.register(t);
    expect(entries(readRecord(dir, t.botId) as Rec)).toEqual([]);
    rec.transportObserved(t.botId, applied("websocket", "sticky"));
    const r = readRecord(dir, t.botId) as Rec;
    expect(r.participant.transport_intended).toBe("auto");
    expect(entries(r)).toEqual([expect.stringContaining("source=sticky")]);
  });
});

describe("persona in the participant record (issue 3091)", () => {
  const room = (n: number, micOn = "0.80,0.65,0.37,0.25,0.11") => {
    const s = resolvePersonaSettings(true, { BOT_PERSONA_MIC_ON: micOn });
    if (s.kind !== "ok" || s.value === null) throw new Error("bad settings");
    return assignPersonas(createPersonaPlanner(SPEECH_PLAN_DEFAULT, n), n, s.value);
  };
  const persona = (index: number, r = room(3)) => personaFor(r, { index, fleetSize: r.fleetSize });
  type Rec = {
    persona: { mic: string; camera: string; toggles: PersonaTally | null } | null;
    unverified: Array<{ field: string; reason: string }>;
  };
  const tally = (over: Partial<PersonaTally> = {}): PersonaTally => ({
    ok: 4,
    failed: 0,
    already: 0,
    rendered: 4,
    unrendered: 0,
    medianAbsMs: 610,
    minGapMs: 9_000,
    ...over,
  });
  const setup = (p: ReturnType<typeof persona> | null) => {
    const dir = runDir();
    const recorder = new ParticipantRecorder({
      runDir: dir,
      role: null,
      kernelNetem: "none",
      pod: POD,
    });
    const t = task({ videoMode: "clock", speechPlan: SPEECH_PLAN_DEFAULT, persona: p });
    recorder.register(t);
    return { recorder, read: () => readRecord(dir, t.botId) as unknown as Rec, botId: t.botId };
  };
  const fields = (r: Rec): string[] => r.unverified.map((u) => u.field);

  it("is null without personas", () => {
    const { read } = setup(null);
    expect(read().persona).toBeNull();
    expect(fields(read())).not.toContain("persona.room");
  });

  it("records the persona, flags the toggled media and a room share below its target", () => {
    const ptt = setup({ ...persona(0), camera: "with-voice" }).read();
    expect(ptt.persona).toMatchObject({ mic: "push-to-talk", camera: "with-voice", toggles: null });
    expect(fields(ptt)).toEqual(
      expect.arrayContaining(["publishes.mic", "publishes.camera", "persona.room"]),
    );
    const open = setup(persona(2, room(3, "0.50,0.65,0.37,0.25,0.11"))).read();
    expect(open.persona).toMatchObject({ mic: "open", camera: "always" });
    expect(fields(open)).not.toContain("publishes.mic");
    expect(fields(open)).not.toContain("publishes.camera");
    expect(fields(open)).not.toContain("persona.room");
  });

  it("carries the latest tally and lists failed or unrendered toggles", () => {
    const { recorder, read, botId } = setup(persona(0));
    recorder.personaToggled(botId, tally());
    expect(read().persona?.toggles).toEqual(tally());
    expect(fields(read())).not.toContain("persona.toggles");
    recorder.personaToggled(botId, tally({ failed: 1, unrendered: 2 }));
    const reason = read().unverified.find((u) => u.field === "persona.toggles")?.reason;
    expect(reason).toContain("1 toggles failed");
    expect(reason).toContain("2 clicked toggles had no 'Host render:' line");
    recorder.personaToggled(botId, tally({ unrendered: 1 }));
    expect(fields(read())).toContain("persona.toggles");
    recorder.personaToggled(botId, tally());
    expect(fields(read())).not.toContain("persona.toggles");
    expect(fields(read())).toContain("persona.room");
  });
});
