import { mkdirSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { type DiagPacketsObservation, participantEmail } from "./bot";
import {
  type DecodeBudget,
  DECODE_BUDGET_OFF_VALUE,
  DECODE_BUDGET_STORAGE_KEY,
  DECODE_BUDGET_WAIVER_NOTE,
  isClientDecodeBudget,
} from "./decode-budget";
import {
  isNetemProfileName,
  type NetemAction,
  NETEM_PROFILES,
  netemActionShapesIngress,
  type NetemParams,
} from "./control/netem";
import { JOIN_MEDIA_ON, type JoinMedia, type JoinMediaState } from "./meeting-join";
import type { BotTask } from "./orchestrator";
import { localImageRevision } from "./resource/session";

/** Wrapper schema; `participant` uses `call-quality-run-manifest/v1` field names (#2913 §5.2). */
export const PARTICIPANT_RECORD_SCHEMA = "bots-app-participant-record/v0";

export const PARTICIPANT_RECORD_DIR = "participants";

export const ROLE_PATTERN = /^[A-Za-z0-9._-]{1,64}$/;

export interface ManifestNetwork {
  profile: string;
  shaped: boolean | null;
  direction: "none" | "egress" | "ingress" | "both" | null;
  shaper: "none" | "netem" | "netsim" | null;
  params: NetemParams | Record<string, never>;
}

/** One `participants[]` entry, minus `steps`, which the scenario runner derives from the step windows. */
export interface ManifestParticipantDraft {
  user_id: string | null;
  fleet: "browser";
  role: string | null;
  observer: true;
  talker: false;
  publishes: { camera: boolean | null; mic: boolean | null; screen: false } | null;
  network: ManifestNetwork;
  transport_intended: "auto";
  join_ts: number | null;
  leave_ts: number | null;
  placement: { ordinal: number | null; node: string | null } | null;
  /** This pod's drawn `BOT_MAX_JOIN_STAGGER_SECS` delay; `null` when no stagger was configured. */
  stagger_ms: number | null;
}

export interface ParticipantRecord {
  schema: typeof PARTICIPANT_RECORD_SCHEMA;
  bot_id: string;
  join_media_requested: { camera: boolean; mic: boolean };
  code: { images: { "bots-app": string | null } };
  /** `finishReason` once the bot has finished; `null` while it runs. */
  outcome: string | null;
  /** Fields a reader must not quote as measured, with why. */
  unverified: Array<{ field: string; reason: string }>;
  /** Not a manifest participant field. */
  diagnostics_packets: {
    requested: "off" | "default";
    observed: DiagPacketsObservation["state"] | null;
    source: DiagPacketsObservation["source"] | null;
  };
  /** Not a manifest participant field. `requested: "off"` is a fidelity waiver. */
  decode_budget: {
    requested: DecodeBudget;
    /** `vc_decode_budget_override` read back after the latest join; `null` before it or when absent. */
    observed: string | null;
  };
  participant: ManifestParticipantDraft;
}

function syncDecodeBudgetUnverified(rec: ParticipantRecord): void {
  rec.unverified = rec.unverified.filter((u) => u.field !== "decode_budget");
  const d = rec.decode_budget;
  const read = `${DECODE_BUDGET_STORAGE_KEY}=${d.observed}`;
  let reason: string | null = null;
  if (d.requested === "client") {
    if (!isClientDecodeBudget(d.observed)) {
      reason = `client requested but ${read} was in storage after join (a captured storage state?), so this bot ran under that override`;
    }
  } else if (d.observed === DECODE_BUDGET_OFF_VALUE) {
    reason = `${DECODE_BUDGET_WAIVER_NOTE}: ${read} read back after join, which the client parses as DecodeBudgetOverride::All`;
  } else {
    reason = `${DECODE_BUDGET_WAIVER_NOTE} requested but ${DECODE_BUDGET_STORAGE_KEY} was not read back after the latest join: that join has not happened yet, or the launch failed`;
  }
  if (reason !== null) rec.unverified.push({ field: "decode_budget", reason });
}

function syncDiagPacketsUnverified(rec: ParticipantRecord): void {
  rec.unverified = rec.unverified.filter((u) => u.field !== "diagnostics_packets");
  const d = rec.diagnostics_packets;
  if (d.requested !== "off" || d.observed === "DISABLED") return;
  rec.unverified.push({
    field: "diagnostics_packets",
    reason:
      d.observed === null
        ? "no 'diagnostics packets:' console line: the client predates #2970, logLevel is above info, or the bot never mounted the meeting"
        : `requested off but the client logged ENABLED (source=${d.source}): this bot still sends DIAGNOSTICS`,
  });
}

const NO_SHAPING: ManifestNetwork = {
  profile: "none",
  shaped: false,
  direction: "none",
  shaper: "none",
  params: {},
};

/**
 * `kernel` is `BOT_NETEM_APPLIED` from the entrypoint: the post-read-verified startup profile,
 * `unknown` when the qdisc was inherited or unreadable, unset outside the container.
 */
export function recordedNetwork(
  netsim: string | null | undefined,
  kernel: string | undefined,
): { network: ManifestNetwork; unverified: string | null } {
  let network = NO_SHAPING;
  let unverified: string | null = null;
  const unset = kernel === undefined || kernel === "";
  if (unset) {
    network = { profile: "unknown", shaped: null, direction: null, shaper: null, params: {} };
    unverified =
      "BOT_NETEM_APPLIED unset: not started by the container entrypoint, so the link was not checked";
  } else if (kernel !== "none") {
    const params = isNetemProfileName(kernel) ? NETEM_PROFILES[kernel] : undefined;
    if (params === undefined) {
      network = { profile: "unknown", shaped: null, direction: null, shaper: null, params: {} };
      unverified = `startup qdisc was not applied by this pod (BOT_NETEM_APPLIED=${kernel}); see the pod log netem=[...]`;
    } else if (params !== null) {
      network = { profile: kernel, shaped: true, direction: "both", shaper: "netem", params };
    }
  }
  if (netsim) {
    if (network.shaped === true) {
      return {
        network,
        unverified: `netsim uplink ${netsim} also requested on top of kernel shaping; the manifest carries one shaper`,
      };
    }
    if (network.shaped === null && !unset) {
      return {
        network,
        unverified: `${unverified}; netsim uplink ${netsim} also requested on top of it`,
      };
    }
    const kernelNote = unset ? "; the kernel qdisc was not checked" : "";
    network = { profile: netsim, shaped: true, direction: "egress", shaper: "netsim", params: {} };
    unverified = `netsim applies only in a client built with --features netsim; not verified${kernelNote}`;
  }
  return { network, unverified };
}

/** The netsim preset `"none"` shapes nothing. */
function netsimShaper(network: string | null | undefined): string | null {
  return network && network !== "none" ? network : null;
}

/** A runtime `/netem` result as one bot records it: a netsim uplink keeps its note, or is the shaper on an unshaped link. */
export function netemActionForBot(
  network: ManifestNetwork,
  reason: string,
  netsim: string | null | undefined,
): { network: ManifestNetwork; reason: string } {
  if (!netsim) return { network, reason };
  if (network.shaped === false) {
    return {
      network: { profile: netsim, shaped: true, direction: "egress", shaper: "netsim", params: {} },
      reason: `netsim applies only in a client built with --features netsim; not verified; kernel qdisc ${reason}`,
    };
  }
  return { network, reason: `${reason}; netsim uplink ${netsim} also requested on top of it` };
}

/** Set by the StatefulSet's downward API (`spec.nodeName`). */
export const NODE_NAME_ENV = "BOT_NODE_NAME";

/** What the entrypoint and the pod spec established for this process, read once at startup. */
export interface PodContext {
  ordinal: number | null;
  node: string | null;
  staggerMs: number | null;
  staggerIncomplete: boolean;
  imageRevision: string | null;
}

export function readPodContext(env: NodeJS.ProcessEnv): PodContext {
  const count = (raw: string | undefined): number | null =>
    raw !== undefined && /^\d{1,9}$/.test(raw) ? Number.parseInt(raw, 10) : null;
  const node = env[NODE_NAME_ENV]?.trim();
  return {
    ordinal: count(env.BOT_POD_ORDINAL),
    node: node ? node : null,
    staggerMs: count(env.BOT_JOIN_STAGGER_MS),
    staggerIncomplete: env.BOT_JOIN_STAGGER_INCOMPLETE === "1",
    imageRevision: localImageRevision(env),
  };
}

export function netemActionNetwork(action: NetemAction): ManifestNetwork {
  if (action.op === "clear") return NO_SHAPING;
  return {
    profile: action.label,
    shaped: true,
    direction: netemActionShapesIngress(action) ? "both" : "egress",
    shaper: "netem",
    params: action.params,
  };
}

export type Resolved<T> = { kind: "ok"; value: T } | { kind: "invalid"; message: string };

function parseEnvFlag(name: string, raw: string | undefined): boolean | string {
  const s = raw?.trim().toLowerCase() ?? "";
  if (s === "" || s === "0" || s === "false") return false;
  if (s === "1" || s === "true") return true;
  return `${name} must be true/false/1/0, got "${raw}"`;
}

/** Flags turn a medium off; the env (`BOT_JOIN_CAMERA_OFF` / `BOT_JOIN_MIC_MUTED`) applies when a flag is absent. */
export function resolveJoinMedia(input: {
  cameraOffFlag?: boolean;
  micMutedFlag?: boolean;
  env: NodeJS.ProcessEnv;
  cameraCycle: boolean;
}): Resolved<JoinMedia> {
  const cameraOff =
    input.cameraOffFlag || parseEnvFlag("BOT_JOIN_CAMERA_OFF", input.env.BOT_JOIN_CAMERA_OFF);
  const micMuted =
    input.micMutedFlag || parseEnvFlag("BOT_JOIN_MIC_MUTED", input.env.BOT_JOIN_MIC_MUTED);
  for (const v of [cameraOff, micMuted])
    if (typeof v === "string") return { kind: "invalid", message: v };
  if (cameraOff === true && input.cameraCycle) {
    return {
      kind: "invalid",
      message:
        "a camera-off join cannot be combined with the camera duty cycle (BOT_CAMERA_*_SECS_*)",
    };
  }
  return { kind: "ok", value: { camera: cameraOff !== true, mic: micMuted !== true } };
}

export function resolveRole(raw: string | undefined): Resolved<string | null> {
  const s = raw?.trim() ?? "";
  if (s === "") return { kind: "ok", value: null };
  if (!ROLE_PATTERN.test(s)) {
    return {
      kind: "invalid",
      message: `--role / BOT_ROLE must match ${ROLE_PATTERN}, got "${raw}"`,
    };
  }
  return { kind: "ok", value: s };
}

export interface ParticipantRecorderOptions {
  runDir: string;
  role: string | null;
  kernelNetem: string | undefined;
  pod: PodContext;
  write?: (path: string, json: string) => void;
  warn?: (msg: string) => void;
}

function atomicWrite(path: string, json: string): void {
  const tmp = `${path}.tmp`;
  writeFileSync(tmp, json, { mode: 0o644 });
  renameSync(tmp, path);
}

/** Keeps one record file per local bot, rewritten at register, first join, rejoin, `/netem` and finish. */
export class ParticipantRecorder {
  private readonly records = new Map<string, ParticipantRecord>();
  private readonly netsim = new Map<string, string | null | undefined>();
  private lastNetem: { network: ManifestNetwork; reason: string } | null = null;
  private readonly dir: string;
  private readonly write: (path: string, json: string) => void;
  private readonly warn: (msg: string) => void;

  constructor(private readonly opts: ParticipantRecorderOptions) {
    this.dir = join(opts.runDir, PARTICIPANT_RECORD_DIR);
    this.write = opts.write ?? atomicWrite;
    this.warn = opts.warn ?? ((m) => console.warn(m));
  }

  register(task: BotTask): void {
    const requested = task.joinMedia ?? JOIN_MEDIA_ON;
    const netsim = netsimShaper(task.network);
    const startup = recordedNetwork(netsim, this.opts.kernelNetem);
    const after =
      this.lastNetem === null
        ? null
        : netemActionForBot(this.lastNetem.network, this.lastNetem.reason, netsim);
    const network = after?.network ?? startup.network;
    const netNote = after?.reason ?? startup.unverified;
    this.netsim.set(task.botId, netsim);
    const unverified: ParticipantRecord["unverified"] = [];
    if (netNote !== null) unverified.push({ field: "network", reason: netNote });
    const { pod } = this.opts;
    if (pod.staggerIncomplete) {
      unverified.push({
        field: "stagger_ms",
        reason: "the stagger sleep failed, so the bot joined before the drawn delay elapsed",
      });
    }
    if (pod.imageRevision === null) {
      unverified.push({
        field: "code.images.bots-app",
        reason: "BOTS_IMAGE_REVISION unset: not a build.sh-stamped fleet image",
      });
    } else if (pod.imageRevision.trim() === "") {
      unverified.push({
        field: "code.images.bots-app",
        reason: "BOTS_IMAGE_REVISION is empty: the build passed an empty GIT_SHA",
      });
    } else if (pod.imageRevision === "unknown") {
      unverified.push({
        field: "code.images.bots-app",
        reason:
          "BOTS_IMAGE_REVISION is the Dockerfile default 'unknown': the build passed no GIT_SHA",
      });
    } else if (pod.imageRevision.endsWith("-dirty")) {
      unverified.push({
        field: "code.images.bots-app",
        reason: "the image was built from an uncommitted tree",
      });
    }
    const userId = task.authBackend === "jwt" ? participantEmail(task.participant) : null;
    unverified.push({
      field: "user_id",
      reason:
        userId === null
          ? `auth ${task.authBackend}: no meeting-api join response named this session; read it back from the labels`
          : "the JWT subject; not checked against a relay label, which must carry it verbatim",
    });
    if (task.cameraCycle) {
      unverified.push({
        field: "publishes.camera",
        reason:
          "the camera duty cycle toggles the camera during the run; this is the join-time state",
      });
    }
    const rec: ParticipantRecord = {
      schema: PARTICIPANT_RECORD_SCHEMA,
      bot_id: task.botId,
      join_media_requested: { camera: requested.camera, mic: requested.mic },
      code: { images: { "bots-app": pod.imageRevision } },
      outcome: null,
      unverified,
      diagnostics_packets: {
        requested: task.diagPackets === "off" ? "off" : "default",
        observed: null,
        source: null,
      },
      decode_budget: { requested: task.decodeBudget ?? "client", observed: null },
      participant: {
        user_id: userId,
        fleet: "browser",
        role: this.opts.role,
        observer: true,
        talker: false,
        publishes: null,
        network,
        transport_intended: "auto",
        join_ts: null,
        leave_ts: null,
        placement:
          pod.ordinal === null && pod.node === null
            ? null
            : { ordinal: pod.ordinal, node: pod.node },
        stagger_ms: pod.staggerMs,
      },
    };
    syncDiagPacketsUnverified(rec);
    syncDecodeBudgetUnverified(rec);
    this.records.set(task.botId, rec);
    this.flush(task.botId);
  }

  joined(
    botId: string,
    joinedAtMs: number,
    media: JoinMediaState | null,
    sessionUserId: string | null,
    decodeBudgetReadback: string | null = null,
  ): void {
    const rec = this.records.get(botId);
    if (rec === undefined) return;
    rec.decode_budget.observed = decodeBudgetReadback;
    syncDecodeBudgetUnverified(rec);
    if (rec.participant.join_ts !== null) {
      this.flush(botId);
      return;
    }
    rec.participant.join_ts = joinedAtMs / 1000;
    if (sessionUserId !== null) {
      const expected = rec.participant.user_id;
      rec.participant.user_id = sessionUserId;
      rec.unverified = rec.unverified.filter((u) => u.field !== "user_id");
      rec.unverified.push({
        field: "user_id",
        reason:
          expected === null || expected === sessionUserId
            ? "read from the meeting-api join response the client connects with; not checked against a relay label"
            : `the meeting-api join response named this session ${sessionUserId}, not the JWT subject ${expected}`,
      });
    }
    const state = media ?? { camera: null, mic: null };
    rec.participant.publishes = { camera: state.camera, mic: state.mic, screen: false };
    for (const kind of ["camera", "mic"] as const) {
      if (state[kind] === null) {
        rec.unverified.push({
          field: `publishes.${kind}`,
          reason: "the join step could not find the control, so the state at join is unknown",
        });
      } else if (state[kind] !== rec.join_media_requested[kind]) {
        rec.unverified.push({
          field: `publishes.${kind}`,
          reason: `requested ${rec.join_media_requested[kind] ? "on" : "off"} at join but observed ${state[kind] ? "on" : "off"}`,
        });
      }
    }
    this.flush(botId);
  }

  /** A ctl network change tears the bot down and relaunches it; fired before the relaunch, which can fail. */
  rejoining(botId: string, network: string | null): void {
    const rec = this.records.get(botId);
    if (rec === undefined) return;
    this.netsim.set(botId, netsimShaper(network));
    rec.diagnostics_packets.observed = null;
    rec.diagnostics_packets.source = null;
    syncDiagPacketsUnverified(rec);
    rec.decode_budget.observed = null;
    syncDecodeBudgetUnverified(rec);
    if (!rec.unverified.some((u) => u.field === "rejoin")) {
      rec.unverified.push({
        field: "rejoin",
        reason:
          "the bot left and relaunched for a ctl network change: join_ts..leave_ts spans that gap, publishes describes the first join, and so does network until a POST /netem re-stamps it",
      });
    }
    this.flush(botId);
  }

  diagPacketsObserved(botId: string, obs: DiagPacketsObservation): void {
    const rec = this.records.get(botId);
    if (rec === undefined) return;
    rec.diagnostics_packets.observed = obs.state;
    rec.diagnostics_packets.source = obs.source;
    syncDiagPacketsUnverified(rec);
    this.flush(botId);
  }

  finished(botId: string, finishedAtMs: number, outcome: string | undefined): void {
    const rec = this.records.get(botId);
    if (rec === undefined) return;
    if (rec.participant.join_ts !== null) rec.participant.leave_ts = finishedAtMs / 1000;
    rec.outcome = outcome ?? "unknown";
    this.flush(botId);
  }

  /** Re-stamps every running bot: they share the pod interface the action shaped. */
  netemApplied(action: NetemAction, atMs: number, failed = false): void {
    const when = new Date(atMs).toISOString();
    const network: ManifestNetwork = failed
      ? { profile: "unknown", shaped: null, direction: null, shaper: null, params: {} }
      : netemActionNetwork(action);
    const reason = failed
      ? `POST /netem ${action.op} failed partway at ${when}; the kernel state is unknown`
      : `set by POST /netem at ${when}; the shaping params are not read back`;
    this.lastNetem = { network, reason };
    for (const [botId, rec] of this.records) {
      if (rec.outcome !== null) continue;
      const own = netemActionForBot(network, reason, this.netsim.get(botId));
      rec.participant.network = own.network;
      rec.unverified = rec.unverified.filter((u) => u.field !== "network");
      rec.unverified.push({ field: "network", reason: own.reason });
      this.flush(botId);
    }
  }

  snapshot(botId: string): ParticipantRecord | undefined {
    const rec = this.records.get(botId);
    return rec === undefined ? undefined : structuredClone(rec);
  }

  private flush(botId: string): void {
    const rec = this.records.get(botId);
    if (rec === undefined) return;
    try {
      mkdirSync(this.dir, { recursive: true });
      this.write(join(this.dir, `${botId}.json`), `${JSON.stringify(rec, null, 2)}\n`);
    } catch (e) {
      this.warn(`participant record for ${botId} not written: ${(e as Error).message}`);
    }
  }
}
