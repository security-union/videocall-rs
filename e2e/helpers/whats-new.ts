import { expect, Locator, Page } from "@playwright/test";

/**
 * The "What's new" change log (issue 2882). The fixture holds fifteen builds
 * that list changes, version 1.1.(29 + N) built on 2026-09-N, in a scrambled
 * file order so only a newest-first sort by `built` yields
 * `CHANGELOG_NEWEST_FIRST`. Two newer stamped builds list no changes (issue
 * 2910) and must neither render nor count.
 */

const FILE_ORDER_DAYS = [7, 12, 3, 15, 1, 9, 14, 5, 11, 2, 13, 8, 6, 10, 4];
const NEWEST_DAY = 15;
const EMPTY_DAY = 16;
const BLANK_ONLY_DAY = 17;

const versionOf = (day: number) => `1.1.${29 + day}`;
const changesOf = (day: number) => [`Build ${day} change one`, `Build ${day} change two`];

const datedBuild = (day: number, changes: string[]) => ({
  built: `2026-09-${String(day).padStart(2, "0")}T05:00:00Z`,
  version: versionOf(day),
  commit: `fixture-${day}`,
  changes,
});

const listedBuilds = FILE_ORDER_DAYS.map((day) =>
  // A blank line in the newest build must not render as an empty bullet.
  datedBuild(
    day,
    day === NEWEST_DAY ? [changesOf(day)[0], "   ", changesOf(day)[1]] : changesOf(day),
  ),
);

export const CHANGELOG_FIXTURE = {
  builds: [
    ...listedBuilds.slice(0, 4),
    datedBuild(EMPTY_DAY, []),
    ...listedBuilds.slice(4, 9),
    datedBuild(BLANK_ONLY_DAY, ["  "]),
    ...listedBuilds.slice(9),
  ],
};

/** Versions of the stamped builds that list no changes. */
export const CHANGELOG_EMPTY_VERSIONS = [
  `v${versionOf(EMPTY_DAY)}`,
  `v${versionOf(BLANK_ONLY_DAY)}`,
];

/** Rendered versions, newest first: v1.1.44 (day 15) ... v1.1.30 (day 1). */
export const CHANGELOG_NEWEST_FIRST = Array.from(
  { length: FILE_ORDER_DAYS.length },
  (_, i) => `v${versionOf(NEWEST_DAY - i)}`,
);

export const CHANGELOG_NEWEST_CHANGES = changesOf(NEWEST_DAY);

export interface ChangelogReply {
  contentType: string;
  body: string;
}

export const CHANGELOG_JSON_REPLY: ChangelogReply = {
  contentType: "application/json",
  body: JSON.stringify(CHANGELOG_FIXTURE),
};

const FIRST_PENDING = ["Unreleased change one", "Unreleased change two"];
// Repeats a line of the first section, which the merged list shows once.
const SECOND_PENDING = ["Unreleased change two", "Unreleased change three"];

/** Both pending sections' lines, in file order, each once. */
export const CHANGELOG_PENDING_CHANGES = [
  "Unreleased change one",
  "Unreleased change two",
  "Unreleased change three",
];

/** The fixture with one pending section mid-file and another LAST. */
export const CHANGELOG_WITH_PENDING_REPLY: ChangelogReply = {
  contentType: "application/json",
  body: JSON.stringify({
    builds: [
      ...CHANGELOG_FIXTURE.builds.slice(0, 7),
      { pending: true, through: "0123abcd", prs: [2890, 2891], changes: FIRST_PENDING },
      ...CHANGELOG_FIXTURE.builds.slice(7),
      { pending: true, through: "4567cdef", prs: [2892], changes: SECOND_PENDING },
    ],
  }),
};

/** A line led by one of these keys shows under that kind's pill (issue 3253). */
export const CHANGELOG_KIND_PREFIX = /^(Feature|UX|Perf|Reliability|bug-fix|Security): /;

export interface ChangelogGroupShown {
  kind: string;
  pill: string;
  changes: string[];
}

