import { readdirSync, readFileSync } from "node:fs";
import { relative, resolve } from "node:path";

import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

import * as MIRRORS from "./rust-mirrored-constants";
import { RUST_MIRRORS } from "./rust-mirrored-constants";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..");
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const PR_CHECK = resolve(REPO_ROOT, PR_CHECK_REL);
const HELPER_REL = "e2e/helpers/rust-mirrored-constants.ts";

/**
 * Value of a top-level `const NAME: ty = <number>;` in `rel`. Column-0 anchored,
 * so a doc-comment mention or function-local cannot satisfy it; throws on 0 or
 * 2+ matches and on a non-literal rather than returning a fallback.
 */
function rustConst(rel: string, name: string): number {
  const abs = resolve(REPO_ROOT, rel);
  let src: string;
  try {
    src = readFileSync(abs, "utf8");
  } catch (err) {
    throw new Error(
      `cannot read ${rel} (resolved ${abs}) while locking ${name}. ` +
        `If the file moved, update RUST_MIRRORS in ${HELPER_REL}.`,
      { cause: err },
    );
  }
  const re = new RegExp(`^(?:pub )?const ${name}\\s*:\\s*[A-Za-z0-9_]+\\s*=\\s*([^;]+);`, "gm");
  const hits = [...src.matchAll(re)];
  if (hits.length !== 1) {
    throw new Error(
      `expected exactly 1 top-level \`const ${name}\` in ${rel}, found ${hits.length}. ` +
        `Renamed, moved, or redefined — update RUST_MIRRORS in ${HELPER_REL}.`,
    );
  }
  const raw = hits[0][1].trim().replace(/_/g, "");
  if (!/^-?\d+(\.\d+)?$/.test(raw)) {
    throw new Error(
      `\`const ${name}\` in ${rel} is \`${raw}\`, not a plain number literal. ` +
        `This lock can only mirror a literal; express ${name} as one or drop it from RUST_MIRRORS.`,
    );
  }
  return Number(raw);
}

const CASES = Object.entries(RUST_MIRRORS).flatMap(([rel, consts]) =>
  Object.entries(consts).map(([name, mirrored]) => ({ rel, name, mirrored })),
);
const LOCKED = new Set(CASES.map((c) => c.name));

describe("Rust constants mirrored into e2e specs (#2377)", () => {
  it("locks at least one constant, or the manifest emptied out unnoticed", () => {
    expect(CASES.length).toBeGreaterThan(0);
  });

  it.each(CASES)("$rel :: $name matches the mirror", ({ rel, name, mirrored }) => {
    expect(
      rustConst(rel, name),
      `${name} in ${rel} has drifted from the mirror in ${HELPER_REL}. ` +
        `Update the mirror to the Rust value, then re-check every spec assertion derived from it.`,
    ).toBe(mirrored);
  });

  /** A numeric export absent from RUST_MIRRORS is a mirror with no Rust symbol behind it. */
  it("locks every numeric export the helper publishes", () => {
    const unlocked = Object.entries(MIRRORS)
      .filter(([, v]) => typeof v === "number")
      .map(([name]) => name)
      .filter((name) => !LOCKED.has(name));
    expect(unlocked, `add these to RUST_MIRRORS in ${HELPER_REL}, or they are unguarded`).toEqual(
      [],
    );
  });

  it("re-runs on a change to any mirrored Rust file, or a retune lands with this lock unread", () => {
    const wf = parseYaml(readFileSync(PR_CHECK, "utf8")) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    // `e2e/**` is what runs this lock on a mirror-only edit; the workflow's own
    // path is what makes a dropped pin reviewable. Neither is optional.
    for (const rel of [...Object.keys(RUST_MIRRORS), "e2e/**", PR_CHECK_REL]) {
      expect(paths, `pr-check-e2e-lint-hcl.yaml must trigger on ${rel}`).toContain(rel);
    }
  });

  it("runs on the branches our PRs target, or no paths: pin fires at all", () => {
    const wf = parseYaml(readFileSync(PR_CHECK, "utf8")) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { branches?: string[] } };
    const branches = on?.pull_request?.branches ?? [];
    for (const branch of ["hcl-main", "PR-staging"]) {
      expect(branches, `pr-check-e2e-lint-hcl.yaml must run on PRs targeting ${branch}`).toContain(
        branch,
      );
    }
  });

  /** A spec re-declaring a locked name puts the assertion back on an unlocked literal. */
  it("keeps the locked names out of spec-local declarations", () => {
    const offenders: string[] = [];
    for (const dir of ["tests", "helpers"]) {
      const base = resolve(REPO_ROOT, "e2e", dir);
      for (const ent of readdirSync(base, { recursive: true, withFileTypes: true })) {
        if (!ent.isFile() || !ent.name.endsWith(".ts")) continue;
        if (ent.name.startsWith("rust-mirrored-constants")) continue;
        const rel = relative(REPO_ROOT, resolve(ent.parentPath, ent.name));
        const src = readFileSync(resolve(ent.parentPath, ent.name), "utf8");
        for (const name of LOCKED) {
          if (new RegExp(`(?:const|let)\\s+${name}\\s*[:=]`).test(src)) {
            offenders.push(`${rel} declares ${name}`);
          }
          // After `{` or `,` rather than at line start: prettier keeps a short
          // object on one line, which is the shape a copy-paste actually takes.
          if (new RegExp(`[{,]\\s*${name}\\s*:\\s*-?[\\d_.]+`).test(src)) {
            offenders.push(`${rel} declares ${name} as an object-literal field`);
          }
        }
      }
    }
    expect(offenders, `import these from ${HELPER_REL} instead`).toEqual([]);
  });
});
