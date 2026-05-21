import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { HistoryInput } from "../components/ui/HistoryInput";

function seed(fieldKey: string, values: string[]) {
  const entries = values.map((value, i) => ({ value, lastUsed: 1000 + i }));
  // The hook reads on mount; sort DESC so the first array element is freshest.
  entries.sort((a, b) => b.lastUsed - a.lastUsed);
  window.localStorage.setItem(`bots-app-dashboard:history:${fieldKey}`, JSON.stringify(entries));
}

function Wrapper(props: { fieldKey: string; initial?: string }) {
  const [value, setValue] = useState(props.initial ?? "");
  return (
    <HistoryInput
      fieldKey={props.fieldKey}
      value={value}
      onChange={setValue}
      placeholder="placeholder"
      testId="hist-input"
      ariaLabel="Test field"
      className="border"
    />
  );
}

describe("<HistoryInput />", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });
  afterEach(() => {
    window.localStorage.clear();
  });

  it("does not open the popover when history is empty", () => {
    render(<Wrapper fieldKey="emptyKey" />);
    const input = screen.getByTestId("hist-input");
    fireEvent.focus(input);
    expect(screen.queryByTestId("hist-input-history")).not.toBeInTheDocument();
  });

  it("opens the popover on focus when history is non-empty", async () => {
    seed("populatedKey", ["alice", "bob"]);
    render(<Wrapper fieldKey="populatedKey" />);
    const input = screen.getByTestId("hist-input");
    fireEvent.focus(input);
    await waitFor(() => expect(screen.getByTestId("hist-input-history")).toBeInTheDocument());
    expect(screen.getByText("alice")).toBeInTheDocument();
    expect(screen.getByText("bob")).toBeInTheDocument();
  });

  it("arrow-down then Enter selects the highlighted entry", async () => {
    seed("k", ["alice", "bob"]);
    render(<Wrapper fieldKey="k" />);
    const input = screen.getByTestId("hist-input") as HTMLInputElement;
    fireEvent.focus(input);
    await waitFor(() => expect(screen.getByTestId("hist-input-history")).toBeInTheDocument());
    // Latest-first ordering: alice = index 1 (added later in seed), bob = index 0.
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    // Whichever entry was at index 0 (most-recent) is now in the input.
    expect(input.value).toBe("bob");
  });

  it("Esc closes the popover", async () => {
    seed("k", ["alice"]);
    render(<Wrapper fieldKey="k" />);
    const input = screen.getByTestId("hist-input");
    fireEvent.focus(input);
    await waitFor(() => expect(screen.getByTestId("hist-input-history")).toBeInTheDocument());
    fireEvent.keyDown(input, { key: "Escape" });
    await waitFor(() =>
      expect(screen.queryByTestId("hist-input-history")).not.toBeInTheDocument(),
    );
  });

  it("clicking an entry selects it and closes the popover", async () => {
    seed("k", ["alice", "bob"]);
    render(<Wrapper fieldKey="k" />);
    const input = screen.getByTestId("hist-input") as HTMLInputElement;
    fireEvent.focus(input);
    await waitFor(() => expect(screen.getByTestId("hist-input-history")).toBeInTheDocument());
    // mousedown is what HistoryInput listens for on the row.
    fireEvent.mouseDown(screen.getByText("alice"));
    expect(input.value).toBe("alice");
  });

  it("the row × button removes that entry from history", async () => {
    seed("k", ["alice", "bob"]);
    render(<Wrapper fieldKey="k" />);
    const input = screen.getByTestId("hist-input");
    fireEvent.focus(input);
    await waitFor(() => expect(screen.getByTestId("hist-input-history")).toBeInTheDocument());
    const removeBtn = screen.getByLabelText('Remove “alice” from history');
    fireEvent.mouseDown(removeBtn);
    await waitFor(() => expect(screen.queryByText("alice")).not.toBeInTheDocument());
    const stored = JSON.parse(window.localStorage.getItem("bots-app-dashboard:history:k")!);
    expect(stored.map((e: { value: string }) => e.value)).toEqual(["bob"]);
  });

  it("ArrowDown opens the popover when closed but history exists", async () => {
    seed("k", ["alice"]);
    render(<Wrapper fieldKey="k" />);
    const input = screen.getByTestId("hist-input");
    // Don't focus to avoid the focus-opens-popover path; instead
    // dispatch the keydown directly. We simulate a window where the
    // popover got closed but the input still has focus.
    fireEvent.focus(input);
    fireEvent.keyDown(input, { key: "Escape" });
    await waitFor(() =>
      expect(screen.queryByTestId("hist-input-history")).not.toBeInTheDocument(),
    );
    fireEvent.keyDown(input, { key: "ArrowDown" });
    await waitFor(() => expect(screen.getByTestId("hist-input-history")).toBeInTheDocument());
  });
});
