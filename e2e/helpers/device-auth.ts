/**
 * The device half of `videocall-cli auth login`, reimplemented with Node
 * `crypto` so E2E tests can drive the device login without the CLI.
 *
 * Wire formats mirror `videocall-meeting-types/src/device.rs` and
 * `meeting-api/src/device_auth.rs`:
 * - public key: base64url (no padding) of the 32 raw Ed25519 bytes
 * - fingerprint: first 8 bytes of SHA-256(raw key), uppercase hex, `XXXX-XXXX-XXXX-XXXX`
 * - registration JWS / device JWT: EdDSA, `iss: videocall-cli`, `aud: <API origin>`
 */
import { createHash, generateKeyPairSync, randomBytes, sign, KeyObject } from "crypto";
import { createServer, type IncomingHttpHeaders } from "http";
import type { AddressInfo } from "net";

/** meeting-api's public origin. Must equal the stack's DEVICE_TOKEN_AUDIENCE. */
export const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

/** The app origin, which hosts /devices and /devices/authorize. */
export const UI_URL = process.env.DIOXUS_UI_URL || "http://localhost:3001";

/** The remote-login redirect_uri: the app shows the code instead of redirecting. */
export const REMOTE_REDIRECT_URI = "urn:ietf:wg:oauth:2.0:oob";

const CLI_ISSUER = "videocall-cli";

export interface DeviceKey {
  privateKey: KeyObject;
  /** base64url of the 32 raw public key bytes, as the authorize link carries it. */
  publicKey: string;
  /** `XXXX-XXXX-XXXX-XXXX`, what the consent page and /devices display. */
  fingerprint: string;
}

export function generateDeviceKey(): DeviceKey {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const jwk = publicKey.export({ format: "jwk" });
  if (!jwk.x) throw new Error("Ed25519 JWK export has no x coordinate");
  const raw = Buffer.from(jwk.x, "base64url");
  return { privateKey, publicKey: jwk.x, fingerprint: fingerprintOf(raw) };
}

export function fingerprintOf(rawPublicKey: Buffer): string {
  const hex = createHash("sha256").update(rawPublicKey).digest("hex").slice(0, 16).toUpperCase();
  return hex.match(/.{4}/g)!.join("-");
}

export interface Pkce {
  verifier: string;
  challenge: string;
}

export function generatePkce(): Pkce {
  const verifier = randomBytes(32).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  return { verifier, challenge };
}

/** The CLI's `state`: 32 random bytes, base64url. */
export function generateState(): string {
  return randomBytes(32).toString("base64url");
}

/** A device name inside the allowed alphabet, unique per call. */
export function uniqueDeviceName(prefix = "e2e-cli"): string {
  return `${prefix}-${randomBytes(4).toString("hex")}`;
}

/** A session subject unique per call, so device lists never leak between tests. */
export function uniqueEmail(prefix = "e2e-device"): string {
  return `${prefix}-${Date.now()}-${randomBytes(3).toString("hex")}@videocall.rs`;
}

export interface AuthorizeParams {
  publicKey: string;
  deviceName: string;
  codeChallenge: string;
  state: string;
  redirectUri: string;
}

/** The query the CLI puts on its authorize link. */
export function authorizeQuery(params: AuthorizeParams): string {
  return new URLSearchParams({
    response_type: "code",
    client_id: "videocall-cli",
    redirect_uri: params.redirectUri,
    code_challenge: params.codeChallenge,
    code_challenge_method: "S256",
    state: params.state,
    public_key: params.publicKey,
    device_name: params.deviceName,
  }).toString();
}

function signJws(
  privateKey: KeyObject,
  header: Record<string, unknown>,
  claims: Record<string, unknown>,
): string {
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const signingInput = `${encode(header)}.${encode(claims)}`;
  const signature = sign(null, Buffer.from(signingInput), privateKey).toString("base64url");
  return `${signingInput}.${signature}`;
}

function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

/** The self-signed registration JWS sent to `POST /api/v1/devices`. */
export function registrationJws(key: DeviceKey, deviceName: string): string {
  const iat = nowSecs();
  return signJws(
    key.privateKey,
    { alg: "EdDSA", typ: "JWT" },
    {
      iss: CLI_ISSUER,
      aud: API_URL,
      iat,
      exp: iat + 120,
      public_key: key.publicKey,
      name: deviceName,
    },
  );
}

/** A short-lived device JWT, as the CLI sends on every API call. */
export function deviceJwt(key: DeviceKey, deviceId: string): string {
  const iat = nowSecs();
  return signJws(
    key.privateKey,
    { alg: "EdDSA", typ: "JWT", kid: deviceId },
    {
      iss: CLI_ISSUER,
      sub: deviceId,
      aud: API_URL,
      iat,
      exp: iat + 120,
      jti: randomBytes(16).toString("base64url"),
    },
  );
}

export interface CallbackHit {
  /** Path and query, e.g. `/callback?code=…&state=…`. */
  url: string;
  headers: IncomingHttpHeaders;
}

export interface LoopbackListener {
  /** `http://127.0.0.1:{port}/callback`, the loopback redirect_uri. */
  redirectUri: string;
  /** Resolves with the first request the listener receives. */
  firstHit: Promise<CallbackHit>;
  close(): Promise<void>;
}

/**
 * The CLI's loopback listener: an HTTP server on 127.0.0.1 and a free port
 * that records the first request and answers with a small page.
 */
export async function startLoopbackListener(): Promise<LoopbackListener> {
  let resolveHit!: (hit: CallbackHit) => void;
  const firstHit = new Promise<CallbackHit>((resolve) => (resolveHit = resolve));
  const server = createServer((req, res) => {
    resolveHit({ url: req.url ?? "", headers: req.headers });
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end("<!doctype html><title>CLI listener</title><h1>CLI listener reached</h1>");
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    redirectUri: `http://127.0.0.1:${port}/callback`,
    firstHit,
    close: () =>
      new Promise<void>((resolve) => {
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
