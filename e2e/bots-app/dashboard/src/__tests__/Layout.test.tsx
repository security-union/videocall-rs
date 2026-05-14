import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { Layout } from "../components/Layout";
import { ThemeProvider } from "../lib/theme";

function renderLayout(daemonBody: Record<string, unknown> | null) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string) => {
      if (url === "/api/daemon") {
        return new Response(daemonBody ? JSON.stringify(daemonBody) : "null", {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/healthz") {
        return new Response(JSON.stringify({ ok: true, bots: 0 }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      return new Response(JSON.stringify({}), { status: 200 });
    }),
  );
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <ThemeProvider initialMode="light">
      <QueryClientProvider client={qc}>
        <Layout currentRoute="bots" onNavigate={() => {}}>
          <div>page</div>
        </Layout>
      </QueryClientProvider>
    </ThemeProvider>,
  );
}

describe("<Layout />", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders the Help nav item alongside Bots and About", () => {
    renderLayout({ port: 1234, pid: 9999, startedAt: "2026-05-13T22:00:00Z", mode: "attached" });
    expect(screen.getByText("Bots")).toBeInTheDocument();
    expect(screen.getByText("Help")).toBeInTheDocument();
    expect(screen.getByText("About")).toBeInTheDocument();
  });

  it("shows the self-hosted daemon label when mode=self-hosted", async () => {
    renderLayout({
      port: 1234,
      pid: 4242,
      startedAt: "2026-05-13T22:00:00Z",
      mode: "self-hosted",
    });
    await waitFor(() => {
      expect(screen.getByTestId("daemon-status")).toHaveTextContent(/Self-hosted daemon/);
      expect(screen.getByTestId("daemon-status")).toHaveTextContent(/pid 4242/);
    });
  });

  it("shows the attached-ctl-port label when mode=attached", async () => {
    renderLayout({
      port: 5555,
      pid: 1111,
      startedAt: "2026-05-13T22:00:00Z",
      mode: "attached",
    });
    await waitFor(() => {
      expect(screen.getByTestId("daemon-status")).toHaveTextContent(/ctl :5555/);
    });
  });
});
