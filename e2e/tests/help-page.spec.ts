import { test, expect, Locator, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { createAuthenticatedContext } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { clickMeetingInfoTriggerAt } from "../helpers/meeting-footer";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * The public /help page (issue 3351), reached from the home footer and About
 * dialog in the same tab and from the in-call footer in a new tab, so the call
 * page stays open in its own tab.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const DESKTOP = { width: 1280, height: 720 };
const PHONE = { width: 390, height: 844 };
const MEETING_INFO_DIALOG = "#meeting-info-dialog";
const HELP_TITLE = "Help & FAQ – videocall.rs";

function helpHeading(page: Page): Locator {
  return page.getByRole("heading", { level: 1, name: "Help & FAQ" });
}

async function expectHelpPage(page: Page): Promise<Locator> {
  await expect(helpHeading(page)).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("help-page")).toBeVisible();
  const faq = page.getByRole("region", { name: "Frequently asked questions" });
  await expect(faq).toBeVisible();
  return faq;
}

async function expectFaqOpensFromItsQuestion(faq: Locator): Promise<void> {
  const item = faq.locator("details.help-faq-item").first();
  const question = item.locator(":scope > summary");
  const answer = item.locator(":scope > .help-faq-answer");
  await expect(question).toBeVisible();
  await expect(item).not.toHaveAttribute("open");
  await expect(answer).toBeHidden();

  await question.click();
  await expect(item).toHaveAttribute("open", "");
  await expect(answer).toBeVisible();
}

