import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { RemoteHostsPanel } from "../components/RemoteHostsPanel";

interface StoredHost {
  label: string;
  host: string;
  user: string;
  sshKey: string | null;
  reposPath: string;
  notes: string | null;
  shell: string | null;
  profileFile: string | null;
  preCommand: string | null;
  forwardSsoState: boolean;
  addedAt: number;
}

interface State {
  hosts: StoredHost[];
  lastAdded?: Record<string, unknown>;
  lastPreviewed?: Record<string, unknown>;
  previewCallCount: number;
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
        const newHost: StoredHost = {
          label: body.label,
          host: body.host,
          user: body.user ?? "ci",
          sshKey: body.sshKey ?? null,
          reposPath: body.reposPath,
          notes: body.notes ?? null,
          shell: body.shell ?? null,
          profileFile: body.profileFile ?? null,
          preCommand: body.preCommand ?? null,
          forwardSsoState: body.forwardSsoState ?? true,
          addedAt: Date.now(),
        };
        state.hosts = [...state.hosts, newHost];
        return new Response(JSON.stringify({ host: newHost }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }
      if (url === "/api/hosts/preview" && init?.method === "POST") {
        const body = JSON.parse(init.body as string);
        state.lastPreviewed = body;
        state.previewCallCount += 1;
        // Build a deterministic display string from the body fields so
        // tests can assert that the preview reflects the form state.
        const h = body.host as {
          user?: string;
          host: string;
          shell?: string | null;
          profileFile?: string | null;
          preCommand?: string | null;
          reposPath: string;
        };
        const profilePart =
          h.profileFile && h.profileFile !== ""
            ? `[ -f ${h.profileFile} ] && . ${h.profileFile}; `
            : "";
        const prePart = h.preCommand && h.preCommand !== "" ? `${h.preCommand}; ` : "";
        const display =
          `ssh -o ConnectTimeout=10 ${h.user ?? "ci"}@${h.host} '` +
          `${h.shell ?? "bash"} -lc "${profilePart}${prePart}cd ${h.reposPath}/e2e && npm run bot"'`;
        return new Response(
          JSON.stringify({ argv: ["ssh", "..."], display, remoteCommand: "npm run bot" }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
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

function legacyHost(over: Partial<StoredHost> = {}): StoredHost {
  return {
    label: "mini-7",
    host: "mini-7.intra",
    user: "alice",
    sshKey: null,
    reposPath: "/home/alice/videocall",
    notes: null,
    shell: null,
    profileFile: null,
    preCommand: null,
    forwardSsoState: true,
    addedAt: Date.now(),
    ...over,
  };
}

describe("RemoteHostsPanel", () => {
  let state: State;
  beforeEach(() => {
    state = { hosts: [], testResultByLabel: {}, previewCallCount: 0 };
    stubFetch(state);
  });

  it("renders the empty state when no hosts are registered", async () => {
    renderPanel();
    expect(await screen.findByTestId("remote-hosts-empty")).toBeInTheDocument();
  });

  it("renders one row per registered host with the user@host chip", async () => {
    state.hosts = [
      legacyHost({
        label: "mini-7",
        host: "mini-7.intra",
        user: "alice",
        sshKey: "/home/alice/.ssh/id_ed25519",
        reposPath: "/home/alice/videocall",
      }),
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

  it("posts to /api/hosts on a valid Add submission with the default shell=bash + profileFile=~/.bash_profile", async () => {
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
      expect(state.lastAdded).toMatchObject({
        label: "mini-7",
        host: "mini-7.intra",
        shell: "bash",
        profileFile: "~/.bash_profile",
        preCommand: null,
      });
    });
  });

  it("renders the OK chip after a successful Test probe", async () => {
    state.hosts = [legacyHost({ label: "good", host: "good.intra" })];
    state.testResultByLabel["good"] = { ok: true, latencyMs: 123 };
    renderPanel();
    await screen.findByTestId("remote-host-row-good");
    fireEvent.click(screen.getByTestId("remote-host-test-good"));
    await waitFor(() => {
      expect(screen.getByTestId("remote-host-test-ok-good")).toBeInTheDocument();
    });
  });

  it("renders the Fail chip after a failed Test probe", async () => {
    state.hosts = [legacyHost({ label: "bad", host: "bad.intra" })];
    state.testResultByLabel["bad"] = { ok: false, error: "Permission denied (publickey)." };
    renderPanel();
    await screen.findByTestId("remote-host-row-bad");
    fireEvent.click(screen.getByTestId("remote-host-test-bad"));
    await waitFor(() => {
      expect(screen.getByTestId("remote-host-test-fail-bad")).toBeInTheDocument();
    });
  });

  it("renders per-field help triggers in the Add host dialog (including the new shell/profile/preCommand triggers)", async () => {
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
    expect(screen.getByTestId("help-shell")).toBeInTheDocument();
    expect(screen.getByTestId("help-profileFile")).toBeInTheDocument();
    expect(screen.getByTestId("help-preCommand")).toBeInTheDocument();
  });

  it("renders per-field help triggers in the Edit host dialog", async () => {
    state.hosts = [legacyHost({ label: "edit-me", host: "edit-me.intra" })];
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
    expect(screen.getByTestId("help-shell")).toBeInTheDocument();
    expect(screen.getByTestId("help-profileFile")).toBeInTheDocument();
    expect(screen.getByTestId("help-preCommand")).toBeInTheDocument();
  });

  it("renders the shell radio group + profileFile / preCommand inputs in the Add dialog", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    // Radio options for each canonical shell + a custom slot.
    expect(screen.getByTestId("remote-host-dialog-shell-bash")).toBeInTheDocument();
    expect(screen.getByTestId("remote-host-dialog-shell-zsh")).toBeInTheDocument();
    expect(screen.getByTestId("remote-host-dialog-shell-sh")).toBeInTheDocument();
    expect(screen.getByTestId("remote-host-dialog-shell-custom")).toBeInTheDocument();
    // Default radio: bash. Default profileFile (hint): ~/.bash_profile.
    expect(
      (screen.getByTestId("remote-host-dialog-shell-bash") as HTMLInputElement).checked,
    ).toBe(true);
    const profileInput = screen.getByTestId(
      "remote-host-dialog-profileFile",
    ) as HTMLInputElement;
    expect(profileInput.value).toBe("~/.bash_profile");
    const preCommandInput = screen.getByTestId(
      "remote-host-dialog-preCommand",
    ) as HTMLInputElement;
    expect(preCommandInput).toBeInTheDocument();
    expect(preCommandInput.value).toBe("");
  });

  it("switching the shell radio to zsh updates the profileFile hint to ~/.zshrc when empty", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    const profileInput = screen.getByTestId(
      "remote-host-dialog-profileFile",
    ) as HTMLInputElement;
    // First clear the bash default, then switch to zsh — the empty input
    // should re-hint to ~/.zshrc.
    fireEvent.change(profileInput, { target: { value: "" } });
    fireEvent.click(screen.getByTestId("remote-host-dialog-shell-zsh"));
    await waitFor(() => {
      expect(profileInput.value).toBe("~/.zshrc");
    });
  });

  it("does NOT overwrite a manually-typed profileFile when shell changes", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    const profileInput = screen.getByTestId(
      "remote-host-dialog-profileFile",
    ) as HTMLInputElement;
    fireEvent.change(profileInput, { target: { value: "/etc/profile" } });
    fireEvent.click(screen.getByTestId("remote-host-dialog-shell-zsh"));
    // The hint must NOT clobber the operator's manual value.
    expect(profileInput.value).toBe("/etc/profile");
  });

  it("pre-fills the structured fields when opening the Edit dialog on a host with custom shell config", async () => {
    state.hosts = [
      legacyHost({
        label: "zsh-host",
        host: "zsh-host.lan",
        shell: "zsh",
        profileFile: "~/.zshrc",
        preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
      }),
    ];
    renderPanel();
    await screen.findByTestId("remote-host-row-zsh-host");
    fireEvent.click(screen.getByTestId("remote-host-edit-zsh-host"));
    await screen.findByTestId("remote-host-dialog");
    expect(
      (screen.getByTestId("remote-host-dialog-shell-zsh") as HTMLInputElement).checked,
    ).toBe(true);
    expect(
      (screen.getByTestId("remote-host-dialog-profileFile") as HTMLInputElement).value,
    ).toBe("~/.zshrc");
    expect(
      (screen.getByTestId("remote-host-dialog-preCommand") as HTMLInputElement).value,
    ).toBe(". ~/.nvm/nvm.sh && nvm use 22");
  });

  it("custom-path shell lands on the custom radio and exposes a free-form text input", async () => {
    state.hosts = [
      legacyHost({
        label: "homebrew-zsh",
        host: "homebrew-zsh.lan",
        shell: "/opt/homebrew/bin/zsh",
        profileFile: "/etc/profile",
      }),
    ];
    renderPanel();
    await screen.findByTestId("remote-host-row-homebrew-zsh");
    fireEvent.click(screen.getByTestId("remote-host-edit-homebrew-zsh"));
    await screen.findByTestId("remote-host-dialog");
    expect(
      (screen.getByTestId("remote-host-dialog-shell-custom") as HTMLInputElement).checked,
    ).toBe(true);
    const customInput = screen.getByTestId(
      "remote-host-dialog-shell-custom-path",
    ) as HTMLInputElement;
    expect(customInput.value).toBe("/opt/homebrew/bin/zsh");
  });

  it("posts shell + profileFile + preCommand on Add when the fields are filled in", async () => {
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
    fireEvent.click(screen.getByTestId("remote-host-dialog-shell-zsh"));
    // Wait for the profileFile hint to flip to ~/.zshrc (the hint only
    // fires when the slot is still empty; we cleared it explicitly to
    // avoid a race with the initial bash default).
    const profileInput = screen.getByTestId(
      "remote-host-dialog-profileFile",
    ) as HTMLInputElement;
    fireEvent.change(profileInput, { target: { value: "~/.zshrc" } });
    fireEvent.change(screen.getByTestId("remote-host-dialog-preCommand"), {
      target: { value: ". ~/.nvm/nvm.sh && nvm use 22" },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    await waitFor(() => {
      expect(state.lastAdded).toMatchObject({
        label: "zsh-mac",
        shell: "zsh",
        profileFile: "~/.zshrc",
        preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
      });
    });
  });

  it("client-side rejects preCommand longer than 512 chars", async () => {
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
    fireEvent.change(screen.getByTestId("remote-host-dialog-preCommand"), {
      target: { value: "a".repeat(513) },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    expect(await screen.findByTestId("remote-host-dialog-error")).toHaveTextContent(
      /Pre-command too long/,
    );
  });

  it("client-side rejects profileFile with shell metacharacters", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "evil-profile" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "h" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-profileFile"), {
      target: { value: "~/.bash_profile;rm -rf /" },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    expect(await screen.findByTestId("remote-host-dialog-error")).toHaveTextContent(
      /Profile file must be a/,
    );
  });

  it("renders the Forward SSO state toggle defaulting to ON in the Add dialog", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    const toggle = await screen.findByTestId("remote-host-dialog-forwardSsoState");
    // Radix Switch keeps state on the data-state attribute (not the
    // checked DOM prop) — `checked` translates to data-state="checked".
    expect(toggle.getAttribute("data-state")).toBe("checked");
    // The help popover trigger for the new field must be wired in
    // exactly like the other field-specific triggers.
    expect(screen.getByTestId("help-forwardSsoState")).toBeInTheDocument();
  });

  it("persists forwardSsoState: true by default when the operator submits the Add form", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "sso-default" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "sso-default.intra" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    await waitFor(() => {
      expect(state.lastAdded).toMatchObject({
        label: "sso-default",
        forwardSsoState: true,
      });
    });
  });

  it("submits forwardSsoState: false when the operator flips the toggle OFF before saving", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    fireEvent.change(screen.getByTestId("remote-host-dialog-label"), {
      target: { value: "sso-off" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "sso-off.intra" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    // Flip the switch — Radix Switch responds to click on its root button.
    fireEvent.click(screen.getByTestId("remote-host-dialog-forwardSsoState"));
    fireEvent.click(screen.getByTestId("remote-host-dialog-submit"));
    await waitFor(() => {
      expect(state.lastAdded).toMatchObject({
        label: "sso-off",
        forwardSsoState: false,
      });
    });
  });

  it("pre-fills the toggle from the existing forwardSsoState when opening the Edit dialog", async () => {
    state.hosts = [
      legacyHost({
        label: "edit-sso-off",
        host: "edit-sso-off.lan",
        forwardSsoState: false,
      }),
    ];
    renderPanel();
    await screen.findByTestId("remote-host-row-edit-sso-off");
    fireEvent.click(screen.getByTestId("remote-host-edit-edit-sso-off"));
    await screen.findByTestId("remote-host-dialog");
    const toggle = screen.getByTestId("remote-host-dialog-forwardSsoState");
    expect(toggle.getAttribute("data-state")).toBe("unchecked");
  });

  it("renders the live Sample command card and posts /api/hosts/preview as the operator types", async () => {
    renderPanel();
    fireEvent.click(screen.getByTestId("remote-hosts-add"));
    await screen.findByTestId("remote-host-dialog");
    // Empty form → the preview surface should display the empty-state
    // hint (preview fetch is short-circuited until host+reposPath are
    // filled in).
    expect(
      screen.getByTestId("remote-host-dialog-sample-cmd-empty"),
    ).toBeInTheDocument();
    fireEvent.change(screen.getByTestId("remote-host-dialog-host"), {
      target: { value: "lab.intra" },
    });
    fireEvent.change(screen.getByTestId("remote-host-dialog-reposPath"), {
      target: { value: "/home/alice/videocall" },
    });
    // The preview is debounced 200ms — wait for the fetch to land and the
    // rendered display string to appear in the card.
    await waitFor(
      () => {
        expect(
          screen.getByTestId("remote-host-dialog-sample-cmd-display"),
        ).toBeInTheDocument();
      },
      { timeout: 2_000 },
    );
    // The stub builds a deterministic display string from the body — assert
    // it contains the bits that should reflect the form's current state.
    const displayEl = screen.getByTestId("remote-host-dialog-sample-cmd-display");
    expect(displayEl).toHaveTextContent("@lab.intra");
    expect(displayEl).toHaveTextContent("/home/alice/videocall/e2e");
    expect(displayEl).toHaveTextContent("bash -lc");
    // The preview body must include the structured fields the operator
    // would otherwise have to save first.
    expect(state.lastPreviewed).toMatchObject({
      host: expect.objectContaining({
        host: "lab.intra",
        reposPath: "/home/alice/videocall",
        shell: "bash",
        profileFile: "~/.bash_profile",
        // The forwardSsoState toggle is part of the preview body so the
        // server's `/hosts/preview` endpoint applies the same SSO wrap
        // decision the launcher will, deterministically.
        forwardSsoState: true,
      }),
    });
  });
});
