import { readFileSync } from "node:fs";
import { readdirSync, statSync } from "node:fs";
import { join, relative, resolve } from "node:path";

import { describe, expect, it } from "vitest";

/**
 * Fleet-wide lock on the Playwright signal-handler opt-out (issues #2089 / #2148).
 *
 * Playwright's handler registry is process-GLOBAL and keyed by signal name, so
 * ONE `chromium.launch()` without the opt-out re-installs `sigtermHandler` for
 * the whole process. That handler is `gracefullyCloseAll()` with NO
 * `process.exit`, and merely registering it overrides Node's default
 * terminate-on-SIGTERM — which is how #2089 produced browsers closing out from
 * under the orchestrator's own shutdown, and how #2148's `bots-app login` ended
 * up closing Chrome and then sitting forever at a dead prompt.
 *
 * #2148's stated acceptance criterion is the GREPPABLE invariant, not any single
 * call site: *every* browser launch in `src/` must carry the opt-out (or an
 * explicit comment saying why not). `bot.ts`'s is pinned three times in
 * `bot.test.ts`; `cli.ts`'s and `sso-capture.ts`'s were pinned nowhere, so
 * reverting them left the suite green.
 *
 * This test reads the SOURCE and enforces the invariant for all call sites at
 * once, so a NEW launch site added without the flags fails here — which a
 * per-call-site mock test could never do.
 *
 * ## Scope of the scan, and why it is wider than `chromium.launch(`
 *
 * A scanner that can MISS a site does not deliver the acceptance criterion, so
 * it covers the whole launch family and the call shapes that actually occur:
 *
 *   - `launchPersistentContext` as well as `launch` — it shares
 *     `_launchProcess` and the same `handleSIG* = true` defaults (see
 *     {@link LAUNCH_METHODS}), so it is equally affected;
 *   - ALIASED receivers (`import { chromium as cr }`, or a local indirection)
 *     and LINE-BROKEN calls, by matching `.launch(` on any receiver with
 *     optional whitespace before the paren.
 *
 * Verified against a probe file carrying all three shapes with no opt-out: the
 * previous literal-`chromium.launch(` scanner found 0 of them, this one finds 3
 * and goes red. The exact-list guard alone was NOT enough — a file dropping out
 * of the glob contributes zero matches and leaves the list unchanged — hence the
 * {@link MIN_EXPECTED_SITES} floor.
 *
 * Deliberately NOT a PTY/subprocess test. The runtime behaviour of #2148 (a
 * SIGTERM'd `login` exits instead of hanging; Ctrl-C at the readline prompt
 * aborts) was verified by hand against playwright-core 1.62.0 during
 * implementation, and reproducing it here would mean driving a headed browser
 * plus a pseudo-terminal for a one-line options object. The invariant that can
 * actually regress silently is "a launch site is missing the flags", and that is
 * a static property — so it is checked statically.
 */

/** Recursively collect `.ts` files under `dir`, skipping tests + fixtures. */
function tsSources(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      out.push(...tsSources(full));
      continue;
    }
    if (!entry.endsWith(".ts")) continue;
    if (entry.endsWith(".test.ts")) continue;
    out.push(full);
  }
  return out;
}

const SRC = resolve(import.meta.dirname);

/**
 * Minimum number of launch sites we expect to find. A file dropping out of the
 * glob (renamed, moved, excluded) contributes ZERO matches — which an
 * exact-list assertion cannot detect, because the list simply stays as it was.
 * Requiring a floor makes that loud. Bump it when a site is added.
 */
const MIN_EXPECTED_SITES = 3;

/**
 * Launch-family methods that share Playwright's `_launchProcess` and therefore
 * its `handleSIGINT/TERM/HUP = true` defaults.
 *
 * `launchPersistentContext` is included because it is EXACTLY as affected as
 * `launch()` — verified in the installed playwright-core 1.62.0 (`coreBundle.js`
 * ~39416): it calls `_innerLaunchWithRetries` → `_launchProcess`, the same path
 * that reads those options. A scanner that only knew about `launch(` would let a
 * `launchPersistentContext` site silently re-install the global SIGTERM handler.
 */
const LAUNCH_METHODS = ["launch", "launchPersistentContext"];

/**
 * Every Playwright browser-launch call site in `src/`, as `{ file, snippet }`
 * where `snippet` runs from the call to its closing `});`. Comments inside the
 * options object count as part of it, which is what makes the "documented
 * exception" escape hatch work.
 *
 * Matches `<anything>.launch(` / `.launchPersistentContext(` rather than the
 * literal `chromium.launch(`, so an ALIASED import (`import { chromium as cr }`,
 * or a `const browserType = chromium` indirection) is still caught, as is a
 * LINE-BROKEN call where `(` sits on the next line.
 */
