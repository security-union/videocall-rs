import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as Toast from "@radix-ui/react-toast";

import { BotsPage } from "../pages/BotsPage";
import type { BotSnapshot } from "../api/types";

/**
 * Tests for the bulk-actions toolbar above the Running Bots table.
 * The toolbar is wired into `BotsPage` rather than tested as an
 * isolated component because it shares the per-bot optimistic
 * mic/camera state with `RunningBotsTable` — the integration is the
 * thing worth verifying.
 *
 * All assertions use the `data-testid` hooks on the toolbar buttons
 * (`bulk-mute-toggle`, `bulk-camera-toggle`, `bulk-leave-all`,
 * `bulk-terminate-all`) and exercise the per-bot fan-out by counting
 * calls into the stubbed `fetch` global.
 */

function renderWithClient(): ReturnType<typeof render> {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  // Toast.Viewport is required for `Toast.Root` children to render
  // their `<Toast.Title>` into the DOM; without it, Radix keeps the
  // toast nodes in a portal-less limbo and `screen.getByText` can't
  // see the summary toast we want to assert on.
  return render(
    <QueryClientProvider client={qc}>
      <Toast.Provider>
        <BotsPage />
        <Toast.Viewport />
      </Toast.Provider>
    </QueryClientProvider>,
  );
}

interface FetchLog {
  url: string;
  method: string;
  body?: unknown;
}

interface FetchState {
  bots: BotSnapshot[];
  log: FetchLog[];
  /**
   * Per-bot override that lets a test inject failures on specific
   * endpoint+botId pairs. Key shape: `${method} ${url}`. If absent,
   * the endpoint returns 200 with an empty body.
   */
  fail?: Record<string, { status: number; body: unknown }>;
}