test.describe("Help & FAQ page (issue 3351)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("About's Help & FAQ link opens /help in the same tab, and a FAQ question opens its answer @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await injectSessionCookie(context, { baseURL });
    await page.goto("/");
    await page.getByTestId("about-footer-link").click();
    const modal = page.getByTestId("about-modal");
    await expect(modal).toBeVisible({ timeout: 10_000 });

    const helpLink = modal.getByRole("link", { name: "Help & FAQ" });
    await expect(helpLink).toHaveAttribute("data-testid", "about-help-link");
    await expect(helpLink).toHaveAttribute("href", "/help");
    await expect(helpLink).not.toHaveAttribute("target");

    await helpLink.click();
    const faq = await expectHelpPage(page);
    await expect(page).toHaveURL("/help");
    await expect(modal).toHaveCount(0);
    expect(context.pages(), "the About link stays in this tab").toHaveLength(1);
    await expect(helpHeading(page), "with no #target the h1 takes focus").toBeFocused();

    await expectFaqOpensFromItsQuestion(faq);

    await page.getByTestId("help-back-home").click();
    await expect(page.getByTestId("about-footer-link")).toBeVisible({ timeout: 10_000 });
    await expect(page).toHaveURL("/");
  });

  test("the call footer's help link opens /help in a new tab and leaves the call and Meeting info alone @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const context = await createAuthenticatedContext(
      browser,
      "help-footer@videocall.rs",
      "HelpFooter",
      baseURL || DEFAULT_UI_URL,
    );

    try {
      const page = await context.newPage();
      await page.setViewportSize(DESKTOP);
      const meetingId = `e2e_help_footer_${Date.now()}`;
      await fillAndSubmitJoinForm(page, meetingId, "HelpFooter");
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`));
      const meetingUrl = page.url();

      const footer = page.getByTestId("meeting-footer");
      await expect(footer).toBeVisible({ timeout: 15_000 });
      const help = footer.getByRole("link", { name: "Help and FAQ (opens in a new tab)" });
      await expect(help).toBeVisible();
      await expect(help).toHaveAttribute("data-testid", "meeting-footer-help");
      await expect(help).toHaveAttribute("href", "/help?from=call");
      await expect(help).toHaveAttribute("target", "_blank");
      await expect(help).toHaveAttribute("rel", "noopener noreferrer");
      await expect(help).toHaveAttribute("title", "Help and FAQ (opens in a new tab)");
      await expect(page.locator("#meeting-footer-trigger")).toHaveAttribute(
        "title",
        "Meeting info",
      );
      const meetingInfo = page.locator(MEETING_INFO_DIALOG);
      await expect(meetingInfo).toHaveCount(0);

      const [helpTab] = await Promise.all([
        context.waitForEvent("page", { timeout: 15_000 }),
        help.click({ timeout: 10_000 }),
      ]);
      await expectHelpPage(helpTab);
      await expect(helpTab).toHaveURL("/help?from=call");
      const note = helpTab.getByTestId("help-call-note");
      await expect(note).toBeVisible();
      await expect(note).toHaveRole("note");
      await expect(
        helpTab.getByTestId("help-back-home"),
        "call mode does not offer Back to home",
      ).toHaveCount(0);
      expect(
        await helpTab.evaluate(() => window.opener === null),
        "noopener: the help tab gets no handle on the call",
      ).toBe(true);

      await expect(page).toHaveURL(meetingUrl);
      await expect(page.locator("#grid-container")).toBeVisible();
      await expect(footer).toBeVisible();
      await expect(help).toBeVisible();
      await expect(meetingInfo, "the help link must not also open Meeting info").toHaveCount(0);

      await clickMeetingInfoTriggerAt(page);
      await expect(
        meetingInfo,
        "premise: the rest of the bar still opens Meeting info",
      ).toBeVisible({ timeout: 5_000 });
    } finally {
      await context.close();
    }
  });

  test("/help loads directly without a session, and its contents links scroll within the page @bvt1", async ({
    page,
    context,
  }) => {
    expect(await context.cookies(), "no session cookie in this context").toHaveLength(0);
    await page.goto("/help");
    const faq = await expectHelpPage(page);
    await expect(page).toHaveURL("/help");

    const faqHeading = faq.getByRole("heading", { level: 2, name: "Frequently asked questions" });
    await expect(faqHeading, "premise: the FAQ starts below the fold").not.toBeInViewport();
    const contents = page.getByRole("navigation", { name: "On this page" });
    await contents.getByRole("link", { name: "Frequently asked questions" }).click();
    await expect(page).toHaveURL("/help#faq");
    await expect(faqHeading).toBeInViewport();
    await expect(helpHeading(page), "a #id link must not leave the help page").toBeAttached();

    await page.reload();
    await expectHelpPage(page);
    await expect(page, "a reload keeps the #target").toHaveURL("/help#faq");
    await expect(faqHeading).toBeInViewport();
    await expectFaqOpensFromItsQuestion(faq);
    await expect(page).toHaveURL("/help#faq");
  });

  test("a direct load of a FAQ answer's link keeps the URL and opens that answer in view @bvt1", async ({
    page,
  }) => {
    await page.goto("/help#faq-browsers");
    const faq = await expectHelpPage(page);
    await expect(page).toHaveURL("/help#faq-browsers");

    const item = faq.locator("details#faq-browsers");
    await expect(item).toHaveAttribute("open", "");
    await expect(item.locator(":scope > .help-faq-answer")).toBeVisible();
    await expect(item.locator(":scope > summary")).toBeInViewport();
    await expect(item.locator(":scope > summary")).toBeFocused();
    await expect(
      faq.locator("details.help-faq-item[open]"),
      "only the linked answer opens",
    ).toHaveCount(1);
    await expect(page).toHaveURL("/help#faq-browsers");
  });

  test("the home footer's Help & FAQ link opens /help in the same tab with its own tab title, and Back to home restores it @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await injectSessionCookie(context, { baseURL });
    await page.goto("/");
    const homeLink = page.getByTestId("home-help-link");
    await expect(homeLink).toBeVisible({ timeout: 10_000 });
    await expect(homeLink).toHaveAccessibleName("Help & FAQ");
    await expect(homeLink).toHaveAttribute("href", "/help");
    await expect(homeLink).not.toHaveAttribute("target");
    const homeTitle = await page.title();
    expect(homeTitle, "premise: home has a different tab title").not.toBe(HELP_TITLE);

    await homeLink.click();
    await expectHelpPage(page);
    await expect(page).toHaveURL("/help");
    await expect(page).toHaveTitle(HELP_TITLE);
    await expect(helpHeading(page)).toBeFocused();
    expect(context.pages(), "the home link stays in this tab").toHaveLength(1);

    await page.getByTestId("help-back-home").click();
    await expect(page.getByTestId("home-help-link")).toBeVisible({ timeout: 10_000 });
    await expect(page).toHaveURL("/");
    await expect(page).toHaveTitle(homeTitle);
  });

  test("on a phone the home footer stacks Help & FAQ under About, centred, without the separator @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await injectSessionCookie(context, { baseURL });
    await page.setViewportSize(PHONE);
    await page.goto("/");
    const about = page.getByTestId("about-footer-link");
    const help = page.getByTestId("home-help-link");
    await expect(about).toBeVisible({ timeout: 10_000 });
    await expect(help).toBeVisible();
    const separator = page
      .locator(".about-footer", { has: help })
      .locator(":scope > .about-footer-sep");
    await expect(separator).toHaveCount(1);
    await expect(separator).toBeHidden();

    const a = await about.boundingBox();
    const h = await help.boundingBox();
    expect(a, "About must be laid out").not.toBeNull();
    expect(h, "Help & FAQ must be laid out").not.toBeNull();
    expect(h!.y, "Help & FAQ sits below About").toBeGreaterThanOrEqual(a!.y + a!.height - 0.5);
    expect(
      Math.abs(h!.x + h!.width / 2 - (a!.x + a!.width / 2)),
      "both links share one centre line",
    ).toBeLessThanOrEqual(1);
  });
});
