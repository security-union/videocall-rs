import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { SshCommandPreview, type SshPreviewSpec } from "../components/SshCommandPreview";

function renderWithClient(ui: React.ReactElement) {
  const qc = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

const validSpec: SshPreviewSpec = {
  meetingURL: "https://example.com/meeting/X",
  participant: "alice",
  ttl: "5m",
  headless: true,
  network: "none",
  authBackend: "jwt",
};

function mockPreviewFetch(
  display: string = "ssh -o ConnectTimeout=10 alice@my-host.lan 'cd /home/alice && npm run bot'",
) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (_url: string) => {
      return new Response(
        JSON.stringify({
          argv: ["ssh", "-o", "ConnectTimeout=10", "alice@my-host.lan", "cd /home/alice && npm run bot"],
          display,
          remoteCommand: "cd /home/alice && npm run bot",
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }),
  );
}

describe("<SshCommandPreview />", () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
  });

  it("renders nothing when hostLabel is null", () => {
    mockPreviewFetch();
    const { container } = renderWithClient(
      <SshCommandPreview hostLabel={null} spec={validSpec} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders nothing when hostLabel is the empty string", () => {
    mockPreviewFetch();
    const { container } = renderWithClient(
      <SshCommandPreview hostLabel="" spec={validSpec} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders a collapsed card by default with the header", () => {
    mockPreviewFetch();
    renderWithClient(<SshCommandPreview hostLabel="my-host" spec={validSpec} />);
    expect(screen.getByTestId("ssh-cmd-preview-root")).toBeInTheDocument();
    expect(screen.getByTestId("ssh-cmd-preview-toggle")).toBeInTheDocument();
    // No display rendered while collapsed.
    expect(screen.queryByTestId("ssh-cmd-preview-display")).toBeNull();
  });

  it("expands on toggle click and shows the command preview", async () => {
    mockPreviewFetch("ssh -o ConnectTimeout=10 alice@my-host.lan 'remote command'");
    renderWithClient(<SshCommandPreview hostLabel="my-host" spec={validSpec} />);
    fireEvent.click(screen.getByTestId("ssh-cmd-preview-toggle"));
    await waitFor(() => {
      expect(screen.getByTestId("ssh-cmd-preview-display")).toBeInTheDocument();
    });
    expect(screen.getByTestId("ssh-cmd-preview-display").textContent).toContain(
      "alice@my-host.lan",
    );
  });

  it("debounces spec changes (no fetch fires for 250ms keystroke window)", async () => {
    const fetchMock = vi.fn().mockImplementation(async () => {
      return new Response(
        JSON.stringify({
          argv: ["ssh", "alice@my-host.lan", "cmd"],
          display: "ssh alice@my-host.lan 'cmd'",
          remoteCommand: "cmd",
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    });
    vi.stubGlobal("fetch", fetchMock);
    const { rerender } = renderWithClient(
      <SshCommandPreview hostLabel="my-host" spec={validSpec} />,
    );
    fireEvent.click(screen.getByTestId("ssh-cmd-preview-toggle"));
    // Wait for the initial post-debounce fetch.
    await waitFor(() => {
      expect(fetchMock).toHaveBeenCalledTimes(1);
    });
    // Three quick re-renders should collapse to ONE additional fetch
    // after the debounce window expires.
    const qc = new QueryClient({
      defaultOptions: {
        queries: { retry: false },
        mutations: { retry: false },
      },
    });
    for (const ttl of ["1m", "2m", "3m"]) {
      rerender(
        <QueryClientProvider client={qc}>
          <SshCommandPreview hostLabel="my-host" spec={{ ...validSpec, ttl }} />
        </QueryClientProvider>,
      );
    }
    // No new fetch yet — still within the debounce window.
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("renders an inline error when the API returns one", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async () => {
        return new Response(JSON.stringify({ error: "host not found" }), {
          status: 404,
          headers: { "content-type": "application/json" },
        });
      }),
    );
    renderWithClient(<SshCommandPreview hostLabel="ghost" spec={validSpec} />);
    fireEvent.click(screen.getByTestId("ssh-cmd-preview-toggle"));
    await waitFor(() => {
      expect(screen.getByTestId("ssh-cmd-preview-error")).toBeInTheDocument();
    });
    expect(screen.getByTestId("ssh-cmd-preview-error").textContent).toContain("host not found");
  });

  it("copies the display string to the clipboard when Copy is clicked", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    mockPreviewFetch("ssh -o ConnectTimeout=10 alice@my-host.lan 'remote command'");
    renderWithClient(<SshCommandPreview hostLabel="my-host" spec={validSpec} />);
    fireEvent.click(screen.getByTestId("ssh-cmd-preview-toggle"));
    await waitFor(() => {
      expect(screen.getByTestId("ssh-cmd-preview-display")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByTestId("ssh-cmd-preview-copy"));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(
        "ssh -o ConnectTimeout=10 alice@my-host.lan 'remote command'",
      );
    });
  });

  it("surfaces the subtitle when provided (multi-launch caveat)", () => {
    mockPreviewFetch();
    renderWithClient(
      <SshCommandPreview
        hostLabel="my-host"
        spec={validSpec}
        subtitle="Preview for first participant"
      />,
    );
    expect(screen.getByTestId("ssh-cmd-preview-subtitle").textContent).toContain(
      "Preview for first participant",
    );
  });
});
