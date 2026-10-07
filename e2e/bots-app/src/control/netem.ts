import { execFile } from "node:child_process";

/**
 * OS-level network impairment for a single-bot pod, via Linux `tc` +
 * `netem`. This is DISTINCT from the client-side `?netsim=` feature
 * (which shapes traffic inside the browser via a WASM shim): `netem`
 * shapes the pod's real kernel network interface, so it also impairs
 * the QUIC/WebTransport and TCP/WebSocket handshakes, TLS, DNS — the
 * whole stack — the way a real degraded link does.
 *
 * SECURITY: every `tc` invocation goes through {@link execFile} with an
 * argv ARRAY and no shell, so no request-supplied value is ever parsed
 * by a shell. Numeric parameters are validated + re-formatted by us
 * (never passed through verbatim), and the interface name is validated
 * against {@link IFACE_PATTERN}. There is no code path that interpolates
 * untrusted text into a shell string.
 *
 * Direction: root netem shapes egress; ingress is shaped on an `ifb` mirror,
 * installed only when the params carry both downlink fields.
 */

/** Default interface shaped when the deploy config does not override it. */
export const NETEM_IFACE_DEFAULT = "eth0";

/**
 * Linux interface-name constraint (IFNAMSIZ is 16 incl. NUL ⇒ 15 usable
 * chars). We additionally restrict the character set so a mis-set
 * `--netem-iface` can never smuggle anything odd into the argv. The
 * interface is operator/deploy configuration — NEVER taken from an HTTP
 * request body — but we validate defensively regardless.
 */
export const IFACE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,14}$/;

/** Loopback names refused as shaping targets — the readinessProbe rides loopback. */
export const LOOPBACK_IFACES = new Set(["lo", "lo0"]);

/**
 * Concrete netem parameters. All optional so a profile / raw request can
 * specify any subset; a shape action requires at least one to be set.
 * `jitterMs` requires `delayMs` (netem's grammar puts jitter as delay's
 * optional second argument).
 */
export interface NetemParams {
  /** One-way delay added to every packet, milliseconds. */
  delayMs?: number;
  /** Delay jitter (±), milliseconds. Only valid alongside `delayMs`. */
  jitterMs?: number;
  /** Independent (Bernoulli) packet loss, percent 0–100. */
  lossPct?: number;
  /** Egress rate cap, kilobit/s. */
  rateKbit?: number;
  /** Ingress rate cap, kilobit/s — for the `ifb` mirror, never the root qdisc. */
  downlinkRateKbit?: number;
  /** Netem queue depth, packets. Unset ⇒ netem's own default backlog. */
  limitPkts?: number;
  /** Ingress queue depth, packets — for the `ifb` mirror, never the root qdisc. */
  ingressLimitPkts?: number;
}

const NETEM_PARAM_KEY_SET = {
  delayMs: true,
  jitterMs: true,
  lossPct: true,
  rateKbit: true,
  downlinkRateKbit: true,
  limitPkts: true,
  ingressLimitPkts: true,
} as const satisfies Record<keyof NetemParams, true>;

export const NETEM_PARAM_KEYS = Object.keys(NETEM_PARAM_KEY_SET) as ReadonlyArray<
  keyof NetemParams
>;

/**
 * A fully-resolved netem operation. `shape` carries validated params;
 * `clear` removes any qdisc (restores the interface to line rate).
 * `label` is a human handle (profile name or `"custom"` / `"clear"`)
 * used only for logging + the API response.
 */
export type NetemAction =
  | { op: "shape"; label: string; params: NetemParams }
  | { op: "clear"; label: string };

/**
 * Named profiles. Values MIRROR `videocall-netsim/src/profiles.rs`
 * (both directions, locked by netem-profile-drift.test.ts) so operators use
 * ONE impairment vocabulary across the client `?netsim=` shim and the
 * OS-level tc path. `null` means "no shaping" ⇒ clear the qdisc.
 *
 * `clean` and `none` are aliases for the clear operation (`none` matches
 * the netsim preset name; `clean` matches the brief's naming).
 */