function launchSites(): Array<{ file: string; snippet: string }> {
  const sites: Array<{ file: string; snippet: string }> = [];
  // `\.(launch|launchPersistentContext)\s*\(` — any receiver, optional
  // whitespace/newline before the paren.
  const re = new RegExp(String.raw`\.(?:${LAUNCH_METHODS.join("|")})\s*\(`, "g");
  for (const file of tsSources(SRC)) {
    const text = readFileSync(file, "utf8");
    for (const m of text.matchAll(re)) {
      const idx = m.index;
      // A doc-comment MENTION (e.g. bot.ts's `chromium.launch({ env })` prose)
      // is not a call site.
      const lineStart = text.lastIndexOf("\n", idx) + 1;
      if (text.slice(lineStart, idx).trimStart().startsWith("*")) continue;
      // Take through the end of the call: the first `});` at or after it.
      const close = text.indexOf("});", idx);
      const snippet = text.slice(idx, close === -1 ? text.length : close + 3);
      sites.push({ file: relative(SRC, file), snippet });
    }
  }
  return sites;
}

describe("chromium.launch signal-handler opt-out (#2089 / #2148)", () => {
  it("finds every real launch site (guards the scanner itself)", () => {
    const sites = launchSites();
    const files = sites.map((s) => s.file).sort();
    // The KNOWN sites, pinned by name: a scanner regression that stops matching
    // them is loud rather than silently making every assertion below vacuous.
    expect(files).toEqual(["auth/sso-capture.ts", "bot.ts", "cli.ts"]);
    // AND a floor. The exact-list assertion above cannot catch a file that drops
    // OUT of the glob — it contributes zero matches, so the list is unchanged.
    // (Real gap: three unflagged sites in a new file once passed all five tests.)
    expect(
      sites.length,
      `expected at least ${MIN_EXPECTED_SITES} launch sites; found ${sites.length}. ` +
        `If a file was renamed or moved out of src/, the scanner is now blind to it.`,
    ).toBeGreaterThanOrEqual(MIN_EXPECTED_SITES);
  });

  it("scans for the whole launch FAMILY, not just chromium.launch(", () => {
    // `launchPersistentContext` shares `_launchProcess` with `launch()` and its
    // `handleSIG* = true` defaults (verified in playwright-core 1.62.0), so it is
    // exactly as affected. Pinning the method list keeps a future `launch`-only
    // narrowing from silently reopening that hole.
    expect(LAUNCH_METHODS).toContain("launch");
    expect(LAUNCH_METHODS).toContain("launchPersistentContext");
  });

  it("every launch site passes handleSIGTERM:false and handleSIGHUP:false", () => {
    const offenders = launchSites().filter(
      (s) => !/handleSIGTERM:\s*false/.test(s.snippet) || !/handleSIGHUP:\s*false/.test(s.snippet),
    );
    expect(
      offenders.map((o) => o.file),
      "Playwright's handler registry is process-global: a launch site without " +
        "`handleSIGTERM: false, handleSIGHUP: false` re-installs the global SIGTERM " +
        "handler for the WHOLE process (#2089), and leaves its own command unable to " +
        "exit on SIGTERM (#2148). Add the flags, or add a comment in the options " +
        "object explaining why this site is exempt.",
    ).toEqual([]);
  });

  it("leaves SIGINT at Playwright's default everywhere", () => {
    // Deliberate: Playwright's `sigintHandler` DOES call `process.exit(130)`, so
    // it is the one handler that terminates rather than hanging. Overriding it
    // would be a behaviour change nobody asked for — and in `login`'s case the
    // abort path comes from readline's own SIGINT event (raw mode clears ISIG, so
    // no process SIGINT is ever delivered at the prompt), not from Playwright.
    for (const site of launchSites()) {
      expect(site.snippet, `${site.file} should not override handleSIGINT`).not.toMatch(
        /handleSIGINT:/,
      );
    }
  });
});

describe("bots-app login abort path (#2148)", () => {
  const cli = readFileSync(join(SRC, "cli.ts"), "utf8");

  it("registers a readline SIGINT listener so Ctrl-C at the prompt aborts", () => {
    // Without this the `login` prompt has NO abort path at all: an active
    // `rl.question` puts the TTY in raw mode, which clears ISIG, so Ctrl-C
    // arrives as a literal 0x03 byte and no SIGINT is delivered to the process
    // (verified experimentally — Playwright's own handler never ran). readline
    // translates that byte into its own `SIGINT` event, so listening on `rl` is
    // the only thing that fires.
    expect(cli).toMatch(/rl\.on\(\s*["']SIGINT["']/);
  });

  it("closes the browser and exits 130 on abort rather than leaving Chrome up", () => {
    const handler = /rl\.on\(\s*["']SIGINT["'][\s\S]{0,900}?process\.exit\(130\)/.exec(cli);
    expect(
      handler,
      "the rl SIGINT handler must exit 130 (conventional SIGINT status)",
    ).not.toBeNull();
    expect(handler?.[0], "the abort path must close the browser first").toMatch(
      /browser\.close\(\)/,
    );
  });
});
