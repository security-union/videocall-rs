import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { SsoPanel, deriveSsoStartUrl, deriveSsoTone } from "../components/SsoPanel";

interface MockedRoute {
  url: string;
  method: string;
  status: number;
  body: unknown;
}

function setupFetch(
  routes: MockedRoute[],
  onCall?: (call: { url: string; method: string }) => void,
) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      onCall?.({ url, method });
      const route = routes.find((r) => r.url === url && r.method === method);
      if (!route) {
        return new Response(JSON.stringify({ error: `no mock for ${method} ${url}` }), {
          status: 500,
          headers: { "content-type": "application/json" },
        });
      }
      return new Response(JSON.stringify(route.body), {
        status: route.status,
        headers: { "content-type": "application/json" },
      });
    }),
  );
}

function renderPanel(
  open = true,
  toast?: (t: { title: string; variant: string }) => void,
  meetingURL?: string,
) {
  const qc = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });
  return render(
    <QueryClientProvider client={qc}>
      <SsoPanel open={open} onOpenChange={() => {}} onToast={toast} meetingURL={meetingURL} />
    </QueryClientProvider>,
  );
}

describe("deriveSsoStartUrl", () => {
  // Locks the contract used by the LaunchForm-scoped SsoPanel to
  // auto-derive the SSO capture target from the meeting URL the
  // operator has typed. Critical because a fnxlabs-captured
  // `hcl-sso.json` cannot authenticate a localhost or preview-deploy
  // launch (cookie domain mismatch); this helper closes that gap.
  it("returns null for undefined or empty inputs (falls back to server default)", () => {
    expect(deriveSsoStartUrl(undefined)).toBeNull();
    expect(deriveSsoStartUrl("")).toBeNull();
    expect(deriveSsoStartUrl("   ")).toBeNull();
  });

  it("returns null for malformed URLs", () => {
    expect(deriveSsoStartUrl("not-a-url")).toBeNull();
    expect(deriveSsoStartUrl("ftp://example.com/")).toBeNull();
    expect(deriveSsoStartUrl("javascript:alert(1)")).toBeNull();
  });

  it("returns the origin with a trailing slash for valid http(s) URLs", () => {
    expect(deriveSsoStartUrl("https://app.videocall.fnxlabs.com/meeting/xyz")).toBe(
      "https://app.videocall.fnxlabs.com/",
    );
    expect(deriveSsoStartUrl("http://localhost:8080/meeting/abc")).toBe("http://localhost:8080/");
    expect(
      deriveSsoStartUrl("https://app.videocall-pr-123.preview.videocall.fnxlabs.com/meeting/x"),
    ).toBe("https://app.videocall-pr-123.preview.videocall.fnxlabs.com/");
  });
});

describe("deriveSsoTone", () => {
  it("is red when data is missing or file does not exist", () => {
    expect(deriveSsoTone(undefined)).toBe("red");
    expect(
      deriveSsoTone({
        filePath: "/x",
        exists: false,
        capturedAt: null,
        ageHours: null,
        size: null,
      }),
    ).toBe("red");
  });

  it("is yellow when the file is older than 12h", () => {
    expect(
      deriveSsoTone({
        filePath: "/x",
        exists: true,
        capturedAt: 0,
        ageHours: 18,
        size: 100,
      }),
    ).toBe("yellow");
  });

  it("is green when the file is fresh", () => {
    expect(
      deriveSsoTone({
        filePath: "/x",
        exists: true,
        capturedAt: 0,
        ageHours: 1,
        size: 100,
      }),
    ).toBe("green");
  });
});