export const NETEM_PROFILES: Readonly<Record<string, NetemParams | null>> = {
  clean: null,
  none: null,
  good_wifi: {
    delayMs: 20,
    jitterMs: 5,
    lossPct: 0.1,
    rateKbit: 20_000,
    downlinkRateKbit: 50_000,
    limitPkts: 100,
    ingressLimitPkts: 150,
  },
  good_4g: {
    delayMs: 50,
    jitterMs: 15,
    lossPct: 0.5,
    rateKbit: 10_000,
    downlinkRateKbit: 30_000,
    limitPkts: 100,
    ingressLimitPkts: 250,
  },
  congested_wifi: {
    delayMs: 80,
    jitterMs: 30,
    lossPct: 2,
    rateKbit: 2_000,
    downlinkRateKbit: 4_000,
    limitPkts: 55,
    ingressLimitPkts: 55,
  },
  lossy_mobile: {
    delayMs: 150,
    jitterMs: 50,
    lossPct: 5,
    rateKbit: 800,
    downlinkRateKbit: 2_000,
    limitPkts: 40,
    ingressLimitPkts: 50,
  },
  satellite: {
    delayMs: 600,
    jitterMs: 50,
    lossPct: 1,
    rateKbit: 1_500,
    downlinkRateKbit: 10_000,
    limitPkts: 300,
    ingressLimitPkts: 700,
  },
  dialup: {
    delayMs: 200,
    jitterMs: 40,
    lossPct: 3,
    rateKbit: 56,
    downlinkRateKbit: 56,
    limitPkts: 10,
    ingressLimitPkts: 10,
  },
};

/** Stable list of profile names for CLI help / error messages. */
export const NETEM_PROFILE_NAMES: readonly string[] = Object.keys(NETEM_PROFILES);

/** Own-property only: `in` would accept `toString`/`constructor` as profiles. */
export function isNetemProfileName(name: string): boolean {
  return NETEM_PROFILE_NAMES.includes(name);
}

/**
 * Thrown by {@link resolveNetemRequest} on any invalid request body. The
 * control server maps this to an HTTP 400.
 */
export class NetemValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NetemValidationError";
  }
}

/** `mirrorRemoved`: a mirror existed and this action left none. `readback`: `tc qdisc show` per device. */
export interface NetemApplyResult {
  commands: string[][];
  label: string;
  op: "shape" | "clear";
  ingressShaped: boolean;
  mirrorRemoved: boolean;
  readback: Record<string, string>;
}

/** `exitStatus` is the status the child exited with, or null when none is available. */
export class NetemExecError extends Error {
  readonly exitStatus: number | null;

  constructor(message: string, exitStatus: number | null) {
    super(message);
    this.name = "NetemExecError";
    this.exitStatus = exitStatus;
  }
}

export const NETEM_EXEC_NOT_RUN_STATUS_MIN = 126;

export function netemExecExitedNonZero(e: unknown): e is NetemExecError {
  return (
    e instanceof NetemExecError &&
    e.exitStatus !== null &&
    e.exitStatus < NETEM_EXEC_NOT_RUN_STATUS_MIN
  );
}

/**
 * Injectable process runner. Production uses {@link defaultNetemExec}
 * (a thin `execFile` wrapper); tests inject a recorder so no real `tc`
 * ever runs. Mirrors the `vpnFetch` / `ssoCaptureFactory` seam pattern
 * already used by the control server. Implementations MUST reject with
 * {@link NetemExecError}.
 */
export type NetemExec = (
  file: string,
  args: string[],
) => Promise<{ stdout: string; stderr: string }>;

/** How long a single `tc` invocation may run before we give up. */
export const NETEM_EXEC_TIMEOUT_MS = 10_000;

/**
 * Real `tc` runner. Uses {@link execFile} (NOT `exec`) so args are passed
 * as a vector to `execvp` with no shell — the injection-safe path.
 */
export function defaultNetemExec(): NetemExec {
  return (file, args) =>
    new Promise((resolve, reject) => {
      execFile(file, args, { timeout: NETEM_EXEC_TIMEOUT_MS }, (err, stdout, stderr) => {
        if (err) {
          const detail = stderr.trim().length > 0 ? stderr.trim() : err.message;
          const exitStatus = typeof err.code === "number" ? err.code : null;
          reject(new NetemExecError(`${file} ${args.join(" ")} failed: ${detail}`, exitStatus));
          return;
        }
        resolve({ stdout, stderr });
      });
    });
}

function assertFiniteNonNegative(value: number, field: string, max: number): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new NetemValidationError(`"${field}" must be a finite number`);
  }
  if (value < 0) {
    throw new NetemValidationError(`"${field}" must be >= 0 (got ${value})`);
  }
  if (value > max) {
    throw new NetemValidationError(`"${field}" must be <= ${max} (got ${value})`);
  }
  return value;
}

