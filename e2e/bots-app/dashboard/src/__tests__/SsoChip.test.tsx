import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { SsoChip } from "../components/SsoChip";

function renderChip(
  routes: Record<string, { status: number; body: unknown }>,
  onOpen: () => void = () => {},
) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string) => {
      const r = routes[url];
      if (!r) return new Response("{}", { status: 500 });
      return new Response(JSON.stringify(r.body), {
        status: r.status,
        headers: { "content-type": "application/json" },
      });
    }),
  );
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SsoChip onOpen={onOpen} />
    </QueryClientProvider>,
  );
}

describe("<SsoChip />", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders VPN OK + SSO age when both endpoints succeed", async () => {
    renderChip({
      "/api/sso/vpn-status": {
        status: 200,
        body: { status: "up", checkedAt: Date.now(), responseTimeMs: 7 },
      },
      "/api/sso/status": {
        status: 200,
        body: {
          filePath: "/x/hcl-sso.json",
          exists: true,
          capturedAt: Date.now(),
          ageHours: 2.5,
          size: 1024,
        },
      },
    });
    await waitFor(() => {
      expect(screen.getByTestId("sso-chip")).toHaveAttribute("data-vpn-status", "up");
    });
    expect(screen.getByTestId("sso-chip")).toHaveAttribute("data-sso-tone", "green");
    expect(screen.getByTestId("sso-chip")).toHaveTextContent(/VPN OK/);
    expect(screen.getByTestId("sso-chip")).toHaveTextContent(/2\.5h ago/);
  });

  it("renders VPN unreachable + SSO missing when both are bad", async () => {
    renderChip({
      "/api/sso/vpn-status": {
        status: 200,
        body: { status: "down", checkedAt: Date.now(), error: "timeout" },
      },
      "/api/sso/status": {
        status: 200,
        body: {
          filePath: "/x/hcl-sso.json",
          exists: false,
          capturedAt: null,
          ageHours: null,
          size: null,
        },
      },
    });
    await waitFor(() => {
      expect(screen.getByTestId("sso-chip")).toHaveAttribute("data-vpn-status", "down");
    });
    expect(screen.getByTestId("sso-chip")).toHaveAttribute("data-sso-tone", "red");
    expect(screen.getByTestId("sso-chip")).toHaveTextContent(/VPN unreachable/);
    expect(screen.getByTestId("sso-chip")).toHaveTextContent(/SSO missing/);
  });

  it("renders SSO stale when ageHours > 12", async () => {
    renderChip({
      "/api/sso/vpn-status": {
        status: 200,
        body: { status: "up", checkedAt: Date.now(), responseTimeMs: 7 },
      },
      "/api/sso/status": {
        status: 200,
        body: {
          filePath: "/x/hcl-sso.json",
          exists: true,
          capturedAt: Date.now(),
          ageHours: 24,
          size: 1024,
        },
      },
    });
    await waitFor(() => {
      expect(screen.getByTestId("sso-chip")).toHaveAttribute("data-sso-tone", "yellow");
    });
    expect(screen.getByTestId("sso-chip")).toHaveTextContent(/SSO stale/);
  });

  it("invokes onOpen when clicked", async () => {
    const onOpen = vi.fn();
    renderChip(
      {
        "/api/sso/vpn-status": {
          status: 200,
          body: { status: "up", checkedAt: Date.now(), responseTimeMs: 7 },
        },
        "/api/sso/status": {
          status: 200,
          body: {
            filePath: "/x/hcl-sso.json",
            exists: true,
            capturedAt: Date.now(),
            ageHours: 1,
            size: 1024,
          },
        },
      },
      onOpen,
    );
    await waitFor(() => screen.getByTestId("sso-chip"));
    fireEvent.click(screen.getByTestId("sso-chip"));
    expect(onOpen).toHaveBeenCalledOnce();
  });
});
