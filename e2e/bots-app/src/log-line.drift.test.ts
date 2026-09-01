import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { botsAppLine, conductLine, sanitizeLogLine, taggedLine } from "./log-line";

const ENTRYPOINT = fileURLToPath(new URL("../docker-entrypoint.sh", import.meta.url));

/** The real `say()` definition, lifted from the production entrypoint. */
function sayDefinition(): string {
  const def = readFileSync(ENTRYPOINT, "utf8")
    .split("\n")
    .find((l) => l.startsWith("say() {") && l.endsWith("}"));
  if (def === undefined) {
    throw new Error("docker-entrypoint.sh no longer defines say() on one line — update this lock");
  }
  return def;
}

/** What the entrypoint's own `say()` writes for a value, minus the trailing newline. */
function shellSay(value: string): string {
  const res = spawnSync("bash", ["-c", `${sayDefinition()}\nsay "$1"`, "bash", value], {
    encoding: "utf8",
  });
  expect(res.status, res.stderr).toBe(0);
  return (res.stdout ?? "").replace(/\n$/, "");
}

describe("sanitizeLogLine ↔ docker-entrypoint.sh say() (#2375)", () => {
  const CASES: Array<[string, string]> = [
    ["LF", "url=x\ndocker-entrypoint: forged"],
    ["CR", "url=x\rdocker-entrypoint: forged"],
    ["CRLF", "url=x\r\ndocker-entrypoint: forged"],
    ["several", "a\nb\rc\r\nd"],
    ["leading and trailing", "\nmiddle\r"],
    ["no control characters", "url=https://example.test/meeting/room ttl=infinite"],
    ["percent and backslash", "100%s of \\n paths"],
  ];

  it.each(CASES)("collapses a %s value identically in both writers", (_label, value) => {
    expect(sanitizeLogLine(value)).toBe(shellSay(value));
  });

  it("keeps the forged tail on the one line, in both writers", () => {
    const value = "url=x\ndocker-entrypoint: forged";
    expect(sanitizeLogLine(value)).toContain("docker-entrypoint: forged");
    expect(sanitizeLogLine(value).split(/[\r\n]/)).toHaveLength(1);
    expect(shellSay(value).split(/[\r\n]/)).toHaveLength(1);
  });

  it.each([
    ["botsAppLine", botsAppLine, "bots-app: "],
    ["conductLine", conductLine, "conduct: "],
  ])("%s collapses the whole composed line, marker included", (_name, compose, marker) => {
    const line = compose(`--port abc\n${marker}forged`);
    expect(line.split(/[\r\n]/)).toHaveLength(1);
    expect(line.startsWith(marker)).toBe(true);
    expect(line.indexOf(marker, marker.length)).toBeGreaterThan(0);
  });

  it("collapses control characters coming from the label as well as the message", () => {
    const line = taggedLine("bot-0\ndocker-entrypoint: forged", "auth: guest\rmore");
    expect(line.split(/[\r\n]/)).toHaveLength(1);
    expect(line).toContain("docker-entrypoint: forged");
    expect(line).toContain("more");
  });
});