function stubFetch(state: FetchState): void {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      const bodyText = typeof init?.body === "string" ? init.body : undefined;
      let body: unknown;
      if (bodyText) {
        try {
          body = JSON.parse(bodyText);
        } catch {
          body = bodyText;
        }
      }
      state.log.push({ url, method, body });

      // Static fixtures (only the endpoints BotsPage's children
      // actually hit on initial render — keep the stub small).
      if (url === "/api/bots" && method === "GET") {
        return new Response(JSON.stringify({ bots: state.bots }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/profiles" && method === "GET") {
        return new Response(JSON.stringify({ profiles: [] }), { status: 200 });
      }
      if (url === "/api/hosts" && method === "GET") {
        return new Response(JSON.stringify({ hosts: [] }), { status: 200 });
      }
      if (url === "/api/assets/manifest" && method === "GET") {
        return new Response(JSON.stringify({ participants: [] }), { status: 200 });
      }
      if (url === "/api/assets/audio" || url === "/api/assets/costumes") {
        return new Response(JSON.stringify({ files: [] }), { status: 200 });
      }

      // Per-bot mutation endpoints (the ones the bulk toolbar fans
      // out into).
      const failKey = `${method} ${url}`;
      if (state.fail && state.fail[failKey]) {
        const f = state.fail[failKey];
        return new Response(JSON.stringify(f.body), {
          status: f.status,
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

function liveBots(n: number): BotSnapshot[] {
  return Array.from({ length: n }, (_, i) =>
    fakeBot({
      botId: `${String(i).padStart(8, "a")}-0000-4000-8000-000000000000`,
      participant: `bot-${i}`,
      status: "in-meeting",
    }),
  );
}

function countCalls(log: FetchLog[], method: string, urlPredicate: (u: string) => boolean): number {
  return log.filter((e) => e.method === method && urlPredicate(e.url)).length;
}

describe("BulkActionsToolbar", () => {
  beforeEach(() => {
    vi.useRealTimers();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("does not render when there are zero running bots", async () => {
    stubFetch({ bots: [], log: [] });
    renderWithClient();
    // BotsPage renders synchronously after the initial fetch settles;
    // the toolbar should never appear when liveBots is empty.
    await waitFor(() => {
      expect(screen.getByTestId("running-bots-count-badge")).toHaveTextContent("0 live / 0 total");
    });
    expect(screen.queryByTestId("bulk-actions-toolbar")).not.toBeInTheDocument();
  });

  it("does not render when only terminated bots are present", async () => {
    stubFetch({
      bots: [
        fakeBot({
          botId: "aaaaaaaa-0000-4000-8000-000000000000",
          status: "done",
          finishedAt: Date.now() - 1_000,
        }),
      ],
      log: [],
    });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("running-bots-count-badge")).toHaveTextContent("0 live / 1 total");
    });
    expect(screen.queryByTestId("bulk-actions-toolbar")).not.toBeInTheDocument();
  });

  it("does not render when only remote (SSH) bots are running — nothing controllable", async () => {
    // Remote bots are skipped by the bulk fan-out, so if every
    // running bot is remote there's nothing to do; the toolbar
    // hides rather than rendering disabled-only buttons.
    stubFetch({
      bots: [
        fakeBot({
          botId: "aaaaaaaa-0000-4000-8000-000000000000",
          status: "in-meeting",
          host: { kind: "ssh", hostLabel: "remote-1" },
        }),
      ],
      log: [],
    });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("running-bots-count-badge")).toHaveTextContent("1 live / 1 total");
    });
    expect(screen.queryByTestId("bulk-actions-toolbar")).not.toBeInTheDocument();
  });

  it("renders the toolbar with Mute all / Camera on all / Leave all / Terminate all when bots are running", async () => {
    stubFetch({ bots: liveBots(3), log: [] });
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-actions-toolbar")).toBeInTheDocument();
    });
    // Default per-row mic state is "muted" (no optimistic flag), so
    // the bulk button defaults to "Unmute all"; camera is "off" by
    // default so the bulk button defaults to "Camera on all".
    expect(screen.getByTestId("bulk-mute-toggle")).toHaveTextContent(/unmute all/i);
    expect(screen.getByTestId("bulk-camera-toggle")).toHaveTextContent(/camera on all/i);
    expect(screen.getByTestId("bulk-leave-all")).toBeInTheDocument();
    expect(screen.getByTestId("bulk-terminate-all")).toBeInTheDocument();
  });

  it("'Terminate all' opens a confirm dialog and fires DELETE /api/bots/:id once per live bot on confirm", async () => {
    const state: FetchState = { bots: liveBots(3), log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-terminate-all")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByTestId("bulk-terminate-all"));
    // Confirm dialog uses the "Terminate all" button label.
    const confirmBtn = await screen.findByRole("button", { name: /^terminate all$/i });
    fireEvent.click(confirmBtn);

    await waitFor(() => {
      expect(
        countCalls(
          state.log,
          "DELETE",
          (u) => u.startsWith("/api/bots/") && !u.endsWith("/terminated"),
        ),
      ).toBe(3);
    });
  });

  it("'Leave all' opens a confirm dialog and fires POST /api/bots/:id/leave once per live bot on confirm", async () => {
    const state: FetchState = { bots: liveBots(4), log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-leave-all")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByTestId("bulk-leave-all"));
    const confirmBtn = await screen.findByRole("button", { name: /^leave all$/i });
    fireEvent.click(confirmBtn);

    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/leave"))).toBe(4);
    });
  });

  it("'Unmute all' (initial label) fires POST /api/bots/:id/mute with mic=true for each muted bot, with NO confirm dialog", async () => {
    const state: FetchState = { bots: liveBots(3), log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-mute-toggle")).toHaveTextContent(/unmute all/i);
    });

    fireEvent.click(screen.getByTestId("bulk-mute-toggle"));

    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/mute"))).toBe(3);
    });
    const muteCalls = state.log.filter((e) => e.method === "POST" && e.url.endsWith("/mute"));
    for (const c of muteCalls) {
      expect(c.body).toEqual({ mic: true });
    }
  });

  it("after 'Unmute all', the button label flips to 'Mute all' and clicking it sends mic=false only for un-muted bots", async () => {
    const state: FetchState = { bots: liveBots(2), log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-mute-toggle")).toHaveTextContent(/unmute all/i);
    });

    // First click: unmute all → 2 requests with mic=true.
    fireEvent.click(screen.getByTestId("bulk-mute-toggle"));
    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/mute"))).toBe(2);
    });

    // Label should now read "Mute all" (both bots are optimistically
    // unmuted). Second click: send mic=false for the un-muted ones.
    await waitFor(() => {
      expect(screen.getByTestId("bulk-mute-toggle")).toHaveTextContent(/^mute all$/i);
    });
    fireEvent.click(screen.getByTestId("bulk-mute-toggle"));
    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/mute"))).toBe(4);
    });
    // The 3rd and 4th calls should send mic=false.
    const muteCalls = state.log.filter((e) => e.method === "POST" && e.url.endsWith("/mute"));
    expect(muteCalls[2].body).toEqual({ mic: false });
    expect(muteCalls[3].body).toEqual({ mic: false });
  });

  it("'Camera on all' fires POST /api/bots/:id/video with camera=true for each bot, with NO confirm dialog", async () => {
    const state: FetchState = { bots: liveBots(3), log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-camera-toggle")).toHaveTextContent(/camera on all/i);
    });

    fireEvent.click(screen.getByTestId("bulk-camera-toggle"));

    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/video"))).toBe(3);
    });
    const cameraCalls = state.log.filter((e) => e.method === "POST" && e.url.endsWith("/video"));
    for (const c of cameraCalls) {
      expect(c.body).toEqual({ camera: true });
    }
  });

  it("after 'Camera on all', the button label flips to 'Camera off all' and clicking it sends camera=false", async () => {
    const state: FetchState = { bots: liveBots(2), log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-camera-toggle")).toHaveTextContent(/camera on all/i);
    });

    fireEvent.click(screen.getByTestId("bulk-camera-toggle"));
    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/video"))).toBe(2);
    });

    await waitFor(() => {
      expect(screen.getByTestId("bulk-camera-toggle")).toHaveTextContent(/camera off all/i);
    });
    fireEvent.click(screen.getByTestId("bulk-camera-toggle"));
    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/video"))).toBe(4);
    });
    const cameraCalls = state.log.filter((e) => e.method === "POST" && e.url.endsWith("/video"));
    expect(cameraCalls[2].body).toEqual({ camera: false });
    expect(cameraCalls[3].body).toEqual({ camera: false });
  });

  it("partial failure: 2 succeed + 1 fails → toolbar surfaces a single summary toast with the success / failure split", async () => {
    const bots = liveBots(3);
    const failingBotUrl = `/api/bots/${bots[1].botId}`;
    const state: FetchState = {
      bots,
      log: [],
      fail: {
        [`DELETE ${failingBotUrl}`]: { status: 500, body: { error: "boom" } },
      },
    };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-terminate-all")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByTestId("bulk-terminate-all"));
    const confirmBtn = await screen.findByRole("button", { name: /^terminate all$/i });
    fireEvent.click(confirmBtn);

    // Toolbar surfaces a SINGLE toast summarising the outcome — never
    // N toasts for N bots — so the operator can see "2 terminated, 1
    // failed" at a glance.
    await waitFor(() => {
      // The toast title contains both success and failure counts.
      const matches = screen.queryAllByText(/2 terminated.*1 failed/i);
      expect(matches.length).toBeGreaterThan(0);
    });
  });

  it("excludes remote (SSH) bots from the fan-out, only fires per-bot requests for local bots", async () => {
    const local1 = fakeBot({
      botId: "aaaaaaaa-0000-4000-8000-000000000000",
      participant: "local-1",
      status: "in-meeting",
    });
    const local2 = fakeBot({
      botId: "bbbbbbbb-0000-4000-8000-000000000000",
      participant: "local-2",
      status: "in-meeting",
    });
    const remote = fakeBot({
      botId: "cccccccc-0000-4000-8000-000000000000",
      participant: "remote-1",
      status: "in-meeting",
      host: { kind: "ssh", hostLabel: "remote-host" },
    });
    const state: FetchState = { bots: [local1, local2, remote], log: [] };
    stubFetch(state);
    renderWithClient();
    await waitFor(() => {
      expect(screen.getByTestId("bulk-leave-all")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByTestId("bulk-leave-all"));
    const confirmBtn = await screen.findByRole("button", { name: /^leave all$/i });
    fireEvent.click(confirmBtn);

    await waitFor(() => {
      expect(countCalls(state.log, "POST", (u) => u.endsWith("/leave"))).toBe(2);
    });
    // None of the /leave calls should target the remote bot id.
    const leaveCalls = state.log.filter((e) => e.method === "POST" && e.url.endsWith("/leave"));
    for (const c of leaveCalls) {
      expect(c.url).not.toContain(remote.botId);
    }
  });
});