function assertRateKbit(value: unknown, field: string): number {
  const rate = assertFiniteNonNegative(value as number, field, 10_000_000);
  if (rate < 8) {
    throw new NetemValidationError(
      `"${field}" must be >= 8 when provided (lower can strand the control API)`,
    );
  }
  return rate;
}

function assertLimitPkts(value: unknown, field: string): number {
  const limit = assertFiniteNonNegative(value as number, field, 100_000);
  if (!Number.isInteger(limit) || limit < 1) {
    throw new NetemValidationError(`"${field}" must be an integer >= 1`);
  }
  return limit;
}

/**
 * Validate a raw params object (from a request body's numeric fields).
 * Enforces sane bounds, the delay-before-jitter grammar rule, and that
 * at least one impairment is specified. Returns a normalized copy —
 * unknown fields are ignored, absent fields stay absent.
 */
export function validateNetemParams(raw: Record<string, unknown>): NetemParams {
  const params: NetemParams = {};
  if (raw.delayMs !== undefined && raw.delayMs !== null) {
    params.delayMs = assertFiniteNonNegative(raw.delayMs as number, "delayMs", 600_000);
  }
  if (raw.jitterMs !== undefined && raw.jitterMs !== null) {
    params.jitterMs = assertFiniteNonNegative(raw.jitterMs as number, "jitterMs", 600_000);
  }
  if (raw.lossPct !== undefined && raw.lossPct !== null) {
    params.lossPct = assertFiniteNonNegative(raw.lossPct as number, "lossPct", 95);
  }
  if (raw.rateKbit !== undefined && raw.rateKbit !== null) {
    params.rateKbit = assertRateKbit(raw.rateKbit, "rateKbit");
  }
  if (raw.downlinkRateKbit !== undefined && raw.downlinkRateKbit !== null) {
    params.downlinkRateKbit = assertRateKbit(raw.downlinkRateKbit, "downlinkRateKbit");
  }
  if (raw.limitPkts !== undefined && raw.limitPkts !== null) {
    params.limitPkts = assertLimitPkts(raw.limitPkts, "limitPkts");
  }
  if (raw.ingressLimitPkts !== undefined && raw.ingressLimitPkts !== null) {
    params.ingressLimitPkts = assertLimitPkts(raw.ingressLimitPkts, "ingressLimitPkts");
  }
  if ((params.downlinkRateKbit === undefined) !== (params.ingressLimitPkts === undefined)) {
    throw new NetemValidationError(
      '"downlinkRateKbit" and "ingressLimitPkts" shape ingress together — supply both or neither',
    );
  }
  if (params.jitterMs !== undefined && params.delayMs === undefined) {
    throw new NetemValidationError('"jitterMs" requires "delayMs" (netem puts jitter after delay)');
  }
  if (
    params.delayMs === undefined &&
    params.lossPct === undefined &&
    params.rateKbit === undefined
  ) {
    throw new NetemValidationError(
      "at least one of delayMs / lossPct / rateKbit is required to shape",
    );
  }
  return params;
}

/**
 * Turn an untrusted request body into a validated {@link NetemAction}.
 *
 * Accepts EITHER:
 *   - `{ profile: "<name>" }` — a named profile ("clean"/"none" ⇒ clear)
 *   - raw {@link NetemParams} — ingress is shaped only when both downlink fields are set
 *
 * Supplying both a profile AND raw params is rejected as ambiguous. An
 * explicit `{ clear: true }` (or the DELETE verb, handled by the caller)
 * also yields a clear action.
 */
export function resolveNetemRequest(body: unknown): NetemAction {
  if (body === null || typeof body !== "object" || Array.isArray(body)) {
    throw new NetemValidationError("request body must be a JSON object");
  }
  const o = body as Record<string, unknown>;

  const hasProfile = o.profile !== undefined && o.profile !== null;
  const hasRawParam = NETEM_PARAM_KEYS.some((k) => o[k] !== undefined);

  if (o.clear === true) {
    if (hasProfile || hasRawParam) {
      throw new NetemValidationError('"clear": true cannot be combined with a profile or params');
    }
    return { op: "clear", label: "clear" };
  }

  if (hasProfile && hasRawParam) {
    throw new NetemValidationError('specify either "profile" or raw params, not both');
  }

  if (hasProfile) {
    if (typeof o.profile !== "string") {
      throw new NetemValidationError('"profile" must be a string');
    }
    if (!isNetemProfileName(o.profile)) {
      throw new NetemValidationError(
        `unknown profile "${o.profile}" (known: ${NETEM_PROFILE_NAMES.join(", ")})`,
      );
    }
    const preset = NETEM_PROFILES[o.profile];
    if (preset === null) {
      // "clean" / "none" ⇒ remove shaping.
      return { op: "clear", label: o.profile };
    }
    return { op: "shape", label: o.profile, params: preset };
  }

  if (hasRawParam) {
    return { op: "shape", label: "custom", params: validateNetemParams(o) };
  }

  throw new NetemValidationError(
    'empty request — supply a "profile", raw params, or "clear": true',
  );
}

