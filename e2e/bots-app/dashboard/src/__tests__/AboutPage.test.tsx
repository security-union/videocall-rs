import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { AboutPage } from "../pages/AboutPage";

describe("<AboutPage />", () => {
  it("shows the dashboard version near the top of the page", () => {
    render(<AboutPage />);
    const versionRow = screen.getByTestId("about-version");
    expect(versionRow).toBeInTheDocument();
    expect(versionRow).toHaveTextContent(/Version:\s*v\d+\.\d+\.\d+(?:[-+][\w.-]+)?/);
  });

  it("still surfaces the discussion #793 link from phase 5", () => {
    render(<AboutPage />);
    const link = screen.getByRole("link", { name: /videocall discussion #793/i });
    expect(link).toHaveAttribute(
      "href",
      "https://github01.hclpnp.com/labs-projects/videocall/discussions/793",
    );
  });
});
