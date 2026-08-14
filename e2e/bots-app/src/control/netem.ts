import { execFile } from "node:child_process";

/**
 * OS-level network impairment for a single-bot pod, via Linux `tc` +
 * `netem`. This is DISTINCT from the client-side `?netsim=` feature
 * (which shapes traffic inside the browser via a WASM shim): `netem`
 * shapes the pod's real kernel network interface, so it also impairs
 * the QUIC/WebTransport and TCP/WebSocket handshakes, TLS, DNS — the
 * whole stack — the way a real degraded link does.
 *
 * Requirements at runtime: the container must have `NET_ADMIN` and the
 * `iproute2` package (provides `tc`). Both are supplied by the pod /
 * image in the deploy task; this module only builds + runs the command.
 *
 * SECURITY: every `tc` invocation goes through {@link execFile} with an
 * argv ARRAY and no shell, so no request-supplied value is ever parsed
 * by a shell. Numeric parameters are validated + re-formatted by us
 * (never passed through verbatim), and the interface name is validated
 * against {@link IFACE_PATTERN}. There is no code path that interpolates
 * untrusted text into a shell string.
 *
 * NOTE — direction: `tc qdisc ... dev <iface> root netem` shapes the
 * interface's EGRESS (outbound) queue. That is the bot's uplink — the
 * direction that carries its published camera/screen media — which is
 * what a load test wants to impair. Inbound shaping would require an
 * `ifb` mirror and is intentionally out of scope here.
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
export const IFACE_PATTERN = /^[A-Za-z0-9._-]{1,15}$/;

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
}

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
 * (uplink direction, since root netem shapes egress) so operators use
 * ONE impairment vocabulary across the client `?netsim=` shim and the
 * OS-level tc path. `null` means "no shaping" ⇒ clear the qdisc.
 *
 * `clean` and `none` are aliases for the clear operation (`none` matches
 * the netsim preset name; `clean` matches the brief's naming).
 */
export const NETEM_PROFILES: Readonly<Record<string, NetemParams | null>> = {
  clean: null,
  none: null,
  good_wifi: { delayMs: 20, jitterMs: 5, lossPct: 0.1, rateKbit: 20_000 },
  good_4g: { delayMs: 50, jitterMs: 15, lossPct: 0.5, rateKbit: 10_000 },
  congested_wifi: { delayMs: 80, jitterMs: 30, lossPct: 2, rateKbit: 2_000 },
  lossy_mobile: { delayMs: 150, jitterMs: 50, lossPct: 5, rateKbit: 800 },
  satellite: { delayMs: 600, jitterMs: 50, lossPct: 1, rateKbit: 1_500 },
  dialup: { delayMs: 200, jitterMs: 40, lossPct: 3, rateKbit: 56 },
};

/** Stable list of profile names for CLI help / error messages. */
export const NETEM_PROFILE_NAMES: readonly string[] = Object.keys(NETEM_PROFILES);

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

/**
 * Result of running a netem action: the exact argv handed to `tc` (for
 * logging / the API response) and the resolved label.
 */
export interface NetemApplyResult {
  /** Full argv including the `tc` program name at index 0. */
  argv: string[];
  label: string;
  op: "shape" | "clear";
}

/**
 * Injectable process runner. Production uses {@link defaultNetemExec}
 * (a thin `execFile` wrapper); tests inject a recorder so no real `tc`
 * ever runs. Mirrors the `vpnFetch` / `ssoCaptureFactory` seam pattern
 * already used by the control server.
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
          reject(new Error(`tc ${args.join(" ")} failed: ${detail}`));
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
    // Cap BELOW 100: netem shapes the pod's own eth0 EGRESS, which also carries
    // the control server's responses. 100% loss would strand the control API
    // (the DELETE /netem that clears it can't get through) — recoverable only
    // by out-of-band `kubectl exec … tc qdisc del`. 95 keeps the channel usable.
    params.lossPct = assertFiniteNonNegative(raw.lossPct as number, "lossPct", 95);
  }
  if (raw.rateKbit !== undefined && raw.rateKbit !== null) {
    const rate = assertFiniteNonNegative(raw.rateKbit as number, "rateKbit", 10_000_000);
    if (rate < 8) {
      // Same self-strand concern: a near-zero rate chokes the control API's own
      // responses. Floor at 8 kbit so the pod stays reachable to be cleared.
      throw new NetemValidationError(
        '"rateKbit" must be >= 8 when provided (lower can strand the control API)',
      );
    }
    params.rateKbit = rate;
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
 *   - `{ delayMs?, jitterMs?, lossPct?, rateKbit? }` — raw params
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
  const hasRawParam =
    o.delayMs !== undefined ||
    o.jitterMs !== undefined ||
    o.lossPct !== undefined ||
    o.rateKbit !== undefined;

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
    if (!(o.profile in NETEM_PROFILES)) {
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
}

/**
 * Build the argv (excluding the `tc` program name) for a shape command:
 *   qdisc replace dev <iface> root netem [delay <d>ms [<j>ms]] [loss <l>%] [rate <r>kbit]
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

/**
 * Run a resolved {@link NetemAction} against `iface` using the injected
 * `exec`. A `clear` on an interface that has no qdisc makes `tc` exit
 * non-zero ("Cannot delete qdisc with handle of zero" / "No such file");
 * we swallow that specific idempotency case so repeated clears succeed.
 */
export async function applyNetemAction(
  action: NetemAction,
  deps: { iface: string; exec: NetemExec },
): Promise<NetemApplyResult> {
  const { iface, exec } = deps;
  if (action.op === "shape") {
    const args = buildNetemShapeArgs(iface, action.params);
    await exec("tc", args);
    return { argv: ["tc", ...args], label: action.label, op: "shape" };
  }
  const args = buildNetemClearArgs(iface);
  try {
    await exec("tc", args);
  } catch (e) {
    // Clearing an already-clean interface is not an error we surface —
    // the desired end state (no shaping) is achieved either way.
    const msg = (e as Error).message.toLowerCase();
    const benign =
      msg.includes("cannot delete") ||
      msg.includes("no such file") ||
      msg.includes("rtnetlink answers: no such file");
    if (!benign) throw e;
  }
  return { argv: ["tc", ...args], label: action.label, op: "clear" };
}