function assertIface(iface: string): void {
  if (!IFACE_PATTERN.test(iface)) {
    throw new NetemValidationError(
      `interface "${iface}" is not a valid device name (${IFACE_PATTERN.source})`,
    );
  }
  if (LOOPBACK_IFACES.has(iface)) {
    throw new NetemValidationError(
      `interface "${iface}" is loopback — shaping it would break the pod's readinessProbe`,
    );
  }
}

/**
 * Build the argv (excluding the `tc` program name) for a shape command:
 *   qdisc replace dev <iface> root netem [delay <d>ms [<j>ms]] [loss <l>%] [rate <r>kbit] [limit <n>]
 *
 * `replace` (not `add`) makes the call idempotent — re-applying a
 * profile overwrites the existing qdisc instead of erroring.
 */
export function buildNetemShapeArgs(iface: string, params: NetemParams): string[] {
  assertIface(iface);
  const args = ["qdisc", "replace", "dev", iface, "root", "netem"];
  if (params.delayMs !== undefined) {
    args.push("delay", `${params.delayMs}ms`);
    if (params.jitterMs !== undefined) {
      args.push(`${params.jitterMs}ms`);
    }
  }
  if (params.lossPct !== undefined) {
    args.push("loss", `${params.lossPct}%`);
  }
  if (params.rateKbit !== undefined) {
    args.push("rate", `${params.rateKbit}kbit`);
  }
  if (params.limitPkts !== undefined) {
    args.push("limit", `${params.limitPkts}`);
  }
  return args;
}

/**
 * Build the argv (excluding `tc`) for clearing the root qdisc:
 *   qdisc del dev <iface> root
 */
export function buildNetemClearArgs(iface: string): string[] {
  assertIface(iface);
  return ["qdisc", "del", "dev", iface, "root"];
}

export const NETEM_IFB_DEV = "ifb0";

/**
 * Device queue depth for the mirror, packets. A fresh `ifb` comes up at 32,
 * which sits AHEAD of the netem qdisc and would bind before its `limit`.
 */
export const NETEM_IFB_TXQUEUELEN = 1000;

export interface NetemCommand {
  file: "tc" | "ip";
  args: string[];
  tolerateFailure?: true;
}

export const NETEM_SETPRIV_DEFAULT = "/usr/local/bin/netem-setpriv";
export const NETEM_SETPRIV_ARGS = [
  "--inh-caps",
  "+net_admin",
  "--ambient-caps",
  "+net_admin",
  "--",
] as const;

/** Same resolution as the entrypoint's `${NETEM_SETPRIV:-…}`: empty means default. */
export function netemSetprivPath(env: NodeJS.ProcessEnv = process.env): string {
  return env.NETEM_SETPRIV || NETEM_SETPRIV_DEFAULT;
}

/** `ip` cannot hold CAP_NET_ADMIN from its file capability alone, so it runs under setpriv. */
export function netemExecArgv(cmd: NetemCommand, setpriv: string): [string, string[]] {
  return cmd.file === "ip"
    ? [setpriv, [...NETEM_SETPRIV_ARGS, "ip", ...cmd.args]]
    : [cmd.file, cmd.args];
}

/**
 * The ingress half of a profile: downlink rate and depth in place of uplink, the rest
 * symmetric. `rateKbit` here would tighten the downlink 2–6.7x. Throws if either
 * is missing: a mirror at the wrong rate or depth mislabels the receipt.
 */
