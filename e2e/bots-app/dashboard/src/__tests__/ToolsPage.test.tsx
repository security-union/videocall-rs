import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as Toast from "@radix-ui/react-toast";

import { ToolsPage } from "../pages/ToolsPage";

function renderWithClient(ui: React.ReactElement) {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <Toast.Provider>{ui}</Toast.Provider>
    </QueryClientProvider>,
  );
}

interface FetchState {
  sessions: Array<{
    label: string;
    filePath: string;
    capturedAt: number;
    ageHours: number;
    size: number;
  }>;
  lastPreview?: unknown;
  lastLaunchYaml?: string;
  previewError?: { status: number; body: unknown };
}

function stubFetch(state: FetchState) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/api/oauth/sessions" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ sessions: state.sessions }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/launch/from-config/preview" && init?.method === "POST") {
        if (state.previewError) {
          return new Response(JSON.stringify(state.previewError.body), {
            status: state.previewError.status,
            headers: { "content-type": "application/json" },
          });
        }
        state.lastPreview = JSON.parse(init.body as string);
        return new Response(
          JSON.stringify({
            meetingUrl: "https://example.com/meeting/X",
            ttl: "5m",
            network: null,
            auth: null,
            botCount: 2,
            bots: [{ participant: "alice" }, { participant: "bob" }],
            meta: null,
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      if (url === "/api/launch/from-config" && init?.method === "POST") {
        state.lastLaunchYaml = (JSON.parse(init.body as string) as { configYaml: string })
          .configYaml;
        return new Response(
          JSON.stringify({
            meetingUrl: "https://example.com/meeting/X",
            count: 2,
            botIds: ["bot-1", "bot-2"],
            errors: [],
          }),
          { status: 202, headers: { "content-type": "application/json" } },
        );
      }
      return new Response("{}", { status: 200 });
    }),
  );
}

describe("ToolsPage", () => {
  let state: FetchState;
  beforeEach(() => {
    state = { sessions: [] };
    stubFetch(state);
  });

  it("renders the OAuth sessions panel and the config import panel", async () => {
    renderWithClient(<ToolsPage />);
    expect(screen.getByTestId("oauth-sessions-section")).toBeInTheDocument();
    expect(screen.getByTestId("config-import-section")).toBeInTheDocument();
    // Empty state copy.
    await waitFor(() => {
      expect(screen.getByTestId("oauth-sessions-empty")).toBeInTheDocument();
    });
  });

  it("lists captured OAuth sessions", async () => {
    state.sessions = [
      {
        label: "alice",
        filePath: "/tmp/run/auth/alice.json",
        capturedAt: Date.now(),
        ageHours: 1.5,
        size: 200,
      },
      {
        label: "bob",
        filePath: "/tmp/run/auth/bob.json",
        capturedAt: Date.now(),
        ageHours: 2.0,
        size: 300,
      },
    ];
    renderWithClient(<ToolsPage />);
    await waitFor(() => {
      expect(screen.getByTestId("oauth-session-alice")).toBeInTheDocument();
      expect(screen.getByTestId("oauth-session-bob")).toBeInTheDocument();
    });
  });

  it("preview button hits the preview endpoint and renders the count", async () => {
    renderWithClient(<ToolsPage />);
    const textarea = screen.getByTestId("config-import-textarea");
    fireEvent.change(textarea, {
      target: {
        value:
          "meeting_url: https://example.com/meeting/X\nbots:\n  - participant: alice\n  - participant: bob\n",
      },
    });
    fireEvent.click(screen.getByTestId("config-import-preview-button"));
    await waitFor(() => {
      expect(screen.getByTestId("config-import-preview-count")).toHaveTextContent("2");
    });
  });

  it("launch button posts the YAML to /api/launch/from-config", async () => {
    renderWithClient(<ToolsPage />);
    const yaml =
      "meeting_url: https://example.com/meeting/X\nbots:\n  - participant: alice\n";
    fireEvent.change(screen.getByTestId("config-import-textarea"), { target: { value: yaml } });
    fireEvent.click(screen.getByTestId("config-import-launch-button"));
    await waitFor(() => {
      expect(state.lastLaunchYaml).toBe(yaml);
    });
  });

  it("preview surfaces parser errors instead of crashing", async () => {
    state.previewError = {
      status: 400,
      body: { error: "meeting config parse failed: meeting_url must be a non-empty string" },
    };
    renderWithClient(<ToolsPage />);
    fireEvent.change(screen.getByTestId("config-import-textarea"), {
      target: { value: "garbage" },
    });
    fireEvent.click(screen.getByTestId("config-import-preview-button"));
    await waitFor(() => {
      expect(screen.getByTestId("config-import-preview-error")).toBeInTheDocument();
    });
  });
});
