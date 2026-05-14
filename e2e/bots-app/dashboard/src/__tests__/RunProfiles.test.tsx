import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { RunProfiles } from "../components/RunProfiles";

function renderWithClient(ui: React.ReactElement) {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

interface FetchState {
  profiles: { name: string; savedAt: string; botCount: number }[];
  lastSavePayload?: unknown;
  lastLaunched?: string;
  lastDeleted?: string;
}

function stubFetch(state: FetchState) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/api/profiles" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ profiles: state.profiles }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/profiles" && init?.method === "POST") {
        state.lastSavePayload = JSON.parse(init.body as string);
        return new Response(
          JSON.stringify({
            name: "demo-new",
            savedAt: "2026-05-13T22:00:00Z",
            version: 1,
            bots: [{ participant: "alice" }],
          }),
          { status: 201, headers: { "content-type": "application/json" } },
        );
      }
      const launchMatch = /^\/api\/profiles\/([^/]+)\/launch$/.exec(url);
      if (launchMatch && init?.method === "POST") {
        state.lastLaunched = launchMatch[1];
        return new Response(JSON.stringify({ name: launchMatch[1], botIds: ["abc", "def"] }), {
          status: 202,
          headers: { "content-type": "application/json" },
        });
      }
      const delMatch = /^\/api\/profiles\/([^/]+)$/.exec(url);
      if (delMatch && init?.method === "DELETE") {
        state.lastDeleted = delMatch[1];
        return new Response(JSON.stringify({ name: delMatch[1], deleted: true }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      return new Response(JSON.stringify({ error: "unmocked" }), { status: 500 });
    }),
  );
}

describe("<RunProfiles />", () => {
  beforeEach(() => {
    // jsdom doesn't ship with Pointer Events; Radix Dialog uses them.
    if (typeof window.PointerEvent === "undefined") {
      (window as unknown as { PointerEvent: typeof MouseEvent }).PointerEvent = MouseEvent;
    }
  });

  it("renders the empty-state message when no profiles are saved", async () => {
    stubFetch({ profiles: [] });
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    await waitFor(() => {
      expect(screen.getByTestId("run-profiles-empty")).toBeInTheDocument();
    });
  });

  it("lists saved profiles fetched from the API", async () => {
    stubFetch({
      profiles: [
        { name: "demo-3-bots", savedAt: "2026-05-13T22:00:00Z", botCount: 3 },
        { name: "single-guest", savedAt: "2026-05-13T20:00:00Z", botCount: 1 },
      ],
    });
    renderWithClient(<RunProfiles hasBots={true} onToast={() => {}} />);
    await waitFor(() => {
      expect(screen.getByTestId("run-profile-row-demo-3-bots")).toBeInTheDocument();
      expect(screen.getByTestId("run-profile-row-single-guest")).toBeInTheDocument();
    });
  });

  it("shows an info toast when saving without any live bots", async () => {
    stubFetch({ profiles: [] });
    const toast = vi.fn();
    renderWithClient(<RunProfiles hasBots={false} onToast={toast} />);
    fireEvent.click(screen.getByTestId("run-profiles-save-button"));
    expect(toast).toHaveBeenCalledWith(
      expect.objectContaining({ title: "No bots to save", variant: "info" }),
    );
  });

  it("posts the save request when the dialog is submitted", async () => {
    const state: FetchState = { profiles: [] };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={true} onToast={() => {}} />);
    fireEvent.click(screen.getByTestId("run-profiles-save-button"));
    const input = await screen.findByTestId("save-profile-name");
    fireEvent.change(input, { target: { value: "demo-new" } });
    fireEvent.click(screen.getByTestId("save-profile-submit"));
    await waitFor(() => {
      expect(state.lastSavePayload).toEqual({ name: "demo-new", source: "current" });
    });
  });

  it("invokes the launch endpoint when the per-row Launch button is clicked", async () => {
    const state: FetchState = {
      profiles: [{ name: "to-launch", savedAt: "2026-05-13T22:00:00Z", botCount: 2 }],
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={true} onToast={() => {}} />);
    const launchBtn = await screen.findByTestId("run-profile-launch-to-launch");
    fireEvent.click(launchBtn);
    await waitFor(() => {
      expect(state.lastLaunched).toBe("to-launch");
    });
  });
});
