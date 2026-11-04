import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { parseSession } from "./parse";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..", "..");
const EDGE_REL = "videocall-client/src/decode/speaking_edge.rs";
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const ASSIGNED_REL = "videocall-client/src/client/video_call_client.rs";
const RENAME_REL = "dioxus-ui/src/components/display_name_edit.rs";

describe("[PEER_SPEAKING] drift lock", () => {
  it("parses the line speaking_edge.rs formats", () => {
    const src = readFileSync(resolve(REPO_ROOT, EDGE_REL), "utf8");
    const fmt = /"(\[PEER_SPEAKING\][^"]*)"/.exec(src);
    expect(fmt, "speaking_edge.rs no longer formats a [PEER_SPEAKING] line").not.toBeNull();
    expect(src).toMatch(/let state = if self\.speaking \{ "on" \} else \{ "off" \};/);
    const line = fmt![1]
      .replace("{state}", "on")
      .replace("{peer_id}", "123")
      .replace("{}", "1791568469224");
    expect(line, "the format gained a field this parser does not read").not.toMatch(/[{}]/);
    const log = parseSession("s", [
      JSON.stringify({
        seq: 0,
        ts: "2026-10-09T17:00:00Z",
        msg: "RENAME: API CALL INITIATED for: bot-1 (session_id: Some(9))",
      }),
      JSON.stringify({ seq: 1, ts: "2026-10-09T17:00:01Z", msg: line }),
    ]);
    expect(log.peerSpeakingLines).toBe(1);
    expect(log.onEdges.get("123")).toEqual([1791568469224]);
  });

  it("parses the session and rename lines the client formats", () => {
    const assigned = /"(Received SESSION_ASSIGNED: session_id=\{\})"/.exec(
      readFileSync(resolve(REPO_ROOT, ASSIGNED_REL), "utf8"),
    );
    const rename =
      /"(RENAME: API CALL INITIATED for: \{valid_name\} \(session_id: \{session_id:\?\}\))"/.exec(
        readFileSync(resolve(REPO_ROOT, RENAME_REL), "utf8"),
      );
    expect(assigned, "video_call_client.rs no longer logs the session id this way").not.toBeNull();
    expect(rename, "display_name_edit.rs no longer logs the rename this way").not.toBeNull();
    const log = parseSession("s", [
      JSON.stringify({ seq: 0, ts: "2026-10-09T17:00:00Z", msg: assigned![1].replace("{}", "77") }),
      JSON.stringify({
        seq: 1,
        ts: "2026-10-09T17:00:01Z",
        msg: rename![1].replace("{valid_name}", "bot-3").replace("{session_id:?}", "Some(9)"),
      }),
    ]);
    expect(log.ordinal).toBe(3);
    expect([...log.sessionIds].sort()).toEqual(["77", "9"]);
  });

  it("the lint workflow re-runs this lock when a source it reads changes", () => {
    const wf = parseYaml(readFileSync(resolve(REPO_ROOT, PR_CHECK_REL), "utf8")) as Record<
      string,
      unknown
    >;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    for (const rel of [EDGE_REL, ASSIGNED_REL, RENAME_REL]) {
      expect(on?.pull_request?.paths ?? []).toContain(rel);
    }
  });
});
