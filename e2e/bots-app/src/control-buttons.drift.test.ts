import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

import { MEETING_STATE_SELECTORS } from "./meeting-join";
import {
  HANG_UP_SELECTOR,
  LIVE_ACTION_BAR_BUTTON,
  CAMERA_TESTID_SELECTOR,
  HANG_UP_LEGACY_TOOLTIP,
  MIC_TESTID_SELECTOR,
  PEER_LIST_TESTID_SELECTOR,
  SCREEN_SHARE_TESTID_SELECTOR,
  MIC_UNMUTE_SELECTOR,
  PEER_LIST_LEGACY_TOOLTIP,
  PEER_LIST_TOOLTIP,
  SCREEN_SHARE_LEGACY_TOOLTIP,
  cameraControlSelector,
  peerListControlSelector,
  SCREEN_SHARE_TOOLTIP,
  micControlSelector,
  screenShareControlSelector,
} from "./control-buttons";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const VIDEO_CONTROL_BUTTONS_REL = "dioxus-ui/src/components/video_control_buttons.rs";
const ATTENDANTS_REL = "dioxus-ui/src/components/attendants.rs";

const repoSource = (rel: string): string => readFileSync(resolve(REPO_ROOT, rel), "utf8");

// Each slice stays scoped to one component/fn body: a whole-file match also
// passes when the pinned string moves to a sibling.
const componentBody = (name: string): string => {
  const rs = repoSource(VIDEO_CONTROL_BUTTONS_REL);
  const m = new RegExp(`pub fn ${name}\\([\\s\\S]*?(?=\\n#\\[component\\]|$)`).exec(rs);
  expect(m, `video_control_buttons.rs no longer defines ${name}`).not.toBeNull();
  return m![0];
};

const testidOf = (selector: string, name: string): string => {
  const m = /\[data-testid="([^"]+)"\]/.exec(selector)?.[1];
  expect(m, `${name} carries no data-testid`).toBeDefined();
  return m!;
};

