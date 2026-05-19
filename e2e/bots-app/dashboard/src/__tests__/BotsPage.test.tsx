import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as Toast from "@radix-ui/react-toast";

import { BotsPage } from "../pages/BotsPage";
import type { BotSnapshot } from "../api/types";

function renderWithClient(): ReturnType<typeof render> {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <Toast.Provider>
        <BotsPage />
      </Toast.Provider>
    </QueryClientProvider>,
  );
}

interface FetchState {
  bots: BotSnapshot[];
  clearTerminatedCalled: boolean;
  clearTerminatedResponse?: { status: number; body: unknown };
}

function stubFetch(state: FetchState): void {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/api/bots" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ bots: state.bots }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/bots/terminated" && init?.method === "DELETE") {
        state.clearTerminatedCalled = true;
        const r = state.clearTerminatedResponse ?? {
          status: 200,
          body: {
            removedCount: state.bots.filter((b) => b.status === "done" || b.status === "failed")
              .length,
          },
        };
        return new Response(JSON.stringify(r.body), {
          status: r.status,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/profiles" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ profiles: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/hosts" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ hosts: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/assets/manifest" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ participants: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/assets/audio" || url === "/api/assets/costumes") {
        return new Response(JSON.stringify({ files: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      return new Response("{}", { status: 200 });
    }),
  );
}

function fakeBot(overrides: Partial<BotSnapshot> = {}): BotSnapshot {
  return {
    botId: "00000000-0000-4000-8000-000000000000",
    participant: "alice",
    status: "in-meeting",
    startedAt: Date.now() - 60_000,
    meetingURL: "https://example.com/meeting/X",
    network: null,
    ttl: "5m",
    ttlRemainingMs: 300_000,
    host: { kind: "local" },
    finishedAt: null,
    ...overrides,
  };
}

describe("BotsPage partitioning", () => {
  beforeEach(() => {
    vi.useRealTimers();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders the Terminated Bots section with its title + subtitle", async () => {
    stubFetch({ bots: [], clearTerminatedCalled: false });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("terminated-bots-section")).toBeInTheDocument();
    });
    expect(screen.getByText("Terminated Bots")).toBeInTheDocument();
    expect(screen.getByText(/bots that have ended in the last hour/i)).toBeInTheDocument();
  });

  it("routes running bots to the Running table and terminated bots to the Terminated table", async () => {
    const live = fakeBot({
      botId: "aaaaaaaa-0000-4000-8000-000000000000",
      participant: "alice-live",
      status: "in-meeting",
    });
    const done = fakeBot({
      botId: "bbbbbbbb-0000-4000-8000-000000000000",
      participant: "bob-done",
      status: "done",
      finishedAt: Date.now() - 10_000,
    });
    const failed = fakeBot({
      botId: "cccccccc-0000-4000-8000-000000000000",
      participant: "carol-failed",
      status: "failed",
      finishedAt: Date.now() - 20_000,
      lastError: "boom",
    });
    stubFetch({
      bots: [live, done, failed],
      clearTerminatedCalled: false,
    });
    renderWithClient();

    await waitFor(() => {
      expect(screen.getByTestId("running-bots-table")).toBeInTheDocument();
    });
    await waitFor(() => {
      expect(screen.getByTestId("terminated-bots-table")).toBeInTheDocument();
    });

    // Live bot only in the running table.
    expect(
      screen.queryByTestId("bot-row-aaaaaaaa-0000-4000-8000-000000000000"),
    ).toBeInTheDocument();
    expect(
      screen.queryByTestId("terminated-bot-row-aaaaaaaa-0000-4000-8000-000000000000"),
    ).not.toBeInTheDocument();

    // Done + failed only in the terminated table.
    expect(
      screen.queryByTestId("terminated-bot-row-bbbbbbbb-0000-4000-8000-000000000000"),
    ).toBeInTheDocument();
    expect(
      screen.queryByTestId("bot-row-bbbbbbbb-0000-4000-8000-000000000000"),
    ).not.toBeInTheDocument();

    expect(
      screen.queryByTestId("terminated-bot-row-cccccccc-0000-4000-8000-000000000000"),
    ).toBeInTheDocument();
    expect(
      screen.queryByTestId("bot-row-cccccccc-0000-4000-8000-000000000000"),
    ).not.toBeInTheDocument();
  });

  it("renders the 'N live / M total' badge that includes terminated bots in the total", async () => {
    stubFetch({
      bots: [
        fakeBot({
          botId: "aaaaaaaa-0000-4000-8000-000000000000",
          status: "in-meeting",
        }),
        fakeBot({
          botId: "bbbbbbbb-0000-4000-8000-000000000000",
          status: "done",
          finishedAt: Date.now() - 1_000,
        }),
        fakeBot({
          botId: "cccccccc-0000-4000-8000-000000000000",
          status: "done",
          finishedAt: Date.now() - 2_000,
        }),
      ],
      clearTerminatedCalled: false,
    });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("running-bots-count-badge")).toHaveTextContent("1 live / 3 total");
    });
  });

  it("renders the terminated count badge as N / 100", async () => {
    stubFetch({
      bots: [
        fakeBot({
          botId: "aaaaaaaa-0000-4000-8000-000000000000",
          status: "done",
          finishedAt: Date.now() - 1_000,
        }),
        fakeBot({
          botId: "bbbbbbbb-0000-4000-8000-000000000000",
          status: "failed",
          finishedAt: Date.now() - 2_000,
        }),
      ],
      clearTerminatedCalled: false,
    });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("terminated-bots-count-badge")).toHaveTextContent("2 / 100");
    });
  });

  it("'Clear all' button is disabled when there are no terminated bots", async () => {
    stubFetch({ bots: [], clearTerminatedCalled: false });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("terminated-bots-clear-all")).toBeDisabled();
    });
  });

  it("'Clear all' button opens a confirm dialog and calls DELETE /api/bots/terminated on confirm", async () => {
    const state: FetchState = {
      bots: [
        fakeBot({
          botId: "aaaaaaaa-0000-4000-8000-000000000000",
          status: "done",
          finishedAt: Date.now() - 1_000,
        }),
      ],
      clearTerminatedCalled: false,
    };
    stubFetch(state);
    renderWithClient();

    await waitFor(() => {
      expect(screen.getByTestId("terminated-bots-clear-all")).not.toBeDisabled();
    });
    fireEvent.click(screen.getByTestId("terminated-bots-clear-all"));
    // Confirm dialog renders the "Clear all" button label.
    fireEvent.click(screen.getByRole("button", { name: /^clear all$/i }));

    await waitFor(() => {
      expect(state.clearTerminatedCalled).toBe(true);
    });
  });
});
