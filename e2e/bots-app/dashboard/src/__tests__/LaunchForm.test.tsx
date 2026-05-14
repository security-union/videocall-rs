import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { LaunchForm } from "../components/LaunchForm";

function renderWithClient(ui: React.ReactElement) {
  const qc = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

describe("<LaunchForm />", () => {
  beforeEach(() => {
    // Stub /api/assets endpoints called by useQuery in the form.
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string) => {
        if (url.startsWith("/api/assets")) {
          return new Response(JSON.stringify({ files: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        // Default: empty success — the real launch request goes
        // through `api.launch` which uses `fetch("/api/launch")`.
        return new Response(JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }),
    );
  });

  it("renders the meeting URL + participant + TTL inputs", () => {
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    expect(screen.getByTestId("meeting-url")).toBeInTheDocument();
    expect(screen.getByTestId("participant")).toBeInTheDocument();
    expect(screen.getByTestId("ttl")).toBeInTheDocument();
  });

  it("surfaces validation errors before sending the request", async () => {
    const onError = vi.fn();
    const onLaunched = vi.fn();
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={onError} />);
    // Submit with the default-empty meeting URL.
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(screen.getByText(/Meeting URL must be/)).toBeInTheDocument();
    });
    expect(onLaunched).not.toHaveBeenCalled();
  });

  it("submits the launch request when all fields are valid", async () => {
    const onLaunched = vi.fn();
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/TonyBots" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onLaunched).toHaveBeenCalledWith("00000000-0000-0000-0000-000000000000");
    });
  });

  it("requires the storage-state file when auth=storage-state", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/TonyBots" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    // Switch the auth radio. We grab the underlying input by id.
    const storageRadio = document.getElementById("auth-storage-state") as HTMLElement;
    expect(storageRadio).not.toBeNull();
    fireEvent.click(storageRadio);
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(screen.getByText(/Storage-state file path is required/)).toBeInTheDocument();
    });
  });
});
