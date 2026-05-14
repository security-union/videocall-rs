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
  /**
   * Per-profile full payload returned by `GET /api/profiles/:name`.
   * Tests that exercise the Details dialog populate this; tests that
   * don't can leave it empty and the mock returns a 404.
   */
  profilesByName?: Record<string, unknown>;
  lastSavePayload?: unknown;
  lastLaunched?: string;
  lastDeleted?: string;
  /**
   * Maps "<oldName>" → response to return from
   * `POST /api/profiles/:name/rename`. Default is success (200, with a
   * synthesized RunProfile). Set a `{ status, body }` entry to force
   * a specific error (e.g. 409 collision).
   */
  renameResponses?: Record<string, { status: number; body: unknown }>;
  /**
   * Captured request bodies seen by the rename mock, in arrival order.
   * Tests assert on the payload to verify the dialog wires the new
   * name through correctly.
   */
  renameCalls?: { oldName: string; body: unknown }[];
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
      const renameMatch = /^\/api\/profiles\/([^/]+)\/rename$/.exec(url);
      if (renameMatch && init?.method === "POST") {
        const oldName = decodeURIComponent(renameMatch[1]);
        const parsedBody = init.body ? JSON.parse(init.body as string) : {};
        state.renameCalls = state.renameCalls ?? [];
        state.renameCalls.push({ oldName, body: parsedBody });
        const forced = state.renameResponses?.[oldName];
        if (forced) {
          return new Response(JSON.stringify(forced.body), {
            status: forced.status,
            headers: { "content-type": "application/json" },
          });
        }
        const newName = (parsedBody as { newName?: string }).newName ?? "";
        return new Response(
          JSON.stringify({
            name: newName,
            savedAt: "2026-05-14T01:00:00Z",
            version: 1,
            bots: [],
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      const detailMatch = /^\/api\/profiles\/([^/]+)$/.exec(url);
      if (detailMatch && (!init?.method || init.method === "GET")) {
        const name = decodeURIComponent(detailMatch[1]);
        const payload = state.profilesByName?.[name];
        if (payload === undefined) {
          return new Response(JSON.stringify({ error: "not found" }), { status: 404 });
        }
        return new Response(JSON.stringify(payload), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (detailMatch && init?.method === "DELETE") {
        const name = decodeURIComponent(detailMatch[1]);
        state.lastDeleted = name;
        return new Response(JSON.stringify({ name, deleted: true }), {
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

  it("Details button opens the dialog and renders the profile's bots + metadata", async () => {
    const state: FetchState = {
      profiles: [{ name: "demo-3-bots", savedAt: "2026-05-13T22:00:00Z", botCount: 2 }],
      profilesByName: {
        "demo-3-bots": {
          name: "demo-3-bots",
          savedAt: "2026-05-13T22:00:00Z",
          version: 1,
          bots: [
            {
              participant: "alice",
              meetingURL: "https://app.videocall.fnxlabs.com/meeting/TonyBots",
              ttl: "5m",
              headless: false,
              network: "none",
              authBackend: "jwt",
              costume: "pirate.y4m",
              audio: "alice.wav",
            },
            {
              participant: "bob",
              meetingURL: "https://app.videocall.fnxlabs.com/meeting/TonyBots",
              ttl: "10m",
              headless: true,
              network: "wifi-bad",
              authBackend: "none",
            },
          ],
        },
      },
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    const detailsBtn = await screen.findByTestId("run-profile-details-demo-3-bots");
    fireEvent.click(detailsBtn);
    // Dialog appears with the bot table populated.
    await waitFor(() => {
      expect(screen.getByTestId("profile-details-dialog")).toBeInTheDocument();
      expect(screen.getByTestId("profile-details-table")).toBeInTheDocument();
    });
    // Both bot rows are rendered with their participant + override
    // values; the second bot (no costume/audio set) shows "auto-match".
    expect(screen.getByTestId("profile-details-row-0")).toHaveTextContent("alice");
    expect(screen.getByTestId("profile-details-row-0")).toHaveTextContent("pirate.y4m");
    expect(screen.getByTestId("profile-details-row-0")).toHaveTextContent("alice.wav");
    expect(screen.getByTestId("profile-details-row-1")).toHaveTextContent("bob");
    expect(screen.getByTestId("profile-details-row-1")).toHaveTextContent("auto-match");
    // Metadata line shows the schema version + bot count so operators
    // can verify the right test setup before launching.
    expect(screen.getByTestId("profile-details-meta")).toHaveTextContent("schema v1");
    expect(screen.getByTestId("profile-details-meta")).toHaveTextContent("2 bots");
  });

  it("Rename button opens the dialog pre-filled with the current name", async () => {
    const state: FetchState = {
      profiles: [{ name: "current-name", savedAt: "2026-05-13T22:00:00Z", botCount: 1 }],
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    const renameBtn = await screen.findByTestId("run-profile-rename-current-name");
    fireEvent.click(renameBtn);
    await waitFor(() => {
      expect(screen.getByTestId("rename-profile-dialog")).toBeInTheDocument();
    });
    expect(screen.getByTestId("rename-profile-old-name")).toHaveTextContent("current-name");
    const input = screen.getByTestId("rename-profile-input") as HTMLInputElement;
    expect(input.value).toBe("current-name");
  });

  it("submitting Rename invokes the rename API and surfaces a success toast", async () => {
    const state: FetchState = {
      profiles: [{ name: "old-name", savedAt: "2026-05-13T22:00:00Z", botCount: 1 }],
    };
    stubFetch(state);
    const toast = vi.fn();
    renderWithClient(<RunProfiles hasBots={false} onToast={toast} />);
    fireEvent.click(await screen.findByTestId("run-profile-rename-old-name"));
    const input = await screen.findByTestId("rename-profile-input");
    fireEvent.change(input, { target: { value: "new-name" } });
    fireEvent.click(screen.getByTestId("rename-profile-submit"));
    await waitFor(() => {
      expect(state.renameCalls).toEqual([
        { oldName: "old-name", body: { newName: "new-name" } },
      ]);
    });
    await waitFor(() => {
      expect(toast).toHaveBeenCalledWith(
        expect.objectContaining({
          title: "Profile renamed",
          description: "old-name → new-name",
          variant: "success",
        }),
      );
    });
    // Dialog closes after success.
    await waitFor(() => {
      expect(screen.queryByTestId("rename-profile-dialog")).not.toBeInTheDocument();
    });
  });

  it("shows an inline error and keeps the dialog open when the server responds 409", async () => {
    const state: FetchState = {
      profiles: [{ name: "from", savedAt: "2026-05-13T22:00:00Z", botCount: 1 }],
      renameResponses: {
        from: {
          status: 409,
          body: { error: 'profile "taken" already exists' },
        },
      },
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    fireEvent.click(await screen.findByTestId("run-profile-rename-from"));
    const input = await screen.findByTestId("rename-profile-input");
    fireEvent.change(input, { target: { value: "taken" } });
    fireEvent.click(screen.getByTestId("rename-profile-submit"));
    const err = await screen.findByTestId("rename-profile-error");
    expect(err).toHaveTextContent("already exists");
    // Dialog is still open so the operator can fix the value.
    expect(screen.getByTestId("rename-profile-dialog")).toBeInTheDocument();
  });

  it("blocks submit and shows an inline error for an invalid new name", async () => {
    const state: FetchState = {
      profiles: [{ name: "valid", savedAt: "2026-05-13T22:00:00Z", botCount: 1 }],
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    fireEvent.click(await screen.findByTestId("run-profile-rename-valid"));
    const input = await screen.findByTestId("rename-profile-input");
    // A leading hyphen violates the server's regex.
    fireEvent.change(input, { target: { value: "-bad" } });
    fireEvent.click(screen.getByTestId("rename-profile-submit"));
    const err = await screen.findByTestId("rename-profile-error");
    expect(err).toBeInTheDocument();
    // No network call made — client-side validation short-circuits.
    expect(state.renameCalls ?? []).toEqual([]);
  });

  it("blocks submit when the new name is unchanged from the current name", async () => {
    const state: FetchState = {
      profiles: [{ name: "same", savedAt: "2026-05-13T22:00:00Z", botCount: 1 }],
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    fireEvent.click(await screen.findByTestId("run-profile-rename-same"));
    fireEvent.click(await screen.findByTestId("rename-profile-submit"));
    const err = await screen.findByTestId("rename-profile-error");
    expect(err).toHaveTextContent("differ");
    expect(state.renameCalls ?? []).toEqual([]);
  });

  it("Launch button inside the Details dialog calls the launch endpoint", async () => {
    const state: FetchState = {
      profiles: [{ name: "from-dialog", savedAt: "2026-05-13T22:00:00Z", botCount: 1 }],
      profilesByName: {
        "from-dialog": {
          name: "from-dialog",
          savedAt: "2026-05-13T22:00:00Z",
          version: 1,
          bots: [
            {
              participant: "alice",
              meetingURL: "https://app.videocall.fnxlabs.com/meeting/X",
              ttl: "5m",
              headless: false,
              network: "none",
              authBackend: "jwt",
            },
          ],
        },
      },
    };
    stubFetch(state);
    renderWithClient(<RunProfiles hasBots={false} onToast={() => {}} />);
    fireEvent.click(await screen.findByTestId("run-profile-details-from-dialog"));
    // Wait until the dialog query finishes loading so the Launch
    // button has been enabled (the metadata line only appears after
    // `query.data` is populated, which gates the button's `disabled`
    // attribute).
    await screen.findByTestId("profile-details-meta");
    const dialogLaunch = screen.getByTestId("profile-details-launch");
    expect(dialogLaunch).not.toBeDisabled();
    fireEvent.click(dialogLaunch);
    await waitFor(() => {
      expect(state.lastLaunched).toBe("from-dialog");
    });
  });
});
