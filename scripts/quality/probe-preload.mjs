// node --import tsx --import /cq/preload.mjs: hands the signing key from /cfg to e2e/helpers/auth.ts without it ever
// being in an exec environment (cq_remote.py RemoteFleet._launch_probe).
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";

const stop = (why) => {
  process.stderr.write(`cq probe preload: ${why}; refusing to start\n`);
  process.exit(3);
};
let key = "";
try {
  key = readFileSync("/cfg/probe-secret", "utf8");
} catch {
  key = "";
}
if (key.length < 8) stop("/cfg/probe-secret is missing or short");
const require = createRequire("/app/e2e/bots-app/src/bot.ts");
writeFileSync("/tmp/cq-warm.ts", "export const warm: number = 1;\n");
require("/tmp/cq-warm.ts");
process.env.JWT_SECRET = key;
let auth;
try {
  auth = require("../../helpers/auth");
} finally {
  delete process.env.JWT_SECRET;
}
try {
  require("jsonwebtoken").verify(auth.generateSessionToken("cq-preload-check@bots-app.local", "cq"), key);
} catch {
  stop("helpers/auth.ts did not take the key");
}
key = "";
writeFileSync("/out/.cq-key-read", "");
