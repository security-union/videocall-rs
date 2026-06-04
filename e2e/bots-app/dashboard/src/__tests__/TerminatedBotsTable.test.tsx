import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { formatFinishedAt, TerminatedBotsTable } from "../components/TerminatedBotsTable";
import type { BotSnapshot } from "../api/types";

function renderWithClient(ui: React.ReactElement): ReturnType<typeof render> {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

function fakeBot(overrides: Partial<BotSnapshot> = {}): BotSnapshot {
  return {
    botId: "00000000-0000-4000-8000-000000000000",
    participant: "alice",
    status: "done",
    startedAt: Date.now() - 60_000,
    meetingURL: "https://example.com/meeting/X",
    network: null,
    ttl: "5m",
    ttlRemainingMs: 0,
    finishedAt: Date.now() - 30_000,
    host: { kind: "local" },
    ...overrides,
  };
}

interface FetchState {
  hosts: { hosts: unknown[] };
  log?: { lines: string[]; totalLines: number };
  killCalls?: string[];
  killResponse?: { status: number; body: unknown };
}

function stubFetch(state: FetchState): ReturnType<typeof vi.fn> {
  const fn = vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
    if (url === "/api/hosts" && (!init?.method || init.method === "GET")) {
      return new Response(JSON.stringify(state.hosts), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    }
    const logMatch = /^\/api\/bots\/([^/]+)\/log/.exec(url);
    if (logMatch) {
      return new Response(JSON.stringify(state.log ?? { lines: [], totalLines: 0 }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    }
    const killMatch = /^\/api\/bots\/([^/]+)$/.exec(url);
    if (killMatch && init?.method === "DELETE") {
      state.killCalls = state.killCalls ?? [];
      state.killCalls.push(decodeURIComponent(killMatch[1]));
      const r = state.killResponse ?? {
        status: 200,
        body: { botId: killMatch[1], action: "drop", removed: true },
      };
      return new Response(JSON.stringify(r.body), {
        status: r.status,
        headers: { "content-type": "application/json" },
      });
    }
    return new Response("{}", { status: 200 });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

describe("TerminatedBotsTable", () => {
  beforeEach(() => {
    vi.useRealTimers();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders the empty-state copy when there are no terminated bots", () => {
    stubFetch({ hosts: { hosts: [] } });
    renderWithClient(<TerminatedBotsTable bots={[]} onToast={() => undefined} />);
    expect(screen.getByTestId("terminated-bots-empty")).toBeInTheDocument();
    expect(
      screen.getByText(
        /no terminated bots yet\. bots that finish will appear here for the last hour\./i,
      ),
    ).toBeInTheDocument();
  });

  it("renders a row per terminated bot with status, participant, and finish reason", () => {
    stubFetch({ hosts: { hosts: [] } });
    const bots: BotSnapshot[] = [
      fakeBot({
        botId: "aaaaaaaa-0000-4000-8000-000000000000",
        participant: "alice",
        status: "done",
        finishReason: "user-hangup",
        finishedAt: Date.now() - 30_000,
      }),
      fakeBot({
        botId: "bbbbbbbb-0000-4000-8000-000000000000",
        participant: "bob",
        status: "failed",
        finishReason: "launch-error",
        finishedAt: Date.now() - 60_000,
        lastError: "chrome crashed",
      }),
    ];
    renderWithClient(<TerminatedBotsTable bots={bots} onToast={() => undefined} />);

    const aliceRow = screen.getByTestId("terminated-bot-row-aaaaaaaa-0000-4000-8000-000000000000");
    expect(within(aliceRow).getByText("alice")).toBeInTheDocument();
    expect(within(aliceRow).getByText("Done")).toBeInTheDocument();
    expect(within(aliceRow).getByText("user-hangup")).toBeInTheDocument();

    const bobRow = screen.getByTestId("terminated-bot-row-bbbbbbbb-0000-4000-8000-000000000000");
    expect(within(bobRow).getByText("bob")).toBeInTheDocument();
    expect(within(bobRow).getByText("Failed")).toBeInTheDocument();
    expect(within(bobRow).getByText("launch-error")).toBeInTheDocument();
    expect(within(bobRow).getByText("chrome crashed")).toBeInTheDocument();
  });

  it("sorts bots newest-first by finishedAt", () => {
    stubFetch({ hosts: { hosts: [] } });
    const now = Date.now();
    const old = fakeBot({
      botId: "11111111-0000-4000-8000-000000000000",
      participant: "old-bot",
      finishedAt: now - 1_000_000,
    });
    const recent = fakeBot({
      botId: "22222222-0000-4000-8000-000000000000",
      participant: "recent-bot",
      finishedAt: now - 5_000,
    });
    // Pass them in deliberately-wrong order; the component sorts.
    renderWithClient(<TerminatedBotsTable bots={[old, recent]} onToast={() => undefined} />);
    const rows = screen.getAllByTestId(/^terminated-bot-row-/);
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveAttribute(
      "data-testid",
      "terminated-bot-row-22222222-0000-4000-8000-000000000000",
    );
    expect(rows[1]).toHaveAttribute(
      "data-testid",
      "terminated-bot-row-11111111-0000-4000-8000-000000000000",
    );
  });

  it("opens the BotLogDialog when View logs is clicked", async () => {
    stubFetch({
      hosts: { hosts: [] },
      log: { lines: ["[alice] auto-prime: done — ok"], totalLines: 1 },
    });
    const bots: BotSnapshot[] = [
      fakeBot({
        botId: "cccccccc-0000-4000-8000-000000000000",
        participant: "alice",
      }),
    ];
    renderWithClient(<TerminatedBotsTable bots={bots} onToast={() => undefined} />);

    fireEvent.click(
      screen.getByTestId("terminated-bot-view-log-cccccccc-0000-4000-8000-000000000000"),
    );
    expect(screen.getByTestId("bot-log-dialog")).toBeInTheDocument();
    await waitFor(() => {
      expect(screen.getByText(/auto-prime: done/)).toBeInTheDocument();
    });
  });

  it("calls DELETE /api/bots/:id when Remove is confirmed", async () => {
    const state: FetchState = { hosts: { hosts: [] } };
    stubFetch(state);
    const toasts: Array<{ title: string; variant?: string }> = [];
    const bots: BotSnapshot[] = [
      fakeBot({
        botId: "dddddddd-0000-4000-8000-000000000000",
        participant: "alice",
        finishedAt: Date.now() - 5_000,
      }),
    ];
    renderWithClient(<TerminatedBotsTable bots={bots} onToast={(t) => toasts.push(t)} />);

    fireEvent.click(
      screen.getByTestId("terminated-bot-remove-dddddddd-0000-4000-8000-000000000000"),
    );
    // Confirm dialog renders the "Remove" button.
    fireEvent.click(screen.getByRole("button", { name: /^remove$/i }));

    await waitFor(() => {
      expect(state.killCalls).toEqual(["dddddddd-0000-4000-8000-000000000000"]);
    });
    await waitFor(() => {
      expect(toasts.some((t) => /removed from registry/i.test(t.title))).toBe(true);
    });
  });

  it("renders the ssh host chip for SSH-hosted terminated bots", () => {
    stubFetch({
      hosts: {
        hosts: [
          {
            label: "lab-01",
            host: "lab-01.example.com",
            user: "bots",
            sshKey: null,
            reposPath: "/home/bots/repos",
            notes: null,
            shell: null,
            profileFile: null,
            preCommand: null,
            forwardSsoState: true,
            addedAt: Date.now(),
          },
        ],
      },
    });
    const bots: BotSnapshot[] = [
      fakeBot({
        botId: "eeeeeeee-0000-4000-8000-000000000000",
        participant: "alice",
        host: { kind: "ssh", hostLabel: "lab-01" },
      }),
    ];
    renderWithClient(<TerminatedBotsTable bots={bots} onToast={() => undefined} />);
    const chip = screen.getByTestId(
      "terminated-bot-host-chip-eeeeeeee-0000-4000-8000-000000000000",
    );
    expect(chip).toHaveTextContent("ssh:lab-01");
  });
});

describe("formatFinishedAt", () => {
  it("returns '—' for null", () => {
    expect(formatFinishedAt(null, 1_000_000)).toBe("—");
  });

  it("returns '—' for undefined", () => {
    expect(formatFinishedAt(undefined, 1_000_000)).toBe("—");
  });

  it("returns 'just now' for sub-minute deltas", () => {
    expect(formatFinishedAt(1_000_000 - 30_000, 1_000_000)).toBe("just now");
  });

  it("returns 'X mins ago' inside the first hour", () => {
    expect(formatFinishedAt(1_000_000 - 5 * 60_000, 1_000_000)).toBe("5 mins ago");
    expect(formatFinishedAt(1_000_000 - 1 * 60_000, 1_000_000)).toBe("1 min ago");
  });

  it("returns 'Xh Ym ago' once over an hour", () => {
    expect(formatFinishedAt(1_000_000 - (60 + 5) * 60_000, 1_000_000)).toBe("1h 5m ago");
  });

  it("does not return a negative duration for clock skew", () => {
    // `finishedAt` is in the future relative to `now` (e.g. clock skew).
    expect(formatFinishedAt(1_000_000 + 5_000, 1_000_000)).toBe("just now");
  });
});
