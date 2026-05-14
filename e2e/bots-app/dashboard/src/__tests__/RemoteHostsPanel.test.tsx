import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { RemoteHostsPanel } from "../components/RemoteHostsPanel";

interface State {
  hosts: Array<{
    label: string;
    host: string;
    user: string;
    sshKey: string | null;
    reposPath: string;
    notes: string | null;
    shellInit: string | null;
    addedAt: number;
  }>;
  lastAdded?: unknown;
  lastDeleted?: string;
  testResultByLabel: Record<string, { ok: boolean; latencyMs?: number; error?: string }>;
}

function renderPanel() {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const toasts: unknown[] = [];
  const utils = render(
    <QueryClientProvider client={qc}>
      <RemoteHostsPanel onToast={(t) => toasts.push(t)} />
    </QueryClientProvider>,
  );
  return { ...utils, toasts };
}

function stubFetch(state: State) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/api/hosts" && (!init?.method || init.method === "GET")) {
        return new Response(JSON.stringify({ hosts: state.hosts }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/hosts" && init?.method === "POST") {
        const body = JSON.parse(init.body as string);
        state.lastAdded = body;
        const newHost = {
          label: body.label,
          host: body.host,
          user: body.user ?? "ci",
          sshKey: body.sshKey ?? null,
          reposPath: body.reposPath,
          notes: body.notes ?? null,
          shellInit: body.shellInit ?? null,
          addedAt: Date.now(),
        };
        state.hosts = [...state.hosts, newHost];
        return new Response(JSON.stringify({ host: newHost }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }
      const m = /^\/api\/hosts\/([^/]+)(?:\/(test))?$/.exec(url);
      if (m && init?.method === "DELETE") {
        const label = decodeURIComponent(m[1]);
        state.lastDeleted = label;
        state.hosts = state.hosts.filter((h) => h.label !== label);
        return new Response("null", {
          status: 204,
          headers: { "content-type": "application/json" },
        });
      }
      if (m && m[2] === "test" && init?.method === "POST") {
        const label = decodeURIComponent(m[1]);
        const result = state.testResultByLabel[label] ?? { ok: true, latencyMs: 42 };
        return new Response(JSON.stringify(result), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      return new Response("{}", { status: 200 });
    }),
  );
}

describe("RemoteHostsPanel", () => {
  let state: State;
  beforeEach(() => {
    state = { hosts: [], testResultByLabel: {} };
    stubFetch(state);
  });

  it("renders the empty state when no hosts are registered", async () => {
    renderPanel();
    expect(await screen.findByTestId("remote-hosts-empty")).toBeInTheDocument();
  });

  it("renders one row per registered host with the user@host chip", async () => {
    state.hosts = [
      {
        label: "mini-7",
        host: "mini-7.intra",
        user: "alice",
        sshKey: "/home/alice/.ssh/id_ed25519",
        reposPath: "/home/alice/videocall",
        notes: null,
        shellInit: null,
        addedAt: Date.now(),
      },
    ];
    renderPanel();
    await waitFor(() => {
      expect(screen.getByTestId("remote-host-row-mini-7")).toBeInTheDocument();
    });
    expect(screen.getByText("alice@mini-7.intra")).toBeInTheDocument();
  });

  it("opens the Add host dialog and validates label client-side", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    // Submit with an invalid label.
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "-bad" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "h" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    expect(await screen.findByTestId("remote-host-dialog-error")).toHaveTextContent(
      /Label must be alphanumeric/,
    );
  });

  it("posts to /api/hosts on a valid Add submission", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "mini-7" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "mini-7.intra" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    await waitFor(() => {
      expect(state.lastAdded).toMatchObject({ label: "mini-7", host: "mini-7.intra" });
    });
  });

  it("renders the OK chip after a successful Test probe", async () => {
    state.hosts = [
      {
        label: "good",
        host: "good.intra",
        user: "alice",
        sshKey: null,
        reposPath: "/home/alice/videocall",
        notes: null,
        shellInit: null,
        addedAt: Date.now(),
      },
    ];
    state.testResultByLabel["good"] = { ok: true, latencyMs: 123 };
    renderPanel();
    await screen.findByTestId("remote-host-row-good");
    fireEvent.click(screen.getByTestId("remote-host-test-good"));
    await waitFor(() => {
      expect(screen.getByTestId("remote-host-test-ok-good")).toBeInTheDocument();
    });
  });

  it("renders the Fail chip after a failed Test probe", async () => {
    state.hosts = [
      {
        label: "bad",
        host: "bad.intra",
        user: "alice",
        sshKey: null,
        reposPath: "/home/alice/videocall",
        notes: null,
        shellInit: null,
        addedAt: Date.now(),
      },
    ];
    state.testResultByLabel["bad"] = { ok: false, error: "Permission denied (publickey)." };
    renderPanel();
    await screen.findByTestId("remote-host-row-bad");
    fireEvent.click(screen.getByTestId("remote-host-test-bad"));
    await waitFor(() => {
      expect(screen.getByTestId("remote-host-test-fail-bad")).toBeInTheDocument();
    });
  });

  it("renders per-field help triggers in the Add host dialog", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    // One trigger per form field — matches the testId="help-<field>" props
    // wired through DialogField's `help` slot.
    expect(screen.getByTestId("help-label")).toBeInTheDocument();
    expect(screen.getByTestId("help-host")).toBeInTheDocument();
    expect(screen.getByTestId("help-user")).toBeInTheDocument();
    expect(screen.getByTestId("help-sshKey")).toBeInTheDocument();
    expect(screen.getByTestId("help-reposPath")).toBeInTheDocument();
    expect(screen.getByTestId("help-notes")).toBeInTheDocument();
    expect(screen.getByTestId("help-shellInit")).toBeInTheDocument();
  });

  it("renders per-field help triggers in the Edit host dialog", async () => {
    state.hosts = [
      {
        label: "edit-me",
        host: "edit-me.intra",
        user: "alice",
        sshKey: null,
        reposPath: "/home/alice/videocall",
        notes: null,
        shellInit: null,
        addedAt: Date.now(),
      },
    ];
    renderPanel();
    await screen.findByTestId("remote-host-row-edit-me");
    fireEvent.click(screen.getByTestId("remote-host-edit-edit-me"));
    await screen.findByTestId("remote-host-dialog");
    expect(screen.getByTestId("help-label")).toBeInTheDocument();
    expect(screen.getByTestId("help-host")).toBeInTheDocument();
    expect(screen.getByTestId("help-user")).toBeInTheDocument();
    expect(screen.getByTestId("help-sshKey")).toBeInTheDocument();
    expect(screen.getByTestId("help-reposPath")).toBeInTheDocument();
    expect(screen.getByTestId("help-notes")).toBeInTheDocument();
    expect(screen.getByTestId("help-shellInit")).toBeInTheDocument();
  });

  it("renders the shellInit input in both Add and Edit dialogs", async () => {
    state.hosts = [
      {
        label: "init-host",
        host: "h",
        user: "alice",
        sshKey: null,
        reposPath: "/home/alice/videocall",
        notes: null,
        shellInit: ". ~/.zshrc",
        addedAt: Date.now(),
      },
    ];
    renderPanel();
    // Add dialog
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    const addInput = screen.getByTestId("remote-host-dialog-shellInit") as HTMLInputElement;
    expect(addInput).toBeInTheDocument();
    expect(addInput.value).toBe("");
    // Close the add dialog and open the edit dialog on the seeded row.
    fireEvent.click(screen.getByText("Cancel"));
    await screen.findByTestId("remote-host-row-init-host");
    fireEvent.click(screen.getByTestId("remote-host-edit-init-host"));
    await screen.findByTestId("remote-host-dialog");
    // Edit dialog pre-fills from the row.
    const editInput = screen.getByTestId("remote-host-dialog-shellInit") as HTMLInputElement;
    expect(editInput.value).toBe(". ~/.zshrc");
  });

  it("posts shellInit on Add when the field is filled in", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "zsh-mac" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "zsh-mac.lan" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-shellInit"), {
      target: { value: ". ~/.zshrc" },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    await waitFor(() => {
      expect(state.lastAdded).toMatchObject({
        label: "zsh-mac",
        shellInit: ". ~/.zshrc",
      });
    });
  });

  it("client-side rejects shellInit longer than 512 chars", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "ok-label" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "h" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-shellInit"), {
      target: { value: "a".repeat(513) },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    expect(await screen.findByTestId("remote-host-dialog-error")).toHaveTextContent(
      /Shell init snippet too long/,
    );
  });
});
