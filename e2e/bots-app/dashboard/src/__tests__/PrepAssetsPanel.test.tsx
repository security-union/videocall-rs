import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import * as Toast from "@radix-ui/react-toast";

import { PrepAssetsPanel } from "../components/PrepAssetsPanel";

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
  startCalls: unknown[];
  startResponse?: { status: number; body: unknown };
}

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  // Type-erased listener storage.
  private listeners: Map<string, ((e: MessageEvent) => void)[]> = new Map();
  public onerror: (() => void) | null = null;
  public url: string;
  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, fn: EventListener): void {
    const arr = this.listeners.get(type) ?? [];
    arr.push(fn as (e: MessageEvent) => void);
    this.listeners.set(type, arr);
  }
  removeEventListener(type: string, fn: EventListener): void {
    const arr = this.listeners.get(type);
    if (!arr) return;
    const idx = arr.indexOf(fn as (e: MessageEvent) => void);
    if (idx >= 0) arr.splice(idx, 1);
  }
  emit(type: string, data: string): void {
    const arr = this.listeners.get(type);
    if (!arr) return;
    for (const fn of arr) fn(new MessageEvent(type, { data }));
  }
  close(): void {}
}

function stubFetch(state: FetchState) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/api/assets/prep" && init?.method === "POST") {
        state.startCalls.push(JSON.parse((init.body as string) || "{}"));
        const resp = state.startResponse ?? {
          status: 202,
          body: { jobId: "job-1234-5678", status: "running", startedAt: Date.now() },
        };
        return new Response(JSON.stringify(resp.body), {
          status: resp.status,
          headers: { "content-type": "application/json" },
        });
      }
      const statusMatch = /^\/api\/assets\/prep\/([^/]+)$/.exec(url);
      if (statusMatch && (!init?.method || init.method === "GET")) {
        return new Response(
          JSON.stringify({
            jobId: statusMatch[1],
            status: "running",
            startedAt: Date.now(),
            finishedAt: null,
            stdoutLog: [],
            exitCode: null,
            error: null,
            audioPrepped: 0,
            costumesPrepped: 0,
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      return new Response("{}", { status: 200 });
    }),
  );
}

describe("PrepAssetsPanel", () => {
  let state: FetchState;
  beforeEach(() => {
    state = { startCalls: [] };
    stubFetch(state);
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
  });

  it("renders the run button and advanced toggle", () => {
    renderWithClient(<PrepAssetsPanel onToast={() => {}} />);
    expect(screen.getByTestId("prep-assets-run")).toBeInTheDocument();
    expect(screen.getByTestId("prep-assets-advanced-toggle")).toBeInTheDocument();
  });

  it("toggles the advanced fieldset on click", () => {
    renderWithClient(<PrepAssetsPanel onToast={() => {}} />);
    expect(screen.queryByTestId("prep-assets-manifest-path")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("prep-assets-advanced-toggle"));
    expect(screen.getByTestId("prep-assets-manifest-path")).toBeInTheDocument();
  });

  it("sends an empty body by default and opens the log dialog with the returned jobId", async () => {
    renderWithClient(<PrepAssetsPanel onToast={() => {}} />);
    fireEvent.click(screen.getByTestId("prep-assets-run"));
    await waitFor(() => {
      expect(state.startCalls).toHaveLength(1);
    });
    expect(state.startCalls[0]).toEqual({});
    await waitFor(() => {
      expect(screen.getByTestId("prep-assets-log-dialog")).toBeInTheDocument();
    });
    // EventSource should have been instantiated for the job's stream.
    expect(FakeEventSource.instances).toHaveLength(1);
    expect(FakeEventSource.instances[0].url).toBe("/api/assets/prep/job-1234-5678/stream");
  });

  it("includes participants when set", async () => {
    renderWithClient(<PrepAssetsPanel onToast={() => {}} />);
    fireEvent.change(screen.getByTestId("prep-assets-participants"), {
      target: { value: "alice, bob , carol" },
    });
    fireEvent.click(screen.getByTestId("prep-assets-run"));
    await waitFor(() => {
      expect(state.startCalls).toHaveLength(1);
    });
    expect(state.startCalls[0]).toEqual({ participants: ["alice", "bob", "carol"] });
  });

  it("appends streamed log lines into the log pane", async () => {
    renderWithClient(<PrepAssetsPanel onToast={() => {}} />);
    fireEvent.click(screen.getByTestId("prep-assets-run"));
    await waitFor(() => {
      expect(FakeEventSource.instances).toHaveLength(1);
    });
    const es = FakeEventSource.instances[0];
    es.emit("message", "prep-assets: starting (3 participant(s))");
    es.emit("message", "[alice] audio stitched (5 lines) -> /tmp/alice.wav");
    await waitFor(() => {
      expect(screen.getByTestId("prep-assets-log").textContent ?? "").toMatch(/alice.wav/);
    });
  });

  it("recognises the `end` event and renders the final log line", async () => {
    renderWithClient(<PrepAssetsPanel onToast={() => {}} />);
    fireEvent.click(screen.getByTestId("prep-assets-run"));
    await waitFor(() => {
      expect(FakeEventSource.instances).toHaveLength(1);
    });
    const es = FakeEventSource.instances[0];
    es.emit("message", "prep-assets done — 1 audio file(s), 1 costume(s)");
    es.emit("end", JSON.stringify({ status: "done", exitCode: 0 }));
    await waitFor(() => {
      const log = screen.queryByTestId("prep-assets-log");
      expect(log).toBeTruthy();
      expect(log?.textContent ?? "").toMatch(/prep-assets done/);
    });
  });
});
