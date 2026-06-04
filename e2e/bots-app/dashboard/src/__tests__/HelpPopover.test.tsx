import { describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { HelpPopover } from "../components/ui/HelpPopover";

describe("<HelpPopover />", () => {
  it("renders an accessible trigger button", () => {
    render(
      <HelpPopover fieldLabel="Meeting URL" testId="help-trigger">
        <p>Body content</p>
      </HelpPopover>,
    );
    const trigger = screen.getByTestId("help-trigger");
    expect(trigger).toBeInTheDocument();
    expect(trigger).toHaveAttribute("aria-label", "Help for Meeting URL");
  });

  it("toggles the popover content on click", async () => {
    render(
      <HelpPopover fieldLabel="Network" testId="help-trigger">
        <p>Pick a profile</p>
      </HelpPopover>,
    );
    const trigger = screen.getByTestId("help-trigger");
    fireEvent.click(trigger);
    await waitFor(() => {
      expect(screen.getByText("Pick a profile")).toBeInTheDocument();
    });
  });

  it("opens the popover on focus (keyboard accessibility)", async () => {
    render(
      <HelpPopover fieldLabel="TTL" testId="help-trigger">
        <p>How long the bot stays</p>
      </HelpPopover>,
    );
    const trigger = screen.getByTestId("help-trigger");
    fireEvent.focus(trigger);
    await waitFor(() => {
      expect(screen.getByText("How long the bot stays")).toBeInTheDocument();
    });
  });
});