describe("in-meeting control button drift locks", () => {
  it("re-runs on a change to either mirrored dioxus-ui component, or an RSX edit lands with this lock unread", () => {
    const wf = parseYaml(repoSource(PR_CHECK_REL)) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    for (const rel of [VIDEO_CONTROL_BUTTONS_REL, ATTENDANTS_REL]) {
      expect(paths, `pr-check-e2e-lint-hcl.yaml must trigger on ${rel}`).toContain(rel);
    }
  });

  it("ScreenShareButton carries the testid the click selector is built from", () => {
    const testid = testidOf(screenShareControlSelector("off"), "screenShareControlSelector");
    expect(
      componentBody("ScreenShareButton"),
      `ScreenShareButton no longer sets data-testid="${testid}"`,
    ).toContain(`"data-testid": "${testid}"`);
  });

  it("ScreenShareButton's aria-label is the tooltip title the click selector matches", () => {
    const src = componentBody("ScreenShareButton");
    expect(src, "ScreenShareButton no longer binds aria-label to tooltip_title").toContain(
      `"aria-label": tooltip_title`,
    );
    for (const tooltip of Object.values(SCREEN_SHARE_TOOLTIP)) {
      expect(
        src,
        `ScreenShareButton no longer renders "${tooltip}" — update SCREEN_SHARE_TOOLTIP in control-buttons.ts`,
      ).toContain(tooltip);
    }
  });

  it("PeerListButton carries the testid the peer-list selectors are built from", () => {
    const testid = testidOf(peerListControlSelector("off"), "peerListControlSelector");
    expect(
      componentBody("PeerListButton"),
      `PeerListButton no longer sets data-testid="${testid}"`,
    ).toContain(`"data-testid": "${testid}"`);
  });

  it("PeerListButton's aria-label is the tooltip title the peer-list selectors match", () => {
    const src = componentBody("PeerListButton");
    expect(src, "PeerListButton no longer binds aria-label to tooltip_title").toContain(
      `"aria-label": tooltip_title`,
    );
    for (const tooltip of Object.values(PEER_LIST_TOOLTIP)) {
      expect(
        src,
        `PeerListButton no longer renders "${tooltip}" — update PEER_LIST_TOOLTIP in control-buttons.ts`,
      ).toContain(tooltip);
    }
  });

  it("MicButton renders both aria-labels the mic control selector matches", () => {
    const src = componentBody("MicButton");
    for (const state of ["on", "off"] as const) {
      const label = /\[aria-label\*="([^"]+)"\]/.exec(micControlSelector(state))?.[1];
      expect(label, `micControlSelector("${state}") carries no aria-label`).toBeDefined();
      expect(
        src,
        `MicButton no longer renders "${label}" — update MIC_TOOLTIP in control-buttons.ts`,
      ).toContain(label!);
    }
  });

  it("HangUpButton carries the testid the leave-meeting selector is built from", () => {
    const testid = testidOf(HANG_UP_SELECTOR, "HANG_UP_SELECTOR");
    expect(
      componentBody("HangUpButton"),
      `HangUpButton no longer sets data-testid="${testid}"`,
    ).toContain(`"data-testid": "${testid}"`);
  });

  it("every action-bar selector is scoped to the live bar", () => {
    const scoped = {
      MIC_UNMUTE_SELECTOR,
      'micControlSelector("on")': micControlSelector("on"),
      'micControlSelector("off")': micControlSelector("off"),
      'cameraControlSelector("on")': cameraControlSelector("on"),
      'cameraControlSelector("off")': cameraControlSelector("off"),
      'screenShareControlSelector("on")': screenShareControlSelector("on"),
      'screenShareControlSelector("off")': screenShareControlSelector("off"),
      'peerListControlSelector("on")': peerListControlSelector("on"),
      'peerListControlSelector("off")': peerListControlSelector("off"),
    };
    for (const [name, selector] of Object.entries(scoped)) {
      expect(
        selector.startsWith(LIVE_ACTION_BAR_BUTTON),
        `${name} is not scoped to the live bar — it can also match the drag-preview clone`,
      ).toBe(true);
    }
  });

  it("the live-bar scope is the product's own action-bar slot-button selector", () => {
    const rs = repoSource(ATTENDANTS_REL);
    const m = /fn focus_first_action_bar_button\(\)[\s\S]*?\n}/.exec(rs);
    expect(m, "attendants.rs no longer defines focus_first_action_bar_button").not.toBeNull();
    expect(
      m![0].replace(/\\"/g, '"'),
      "attendants.rs no longer selects action-bar buttons this way — update LIVE_ACTION_BAR_BUTTON",
    ).toContain(LIVE_ACTION_BAR_BUTTON);
  });

  it("the customize-mode drag preview renders its clones outside any [data-slot] wrapper", () => {
    const rs = repoSource(ATTENDANTS_REL);
    const m =
      /class: "action-bar-drag-preview",[\s\S]*?ActionBarSlot::MeetingTimer =>[^\n]*\n/.exec(rs);
    expect(
      m,
      "attendants.rs no longer renders the action-bar-drag-preview clone block",
    ).not.toBeNull();
    expect(m![0], "the drag preview no longer clones ScreenShareButton").toContain(
      "ScreenShareButton",
    );
    const slotAttr = /\[data-slot\]/.exec(LIVE_ACTION_BAR_BUTTON)?.[0];
    expect(slotAttr, "LIVE_ACTION_BAR_BUTTON no longer scopes on [data-slot]").toBeDefined();
    expect(
      m![0],
      "the drag preview now carries data-slot — LIVE_ACTION_BAR_BUTTON no longer excludes it",
    ).not.toContain("data-slot");
  });

  // The fallback needles can only be pinned against the CURRENT RSX: a target
  // deployed before the markers is not in this tree. Case-insensitive to mirror
  // Playwright's `:has-text`.
  it("every legacy tooltip needle still appears in its component's tooltip title", () => {
    const cases: [string, string][] = [
      ["HangUpButton", HANG_UP_LEGACY_TOOLTIP],
      ...Object.values(SCREEN_SHARE_LEGACY_TOOLTIP).map((t): [string, string] => [
        "ScreenShareButton",
        t,
      ]),
      ...Object.values(PEER_LIST_LEGACY_TOOLTIP).map((t): [string, string] => [
        "PeerListButton",
        t,
      ]),
    ];
    for (const [component, needle] of cases) {
      expect(
        componentBody(component).toLowerCase(),
        `${component} no longer renders "${needle}" — the pre-marker fallback would match nothing`,
      ).toContain(needle.toLowerCase());
    }
  });

  // Justifies the container-scoped fallback arm: customize mode introduced the
  // slot wrapper and the clone together, so no wrapper implies no clone.
  it("the drag-preview clone and the slot wrapper are one feature", () => {
    const rs = repoSource(ATTENDANTS_REL);
    expect(rs).toContain("action-bar-drag-preview");
    expect(rs).toContain("action-bar-slot-wrapper");
  });

  // The README states this dependency as an invariant; a new prod-UI marker that
  // is not documented there is the staleness this pins.
  it("the README documents every prod-UI marker the bots-app depends on", () => {
    const readme = readFileSync(resolve(import.meta.dirname, "..", "README.md"), "utf8");
    const depended = [
      ...Object.values(MEETING_STATE_SELECTORS),
      MIC_TESTID_SELECTOR,
      CAMERA_TESTID_SELECTOR,
      SCREEN_SHARE_TESTID_SELECTOR,
      PEER_LIST_TESTID_SELECTOR,
      HANG_UP_SELECTOR,
    ].map((sel) => /\[data-testid="([^"]+)"\]/.exec(sel)?.[1] ?? sel);
    for (const marker of depended) {
      expect(readme, `README's prod-UI touchpoint section does not mention "${marker}"`).toContain(
        marker,
      );
    }
  });
});
