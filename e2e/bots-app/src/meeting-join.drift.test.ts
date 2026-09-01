import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { MIC_UNMUTE_SELECTOR } from "./control-buttons";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");

// Must stay scoped to MicButton's body: a whole-file match also passes when
// these strings move to a sibling button.
const micButtonSource = (): string => {
  const rs = readFileSync(
    resolve(REPO_ROOT, "dioxus-ui", "src", "components", "video_control_buttons.rs"),
    "utf8",
  );
  const m = /pub fn MicButton\([\s\S]*?(?=\n#\[component\]|$)/.exec(rs);
  expect(m, "video_control_buttons.rs no longer defines MicButton").not.toBeNull();
  return m![0];
};

const part = (re: RegExp, name: string): string => {
  const m = re.exec(MIC_UNMUTE_SELECTOR)?.[1];
  expect(m, `MIC_UNMUTE_SELECTOR carries no ${name}`).toBeDefined();
  return m!;
};

describe("mic toggle selector drift locks", () => {
  it("MicButton carries the testid the click selector is built from", () => {
    const testid = part(/\[data-testid="([^"]+)"\]/, "data-testid");
    expect(micButtonSource(), `MicButton no longer sets data-testid="${testid}"`).toContain(
      `"data-testid": "${testid}"`,
    );
  });

  it("MicButton's aria-label is the tooltip title the click selector matches", () => {
    const label = part(/\[aria-label\*="([^"]+)"\]/, "aria-label");
    const src = micButtonSource();
    expect(src, "MicButton no longer binds aria-label to tooltip_title").toContain(
      `"aria-label": tooltip_title`,
    );
    expect(
      src,
      `MicButton no longer renders "${label}" — update MIC_UNMUTE_ARIA_LABEL in control-buttons.ts`,
    ).toContain(label);
  });
});
