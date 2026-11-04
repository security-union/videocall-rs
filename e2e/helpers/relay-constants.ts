import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const RELAY_CONSTANTS = resolve(__dirname, "../../actix-api/src/constants.rs");

export function rustDurationMs(src: string, name: string): number {
  const re = new RegExp(
    `^pub const ${name}: Duration = Duration::from_(secs|millis)\\(([\\d_]+)\\);`,
    "gm",
  );
  const hits = [...src.matchAll(re)];
  if (hits.length !== 1) {
    throw new Error(`expected exactly 1 \`${name}\` Duration literal, found ${hits.length}`);
  }
  const n = Number(hits[0][2].replace(/_/g, ""));
  return hits[0][1] === "secs" ? n * 1000 : n;
}

/** `RECONNECT_GRACE_PERIOD` in actix-api/src/constants.rs, read from source. */
export function relayReconnectGraceMs(): number {
  return rustDurationMs(readFileSync(RELAY_CONSTANTS, "utf8"), "RECONNECT_GRACE_PERIOD");
}