// One pending section per PR, as the curator writes them, kinds out of display order.
const GROUPED_PENDING_SECTIONS = [
  ["bug-fix: Muted mic icon stays in sync (#3101)"],
  ["UX: Bigger buttons on phones (#3102)"],
  ["Feature: Raise your hand from the footer (#3103)", "UX: (#3104)"],
  // Shows the same text as #3102's line, so it is listed once.
  ["UX: Bigger buttons on phones (#3105)", "UX: Clearer join screen (#3106)"],
  ["Perf: Lighter grid in big meetings"],
];

const GROUPED_BUILD_CHANGES = [
  "Meeting-api: Older raw title kept as is (#3201)",
  "Security: Meeting links are harder to guess (#3202)",
  "Reliability: Calls come back after a network drop",
  "UX: Settings open faster on phones (#3203)",
];

const FLAT_BUILD_CHANGES = [
  "Pinning moves a tile to the front (#3001)",
  "Plain release note without a number",
];

export const CHANGELOG_GROUPED_UNRELEASED: ChangelogGroupShown[] = [
  { kind: "features", pill: "Features", changes: ["Raise your hand from the footer"] },
  {
    kind: "ux",
    pill: "UX",
    changes: ["Bigger buttons on phones", "Clearer join screen"],
  },
  { kind: "performance", pill: "Performance", changes: ["Lighter grid in big meetings"] },
  { kind: "fixes", pill: "Fixes", changes: ["Muted mic icon stays in sync"] },
];

export const CHANGELOG_GROUPED_BUILD: { version: string; groups: ChangelogGroupShown[] } = {
  version: "v1.2.1",
  groups: [
    { kind: "ux", pill: "UX", changes: ["Settings open faster on phones"] },
    { kind: "reliability", pill: "Reliability", changes: ["Calls come back after a network drop"] },
    { kind: "security", pill: "Security", changes: ["Meeting links are harder to guess"] },
    {
      kind: "miscellaneous",
      pill: "Miscellaneous",
      changes: ["Meeting-api: Older raw title kept as is"],
    },
  ],
};

export const CHANGELOG_FLAT_BUILD = {
  version: "v1.2.0",
  changes: ["Pinning moves a tile to the front", "Plain release note without a number"],
};

/** Pending sections, a build with kind-named lines, and an older build with none. */
export const CHANGELOG_GROUPED_REPLY: ChangelogReply = {
  contentType: "application/json",
  body: JSON.stringify({
    builds: [
      {
        built: "2026-10-01T05:00:00Z",
        version: "1.2.0",
        commit: "fixture-flat",
        changes: FLAT_BUILD_CHANGES,
      },
      ...GROUPED_PENDING_SECTIONS.map((changes, i) => ({
        pending: true,
        through: `fixture-pending-${i}`,
        prs: [3101 + i],
        changes,
      })),
      {
        built: "2026-10-02T05:00:00Z",
        version: "1.2.1",
        commit: "fixture-grouped",
        changes: GROUPED_BUILD_CHANGES,
      },
    ],
  }),
};

/** What an SPA server answers for a file it does not have: 200 + the app shell. */
export const CHANGELOG_HTML_SHELL_REPLY: ChangelogReply = {
  contentType: "text/html",
  body: '<!DOCTYPE html><html><head><title>videocall</title></head><body><div id="main"></div></body></html>',
};

/**
 * Answers `/assets/changelog.json` with `replies` in order (the last one
 * repeats) and returns a live count of the requests it answered.
 */
export async function routeChangelog(page: Page, replies: ChangelogReply[]): Promise<() => number> {
  let answered = 0;
  await page.route("**/assets/changelog.json", async (route) => {
    const reply = replies[Math.min(answered, replies.length - 1)];
    answered += 1;
    await route.fulfill({ status: 200, contentType: reply.contentType, body: reply.body });
  });
  return () => answered;
}

export async function openAboutModal(page: Page): Promise<Locator> {
  await page.goto("/");
  await page.locator('[data-testid="about-footer-link"]').click();
  const modal = page.locator('[data-testid="about-modal"]');
  await expect(modal).toBeVisible({ timeout: 5_000 });
  return modal;
}

export function aboutWhatsNewToggle(modal: Locator): Locator {
  return modal.locator(
    'section[aria-labelledby="about-client-heading"] > .changelog > [data-testid="changelog-toggle"]',
  );
}
