import { NETSIM_PRESETS } from "./constants";
import { isValidTtl } from "./ttl";

/**
 * Light client-side validation. The Node sidecar runs the canonical
 * validation again on POST /api/launch — these checks just give the
 * operator fast feedback before the request is sent.
 */

export interface LaunchFormValues {
  meetingURL: string;
  participant: string;
  displayName: string;
  ttl: string;
  network: string;
  headless: boolean;
  authBackend: "jwt" | "storage-state" | "none";
  storageStateFile: string;
  runLocation: string;
  /**
   * When `runLocation === "ssh"`, the label of the registered host the
   * operator picked. The form sets this empty string when the
   * dropdown has no value chosen yet. Validation rejects an empty
   * string only when SSH mode is active.
   */
  sshHostLabel?: string;
  costume: string;
  audio: string;
}

export interface FieldErrors {
  meetingURL?: string;
  participant?: string;
  ttl?: string;
  network?: string;
  storageStateFile?: string;
  runLocation?: string;
  sshHostLabel?: string;
}

export function isValidMeetingUrl(value: string): boolean {
  if (value.trim() === "") return false;
  try {
    const url = new URL(value);
    if (url.protocol !== "http:" && url.protocol !== "https:") return false;
    return url.pathname.includes("/meeting/");
  } catch {
    return false;
  }
}

export function isValidParticipant(value: string): boolean {
  // Same regex as the JWT-cookie path uses; allow handles or emails.
  // Reject whitespace and the empty string.
  const v = value.trim();
  if (v === "") return false;
  return /^[A-Za-z0-9._@+-]+$/.test(v);
}

export function validateLaunchForm(values: LaunchFormValues): FieldErrors {
  const errors: FieldErrors = {};
  if (!isValidMeetingUrl(values.meetingURL)) {
    errors.meetingURL = "Meeting URL must be a full http(s) URL with a /meeting/<id> path";
  }
  if (!isValidParticipant(values.participant)) {
    errors.participant = "Participant must be a non-empty handle or email";
  }
  if (!isValidTtl(values.ttl)) {
    errors.ttl = `TTL must be "<int>s|m|h" or "infinite" (got "${values.ttl}")`;
  }
  if (!NETSIM_PRESETS.includes(values.network as (typeof NETSIM_PRESETS)[number])) {
    errors.network = `Network must be one of: ${NETSIM_PRESETS.join(", ")}`;
  }
  if (values.authBackend === "storage-state" && values.storageStateFile.trim() === "") {
    errors.storageStateFile = "Storage-state file path is required when auth=storage-state";
  }
  if (values.runLocation !== "local" && values.runLocation !== "ssh") {
    errors.runLocation = "Only Local machine and SSH-able host are wired today";
  }
  if (values.runLocation === "ssh") {
    const label = (values.sshHostLabel ?? "").trim();
    if (label === "") {
      errors.sshHostLabel = "Pick a registered SSH host";
    }
  }
  return errors;
}
