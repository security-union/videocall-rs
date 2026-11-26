/**
 * The device half of `videocall-cli auth login`, reimplemented with Node
 * `crypto` so E2E tests can drive meeting-api's device pages without the CLI.
 *
 * Wire formats mirror `videocall-meeting-types/src/device.rs` and
 * `meeting-api/src/device_auth.rs`:
 * - public key: base64url (no padding) of the 32 raw Ed25519 bytes
 * - fingerprint: first 8 bytes of SHA-256(raw key), uppercase hex, `XXXX-XXXX-XXXX-XXXX`
 * - registration JWS / device JWT: EdDSA, `iss: videocall-cli`, `aud: <API origin>`
 */
import { createHash, generateKeyPairSync, randomBytes, sign, KeyObject } from "crypto";

/** meeting-api's public origin. Must equal the stack's DEVICE_TOKEN_AUDIENCE. */
export const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

/** The remote-login redirect_uri: `{api origin}/cli/code`. */
export const REMOTE_REDIRECT_URI = `${API_URL}/cli/code`;

const CLI_ISSUER = "videocall-cli";

export interface DeviceKey {
  privateKey: KeyObject;
  /** base64url of the 32 raw public key bytes, as `/cli/authorize` expects. */
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
  state?: string;
  redirectUri?: string;
}

export function authorizeUrl(params: AuthorizeParams): string {
  const query = new URLSearchParams({
    response_type: "code",
    client_id: "videocall-cli",
    redirect_uri: params.redirectUri ?? REMOTE_REDIRECT_URI,
    code_challenge: params.codeChallenge,
    code_challenge_method: "S256",
    state: params.state ?? randomBytes(16).toString("base64url"),
    public_key: params.publicKey,
    device_name: params.deviceName,
  });
  return `${API_URL}/cli/authorize?${query.toString()}`;
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
