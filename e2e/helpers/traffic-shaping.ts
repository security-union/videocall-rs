import { Locator, Page, expect } from "@playwright/test";
import { controlButton } from "./meeting";

/** Matches `REVEAL_TAPS` in dioxus-ui/src/components/traffic_shaping.rs. */
export const REVEAL_TAPS = 7;

/** Matches `REVEAL_WINDOW_MS` in dioxus-ui/src/components/traffic_shaping.rs. */
export const REVEAL_WINDOW_MS = 3_000;

/** Matches `STEP_TEST_DURATION_MS` in videocall-client/src/traffic_shaping/shaper.rs. */
export const STEP_TEST_DURATION_MS = 15_000;

/** Rate presets in kbps, as used in the rate buttons' test ids. */
export const RATE_PRESETS_KBPS = [2000, 1000, 500, 250] as const;

/** Added-delay presets in ms, as used in the delay buttons' test ids. */
export const DELAY_PRESETS_MS = [0, 50, 150] as const;

export const SCOPES = ["audio", "video", "both"] as const;

export type RateKbps = (typeof RATE_PRESETS_KBPS)[number];
export type DelayMs = (typeof DELAY_PRESETS_MS)[number];
export type Scope = (typeof SCOPES)[number];

export function diagnosticsSidebar(page: Page): Locator {
  return page.locator("#diagnostics-sidebar");
}

export function diagnosticsTitle(page: Page): Locator {
  return page.getByTestId("diagnostics-title");
}

export function shapingSection(page: Page): Locator {
  return page.getByTestId("traffic-shaping-section");
}

export function shapingToggle(page: Page): Locator {
  return page.getByTestId("traffic-shaping-toggle");
}

export function shapingBadge(page: Page): Locator {
  return page.getByTestId("traffic-shaping-badge");
}

export function shapingReadout(page: Page): Locator {
  return page.getByTestId("traffic-shaping-readout");
}

export function shapingOff(page: Page): Locator {
  return page.getByTestId("traffic-shaping-off");
}

export function stepTestButton(page: Page): Locator {
  return page.getByTestId("traffic-shaping-step-test");
}

export function stepRemaining(page: Page): Locator {
  return page.getByTestId("traffic-shaping-step-remaining");
}

export function rateButton(page: Page, kbps: RateKbps): Locator {
  return page.getByTestId(`traffic-shaping-rate-${kbps}`);
}

export function delayButton(page: Page, ms: DelayMs): Locator {
  return page.getByTestId(`traffic-shaping-delay-${ms}`);
}

export function scopeButton(page: Page, scope: Scope): Locator {
  return page.getByTestId(`traffic-shaping-scope-${scope}`);
}

export async function openDiagnostics(page: Page): Promise<void> {
  await controlButton(page, "Open Diagnostics").click();
  await expect(diagnosticsSidebar(page)).toBeVisible({ timeout: 10_000 });
  await expect(diagnosticsTitle(page)).toBeVisible();
}

export async function closeDiagnostics(page: Page): Promise<void> {
  await diagnosticsSidebar(page).locator(".close-button").click();
  await expect(diagnosticsSidebar(page)).toHaveCount(0);
}

/**
 * Click the Diagnostics title `count` times. With no `gapMs` the clicks go
 * as fast as Playwright can send them, well inside the 3 s window. A gap is
 * the behaviour under test when given, so a fixed wait is intended there.
 */
export async function tapTitle(page: Page, count: number, gapMs = 0): Promise<void> {
  const title = diagnosticsTitle(page);
  for (let i = 0; i < count; i++) {
    if (i > 0 && gapMs > 0) {
      await page.waitForTimeout(gapMs);
    }
    await title.click();
  }
}

/** Open Diagnostics and reveal the hidden section with seven quick taps. */
export async function revealTrafficShaping(page: Page): Promise<void> {
  await openDiagnostics(page);
  await tapTitle(page, REVEAL_TAPS);
  await expect(shapingSection(page)).toBeVisible();
}

export async function turnShapingOn(page: Page): Promise<void> {
  await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "false");
  await shapingToggle(page).click();
  await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "true");
  await expect(shapingBadge(page)).toBeVisible();
}

export async function turnShapingOff(page: Page): Promise<void> {
  await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "true");
  await shapingToggle(page).click();
  await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "false");
  await expect(shapingBadge(page)).toHaveCount(0);
}

/**
 * Assert that within one preset group (`rate`, `delay` or `scope`) only the
 * `selected` button has `aria-pressed="true"`.
 */
export async function expectOnlyPressed<T extends string | number>(
  page: Page,
  group: "rate" | "delay" | "scope",
  values: readonly T[],
  selected: T,
): Promise<void> {
  for (const value of values) {
    await expect(
      page.getByTestId(`traffic-shaping-${group}-${value}`),
      `${group} ${value} should${value === selected ? "" : " not"} be pressed`,
    ).toHaveAttribute("aria-pressed", value === selected ? "true" : "false");
  }
}

/** Seconds left in the step test, parsed from "Step test ends in N s". */
export async function stepRemainingSeconds(page: Page): Promise<number> {
  const text = (await stepRemaining(page).textContent()) ?? "";
  const match = text.match(/(\d+)\s*s/);
  if (!match) {
    throw new Error(`unexpected step test readout: "${text}"`);
  }
  return Number(match[1]);
}