describe("<SsoPanel />", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders the VPN section, fetching status on open", async () => {
    setupFetch([
      {
        url: "/api/sso/vpn-status",
        method: "GET",
        status: 200,
        body: { status: "up", checkedAt: Date.now(), responseTimeMs: 42 },
      },
      {
        url: "/api/sso/status",
        method: "GET",
        status: 200,
        body: {
          filePath: "/runDir/auth/hcl-sso.json",
          exists: true,
          capturedAt: Date.now() - 60_000,
          ageHours: 0.016,
          size: 4096,
        },
      },
    ]);
    renderPanel(true);
    await waitFor(() => {
      expect(screen.getByTestId("sso-panel-vpn")).toHaveTextContent(/Reachable/);
    });
    expect(screen.getByTestId("sso-panel-state")).toHaveTextContent(/hcl-sso\.json/);
  });

  it("shows a missing-state warning when the file does not exist", async () => {
    setupFetch([
      {
        url: "/api/sso/vpn-status",
        method: "GET",
        status: 200,
        body: { status: "up", checkedAt: Date.now(), responseTimeMs: 10 },
      },
      {
        url: "/api/sso/status",
        method: "GET",
        status: 200,
        body: {
          filePath: "/runDir/auth/hcl-sso.json",
          exists: false,
          capturedAt: null,
          ageHours: null,
          size: null,
        },
      },
    ]);
    renderPanel(true);
    await waitFor(() => {
      expect(screen.getByTestId("sso-missing")).toBeInTheDocument();
    });
  });

  it("VPN-down message shows the underlying error", async () => {
    setupFetch([
      {
        url: "/api/sso/vpn-status",
        method: "GET",
        status: 200,
        body: { status: "down", checkedAt: Date.now(), error: "timeout" },
      },
      {
        url: "/api/sso/status",
        method: "GET",
        status: 200,
        body: {
          filePath: "/x",
          exists: false,
          capturedAt: null,
          ageHours: null,
          size: null,
        },
      },
    ]);
    renderPanel(true);
    await waitFor(() => {
      expect(screen.getByTestId("sso-panel-vpn")).toHaveTextContent(/timeout/);
    });
  });

  it("recapture flow: start → complete updates state", async () => {
    let ssoStateBody: {
      filePath: string;
      exists: boolean;
      capturedAt: number | null;
      ageHours: number | null;
      size: number | null;
    } = {
      filePath: "/runDir/auth/hcl-sso.json",
      exists: false,
      capturedAt: null,
      ageHours: null,
      size: null,
    };
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
        const method = init?.method ?? "GET";
        if (url === "/api/sso/vpn-status" && method === "GET") {
          return new Response(
            JSON.stringify({ status: "up", checkedAt: Date.now(), responseTimeMs: 5 }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/status" && method === "GET") {
          return new Response(JSON.stringify(ssoStateBody), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url === "/api/sso/recapture" && method === "POST") {
          return new Response(
            JSON.stringify({
              recaptureSessionId: "session-abc",
              startUrl: "https://app.videocall.fnxlabs.com/",
              startedAt: Date.now(),
            }),
            { status: 201, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/recapture/session-abc/complete" && method === "POST") {
          ssoStateBody = {
            filePath: "/runDir/auth/hcl-sso.json",
            exists: true,
            capturedAt: Date.now(),
            ageHours: 0.001,
            size: 1024,
          };
          return new Response(JSON.stringify(ssoStateBody), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        return new Response("{}", { status: 500 });
      }),
    );
    const onToast = vi.fn();
    renderPanel(true, onToast);
    await waitFor(() => {
      expect(screen.getByTestId("sso-recapture-start")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByTestId("sso-recapture-start"));
    await waitFor(() => {
      expect(screen.getByTestId("sso-recapture-active")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByTestId("sso-recapture-complete"));
    await waitFor(() => {
      // After save, the active panel goes away and the SSO line shows
      // the new file path.
      expect(screen.queryByTestId("sso-recapture-active")).not.toBeInTheDocument();
    });
    expect(onToast).toHaveBeenCalledWith(expect.objectContaining({ variant: "success" }));
  });

  it("recapture POST body carries derived startUrl when meetingURL is provided", async () => {
    // Closes the gap that produced the symptom: operator captured
    // against fnxlabs (server's hardcoded default), then launched
    // against localhost — the captured cookies were scoped to
    // `.fnxlabs.com` and didn't apply at localhost, so the Google
    // account picker reappeared. With the meetingURL wired through,
    // the capture navigates to the same origin the bot will launch
    // against, so the captured cookies match by construction.
    const captured: Array<{ url: string; body: unknown }> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
        const method = init?.method ?? "GET";
        if (url === "/api/sso/vpn-status") {
          return new Response(
            JSON.stringify({ status: "up", checkedAt: Date.now(), responseTimeMs: 5 }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/status") {
          return new Response(
            JSON.stringify({
              filePath: "/runDir/auth/hcl-sso.json",
              exists: false,
              capturedAt: null,
              ageHours: null,
              size: null,
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/recapture" && method === "POST") {
          captured.push({ url, body: init?.body ? JSON.parse(init.body as string) : null });
          return new Response(
            JSON.stringify({
              recaptureSessionId: "session-localhost",
              startUrl: "http://localhost:8080/",
              startedAt: Date.now(),
            }),
            { status: 201, headers: { "content-type": "application/json" } },
          );
        }
        return new Response("{}", { status: 500 });
      }),
    );
    renderPanel(true, undefined, "http://localhost:8080/meeting/abc");
    await waitFor(() => screen.getByTestId("sso-recapture-start"));
    fireEvent.click(screen.getByTestId("sso-recapture-start"));
    await waitFor(() => expect(captured).toHaveLength(1));
    expect(captured[0].body).toEqual({ startUrl: "http://localhost:8080/" });
  });

  it("recapture POST body omits startUrl when no meetingURL is supplied (falls back to server default)", async () => {
    // The header-chip's global SsoPanel never has a meeting URL
    // context — it must continue to use the server-side fnxlabs
    // default. Locks this so a future refactor doesn't accidentally
    // require meetingURL.
    const captured: Array<{ url: string; body: unknown }> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
        const method = init?.method ?? "GET";
        if (url === "/api/sso/vpn-status") {
          return new Response(
            JSON.stringify({ status: "up", checkedAt: Date.now(), responseTimeMs: 5 }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/status") {
          return new Response(
            JSON.stringify({
              filePath: "/runDir/auth/hcl-sso.json",
              exists: false,
              capturedAt: null,
              ageHours: null,
              size: null,
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/recapture" && method === "POST") {
          captured.push({ url, body: init?.body ? JSON.parse(init.body as string) : null });
          return new Response(
            JSON.stringify({
              recaptureSessionId: "session-default",
              startUrl: "https://app.videocall.fnxlabs.com/",
              startedAt: Date.now(),
            }),
            { status: 201, headers: { "content-type": "application/json" } },
          );
        }
        return new Response("{}", { status: 500 });
      }),
    );
    renderPanel(true, undefined, undefined);
    await waitFor(() => screen.getByTestId("sso-recapture-start"));
    fireEvent.click(screen.getByTestId("sso-recapture-start"));
    await waitFor(() => expect(captured).toHaveLength(1));
    expect(captured[0].body).toEqual({});
  });

  it("renders the derived capture target so the operator can verify before clicking Start", async () => {
    setupFetch([
      {
        url: "/api/sso/vpn-status",
        method: "GET",
        status: 200,
        body: { status: "up", checkedAt: Date.now(), responseTimeMs: 5 },
      },
      {
        url: "/api/sso/status",
        method: "GET",
        status: 200,
        body: {
          filePath: "/runDir/auth/hcl-sso.json",
          exists: false,
          capturedAt: null,
          ageHours: null,
          size: null,
        },
      },
    ]);
    renderPanel(true, undefined, "http://localhost:8080/meeting/abc");
    await waitFor(() => {
      expect(screen.getByTestId("sso-capture-target")).toHaveTextContent("http://localhost:8080/");
    });
  });

  it("falls back to the (default) label in the capture-target line when no meetingURL is supplied", async () => {
    setupFetch([
      {
        url: "/api/sso/vpn-status",
        method: "GET",
        status: 200,
        body: { status: "up", checkedAt: Date.now(), responseTimeMs: 5 },
      },
      {
        url: "/api/sso/status",
        method: "GET",
        status: 200,
        body: {
          filePath: "/runDir/auth/hcl-sso.json",
          exists: false,
          capturedAt: null,
          ageHours: null,
          size: null,
        },
      },
    ]);
    renderPanel(true, undefined, undefined);
    await waitFor(() => {
      expect(screen.getByTestId("sso-capture-target")).toHaveTextContent(
        "https://app.videocall.fnxlabs.com/ (default)",
      );
    });
  });

  it("recapture cancel hits DELETE and clears the active state", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
        const method = init?.method ?? "GET";
        if (url === "/api/sso/vpn-status") {
          return new Response(
            JSON.stringify({ status: "up", checkedAt: Date.now(), responseTimeMs: 5 }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/status") {
          return new Response(
            JSON.stringify({
              filePath: "/runDir/auth/hcl-sso.json",
              exists: false,
              capturedAt: null,
              ageHours: null,
              size: null,
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/recapture" && method === "POST") {
          return new Response(
            JSON.stringify({
              recaptureSessionId: "session-xyz",
              startUrl: "https://app.videocall.fnxlabs.com/",
              startedAt: Date.now(),
            }),
            { status: 201, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/sso/recapture/session-xyz" && method === "DELETE") {
          return new Response(
            JSON.stringify({ recaptureSessionId: "session-xyz", cancelled: true }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        return new Response("{}", { status: 500 });
      }),
    );
    renderPanel(true);
    await waitFor(() => screen.getByTestId("sso-recapture-start"));
    fireEvent.click(screen.getByTestId("sso-recapture-start"));
    await waitFor(() => screen.getByTestId("sso-recapture-cancel"));
    fireEvent.click(screen.getByTestId("sso-recapture-cancel"));
    await waitFor(() => {
      expect(screen.queryByTestId("sso-recapture-active")).not.toBeInTheDocument();
    });
  });
});