export function ingressNetemParams(params: NetemParams): NetemParams {
  const { downlinkRateKbit, ingressLimitPkts, ...rest } = params;
  if (downlinkRateKbit === undefined || ingressLimitPkts === undefined) {
    throw new NetemValidationError(
      "cannot shape ingress without a downlinkRateKbit and an ingressLimitPkts (see NETEM_PROFILES)",
    );
  }
  return { ...rest, rateKbit: downlinkRateKbit, limitPkts: ingressLimitPkts };
}

/**
 * Install commands in order. `ip link add` runs ONLY when the `ip link show`
 * before it fails; the hook is deleted then re-added, never replaced.
 */
export function buildNetemMirrorInstallArgs(iface: string, params: NetemParams): NetemCommand[] {
  assertIface(iface);
  const ingress = ingressNetemParams(params);
  return [
    { file: "ip", args: ["link", "show", NETEM_IFB_DEV] },
    { file: "ip", args: ["link", "add", NETEM_IFB_DEV, "type", "ifb"] },
    { file: "ip", args: ["link", "set", NETEM_IFB_DEV, "up"] },
    {
      file: "ip",
      args: ["link", "set", NETEM_IFB_DEV, "txqueuelen", `${NETEM_IFB_TXQUEUELEN}`],
    },
    { file: "tc", args: ["qdisc", "del", "dev", iface, "ingress"], tolerateFailure: true },
    { file: "tc", args: ["qdisc", "add", "dev", iface, "handle", "ffff:", "ingress"] },
    {
      file: "tc",
      args: [
        ...["filter", "add", "dev", iface, "parent", "ffff:", "protocol", "all"],
        ...["u32", "match", "u32", "0", "0"],
        ...["action", "mirred", "egress", "redirect", "dev", NETEM_IFB_DEV],
      ],
    },
    { file: "tc", args: buildNetemShapeArgs(NETEM_IFB_DEV, ingress) },
  ];
}

export const NETEM_MIRROR_ADD_STEP = 1;

/** Teardown order: hook first, and its exit status gates the `ifb` steps. */
export function buildNetemMirrorClearArgs(iface: string): NetemCommand[] {
  assertIface(iface);
  return [
    { file: "tc", args: ["qdisc", "del", "dev", iface, "ingress"] },
    { file: "tc", args: buildNetemClearArgs(NETEM_IFB_DEV) },
    { file: "ip", args: ["link", "del", NETEM_IFB_DEV] },
  ];
}

export function buildNetemProbeArgs(iface: string): string[] {
  assertIface(iface);
  return ["qdisc", "show", "dev", iface];
}

export const NETEM_INGRESS_QDISC_MARKER = "qdisc ingress";

/** Lowercased `tc qdisc del` stderr meaning "already clean". Wording only. */
export const NETEM_BENIGN_CLEAR_ERRORS = ["cannot delete", "no such file"] as const;

export function isBenignClearError(message: string): boolean {
  const msg = message.toLowerCase();
  return NETEM_BENIGN_CLEAR_ERRORS.some((needle) => msg.includes(needle));
}

/** Lowercased `ip link del` stderr meaning the device is already gone. Wording only. */
export const NETEM_BENIGN_LINK_DEL_ERRORS = ["cannot find device", "does not exist"] as const;

export function isBenignLinkDelError(message: string): boolean {
  const msg = message.toLowerCase();
  return NETEM_BENIGN_LINK_DEL_ERRORS.some((needle) => msg.includes(needle));
}

export class NetemStateError extends Error {
  readonly result: NetemApplyResult;

  constructor(message: string, result: NetemApplyResult) {
    super(message);
    this.name = "NetemStateError";
    this.result = result;
  }
}

const NETEM_QDISC_MARKER = "qdisc netem";

export function netemActionShapesIngress(
  action: NetemAction,
): action is Extract<NetemAction, { op: "shape" }> {
  return (
    action.op === "shape" &&
    action.params.downlinkRateKbit !== undefined &&
    action.params.ingressLimitPkts !== undefined
  );
}

