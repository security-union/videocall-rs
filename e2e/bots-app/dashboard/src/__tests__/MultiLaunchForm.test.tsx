import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { MultiLaunchForm } from "../components/MultiLaunchForm";

function renderWithClient(ui: React.ReactElement) {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

interface FetchState {
  multiLaunchCalls: unknown[];
  multiLaunchResponse?: { status: number; body: unknown };
}

function stubFetch(state: FetchState) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/api/launch/multi" && init?.method === "POST") {
        state.multiLaunchCalls.push(JSON.parse(init.body as string));
        const resp = state.multiLaunchResponse ?? {
          status: 202,
          body: {
            mode: "first-n",
            count: 2,
            seed: null,
            participants: ["alice", "bob"],
            botIds: ["bot-1", "bot-2"],
            errors: [],
          },
        };
        return new Response(JSON.stringify(resp.body), {
          status: resp.status,
          headers: { "content-type": "application/json" },
        });
      }
      return new Response("{}", { status: 200 });
    }),
  );
}

describe("MultiLaunchForm", () => {
  let state: FetchState;
  beforeEach(() => {
    state = { multiLaunchCalls: [] };
    stubFetch(state);
  });

  it("renders both tabs and required fields", () => {
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    expect(screen.getByText(/First-N from manifest/i)).toBeInTheDocument();
    expect(screen.getByText(/Random N \(seeded\)/i)).toBeInTheDocument();
    expect(screen.getByTestId("multi-count")).toBeInTheDocument();
    expect(screen.getByTestId("multi-meeting-url")).toBeInTheDocument();
    expect(screen.getByTestId("multi-ttl")).toBeInTheDocument();
  });

  it("seed + observer toggle appear only in random mode", () => {
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    // First-N is the default; seed should be hidden.
    expect(screen.queryByTestId("multi-seed")).not.toBeInTheDocument();
    expect(screen.queryByTestId("multi-include-observers")).not.toBeInTheDocument();
    // Switch to random.
    fireEvent.click(screen.getByLabelText(/Random N/i));
    expect(screen.getByTestId("multi-seed")).toBeInTheDocument();
    expect(screen.getByTestId("multi-include-observers")).toBeInTheDocument();
  });

  it("rejects submission with an invalid meeting URL", async () => {
    const onError = vi.fn();
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={onError} />);
    // Leave the meeting URL blank; the validator must fire.
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(screen.getByRole("alert")).toBeInTheDocument();
    });
    expect(state.multiLaunchCalls).toHaveLength(0);
  });

  it("submits a first-N launch with the configured fields", async () => {
    const onLaunched = vi.fn();
    renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.mode).toBe("first-n");
    expect(sent.count).toBe(2);
    expect(sent.meetingURL).toBe("https://app.videocall.fnxlabs.com/meeting/X");
    expect(sent).not.toHaveProperty("seed");
    expect(sent).not.toHaveProperty("includeObservers");
    await waitFor(() => expect(onLaunched).toHaveBeenCalled());
  });

  it("submits a random launch with seed + includeObservers", async () => {
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    fireEvent.click(screen.getByLabelText(/Random N/i));
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "3" } });
    fireEvent.change(screen.getByTestId("multi-seed"), { target: { value: "42" } });
    fireEvent.click(screen.getByTestId("multi-include-observers"));
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.mode).toBe("random");
    expect(sent.seed).toBe(42);
    expect(sent.includeObservers).toBe(true);
  });
});
