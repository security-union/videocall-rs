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