/** Shapes with both downlink fields install or replace the mirror; every other action removes it. */
export async function applyNetemAction(
  action: NetemAction,
  deps: { iface: string; exec: NetemExec; setpriv?: string },
): Promise<NetemApplyResult> {
  const { iface, exec } = deps;
  const setpriv = deps.setpriv ?? netemSetprivPath();
  const commands: string[][] = [];

  const run = async (cmd: NetemCommand): Promise<{ stdout: string; stderr: string }> => {
    const [file, args] = netemExecArgv(cmd, setpriv);
    commands.push([file, ...args]);
    return exec(file, args);
  };
  const attempt = async (cmd: NetemCommand): Promise<boolean> => {
    try {
      await run(cmd);
      return true;
    } catch (e) {
      if (!netemExecExitedNonZero(e)) throw e;
      return false;
    }
  };

  const probe = async (dev: string, strict: boolean): Promise<string> => {
    try {
      return (await run({ file: "tc", args: buildNetemProbeArgs(dev) })).stdout.trim();
    } catch (e) {
      if (strict && !netemExecExitedNonZero(e)) throw e;
      return `unread: ${(e as Error).message}`;
    }
  };
  const bestEffortReadback = async (): Promise<Record<string, string>> => ({
    [iface]: await probe(iface, false),
    [NETEM_IFB_DEV]: await probe(NETEM_IFB_DEV, false),
  });

  const wantIngress = netemActionShapesIngress(action);
  let mirrorRemoved = false;
  const result = (readback: Record<string, string>): NetemApplyResult => ({
    commands,
    label: action.label,
    op: action.op,
    ingressShaped: wantIngress,
    mirrorRemoved,
    readback,
  });

  if (action.op === "shape") {
    await run({ file: "tc", args: buildNetemShapeArgs(iface, action.params) });
  } else {
    try {
      await run({ file: "tc", args: buildNetemClearArgs(iface) });
    } catch (e) {
      if (!netemExecExitedNonZero(e) || !isBenignClearError(e.message)) throw e;
    }
  }

  if (netemActionShapesIngress(action)) {
    const [show, add, ...rest] = buildNetemMirrorInstallArgs(iface, action.params);
    try {
      if (!(await attempt(show))) await run(add);
      for (const step of rest) {
        if (step.tolerateFailure) mirrorRemoved = await attempt(step);
        else await run(step);
        if (step.args.includes("mirred")) mirrorRemoved = false;
      }
    } catch (e) {
      throw new NetemStateError(
        `netem shape (${action.label}) shaped egress on ${iface} but the ${NETEM_IFB_DEV} ingress mirror failed to install: ${(e as Error).message}`,
        { ...result(await bestEffortReadback()), ingressShaped: false },
      );
    }
  } else {
    const [hook, ifbQdisc, ifbLink] = buildNetemMirrorClearArgs(iface);
    if (await attempt(hook)) {
      await attempt(ifbQdisc);
      try {
        await run(ifbLink);
      } catch (e) {
        if (!netemExecExitedNonZero(e) || !isBenignLinkDelError(e.message)) {
          throw new NetemStateError(
            `netem ${action.op} (${action.label}) removed the ingress hook on ${iface} but ip ${ifbLink.args.join(" ")} failed, so ${NETEM_IFB_DEV} was NOT removed: ${(e as Error).message}`,
            result(await bestEffortReadback()),
          );
        }
      }
      mirrorRemoved = true;
    }
  }

  const readback: Record<string, string> = {};
  readback[iface] = (await run({ file: "tc", args: buildNetemProbeArgs(iface) })).stdout.trim();
  if (wantIngress) readback[NETEM_IFB_DEV] = await probe(NETEM_IFB_DEV, true);

  const eth = readback[iface];
  const wrong: string[] = [];
  if (action.op === "shape" && !eth.includes(NETEM_QDISC_MARKER)) {
    wrong.push(`no netem on ${iface}`);
  }
  if (action.op === "clear" && eth.includes(NETEM_QDISC_MARKER)) {
    wrong.push(`a netem qdisc left on ${iface}`);
  }
  if (wantIngress) {
    if (!eth.includes(NETEM_INGRESS_QDISC_MARKER)) wrong.push(`no ingress hook on ${iface}`);
    if (!readback[NETEM_IFB_DEV].includes(NETEM_QDISC_MARKER)) {
      wrong.push(`no netem on ${NETEM_IFB_DEV}`);
    }
  } else if (eth.includes(NETEM_INGRESS_QDISC_MARKER)) {
    wrong.push(`an ingress mirror left on ${iface}`);
  }
  if (wrong.length > 0) {
    const read = Object.entries(readback)
      .map(([dev, out]) => `${dev}=[${out}]`)
      .join(" ");
    throw new NetemStateError(
      `netem ${action.op} (${action.label}) post-read found ${wrong.join(" and ")}; read back ${read}`,
      { ...result(readback), ingressShaped: false },
    );
  }
  return result(readback);
}
