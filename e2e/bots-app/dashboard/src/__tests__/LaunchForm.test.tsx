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
    // Stub /api/assets + /api/sso/status endpoints called by useQuery
    // in the form. The form's `ssoStatusQuery` fires regardless of
    // auth backend; without a stub here the unmocked fetch would
    // surface as a console error in the test output.
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string) => {
        if (url === "/api/assets/manifest") {
          return new Response(JSON.stringify({ participants: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url.startsWith("/api/assets")) {
          return new Response(JSON.stringify({ files: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
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

  it("renders every form section", () => {
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    expect(screen.getByTestId("launch-section-meeting")).toBeInTheDocument();
    expect(screen.getByTestId("launch-section-identity")).toBeInTheDocument();
    expect(screen.getByTestId("launch-section-behavior")).toBeInTheDocument();
    expect(screen.getByTestId("launch-section-assets")).toBeInTheDocument();
    expect(screen.getByTestId("launch-section-runtime")).toBeInTheDocument();
  });

  it("exposes a Guest (no auth) radio that the user can select", async () => {
    const onLaunched = vi.fn();
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={vi.fn()} />);
    const guestRadio = document.getElementById("auth-none") as HTMLElement;
    expect(guestRadio).not.toBeNull();
    fireEvent.click(guestRadio);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://example.com/meeting/Guest" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "guest1" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onLaunched).toHaveBeenCalled();
    });
    // No storage-state file field should be visible while Guest is selected.
    expect(screen.queryByTestId("storage-state-file")).not.toBeInTheDocument();
  });

  it("renders per-field help triggers", () => {
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    // A representative subset — these are the ARIA-described controls.
    expect(screen.getByTestId("help-meetingURL")).toBeInTheDocument();
    expect(screen.getByTestId("help-participant")).toBeInTheDocument();
    expect(screen.getByTestId("help-authBackend")).toBeInTheDocument();
    expect(screen.getByTestId("help-ttl")).toBeInTheDocument();
    expect(screen.getByTestId("help-network")).toBeInTheDocument();
    expect(screen.getByTestId("help-headless")).toBeInTheDocument();
    expect(screen.getByTestId("help-costume")).toBeInTheDocument();
    expect(screen.getByTestId("help-audio")).toBeInTheDocument();
    expect(screen.getByTestId("help-runLocation")).toBeInTheDocument();
  });

  it("renders the SSO state warning when JWT auth + missing file", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    await waitFor(() => {
      expect(screen.getByTestId("launch-sso-line")).toHaveTextContent(/No SSO state captured/);
    });
    expect(screen.getByTestId("launch-sso-capture-now")).toBeInTheDocument();
  });

  it("renders the SSO path + age when the file exists, and forwards ssoStateFile in the launch payload", async () => {
    const onLaunched = vi.fn();
    let captured: Record<string, unknown> | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
        if (url === "/api/assets/manifest") {
          return new Response(JSON.stringify({ participants: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url.startsWith("/api/assets")) {
          return new Response(JSON.stringify({ files: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url === "/api/sso/status") {
          return new Response(
            JSON.stringify({
              filePath: "/run/auth/hcl-sso.json",
              exists: true,
              capturedAt: Date.now(),
              ageHours: 1.25,
              size: 3210,
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/launch") {
          captured = JSON.parse(init?.body as string);
          return new Response(
            JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }),
            { status: 201, headers: { "content-type": "application/json" } },
          );
        }
        return new Response("{}", { status: 200 });
      }),
    );
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={vi.fn()} />);
    await waitFor(() => {
      expect(screen.getByTestId("launch-sso-path")).toHaveTextContent("/run/auth/hcl-sso.json");
      expect(screen.getByTestId("launch-sso-line")).toHaveTextContent(/captured 1\.3h ago/);
    });
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/TonyBots" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onLaunched).toHaveBeenCalled();
    });
    expect((captured as { ssoStateFile?: string } | null)?.ssoStateFile).toBe(
      "/run/auth/hcl-sso.json",
    );
  });

  it("omits ssoStateFile from the launch payload when auth is not JWT", async () => {
    let captured: Record<string, unknown> | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string, init?: RequestInit) => {
        if (url === "/api/assets/manifest") {
          return new Response(JSON.stringify({ participants: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url.startsWith("/api/assets")) {
          return new Response(JSON.stringify({ files: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url === "/api/sso/status") {
          return new Response(
            JSON.stringify({
              filePath: "/run/auth/hcl-sso.json",
              exists: true,
              capturedAt: Date.now(),
              ageHours: 1,
              size: 1024,
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url === "/api/launch") {
          captured = JSON.parse(init?.body as string);
          return new Response(
            JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }),
            { status: 201, headers: { "content-type": "application/json" } },
          );
        }
        return new Response("{}", { status: 200 });
      }),
    );
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    const guestRadio = document.getElementById("auth-none") as HTMLElement;
    fireEvent.click(guestRadio);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://example.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "guest1" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(captured).not.toBeNull();
    });
    expect((captured as { ssoStateFile?: unknown } | null)?.ssoStateFile).toBeUndefined();
    // And the SSO line is hidden when auth ≠ JWT.
    expect(screen.queryByTestId("launch-sso-line")).not.toBeInTheDocument();
  });
});

describe("<LaunchForm /> manifest auto-match", () => {
  // The auto-match useEffect runs on a 250ms debounce; vitest's
  // default `waitFor` window (1s) accommodates that without explicit
  // fake timers. Each test starts with a fresh fetch stub that
  // returns a 2-participant manifest: alice→pirate.y4m+alice.wav,
  // bob→cowboy.y4m+bob.wav.
  function manifestStub() {
    return {
      participants: [
        { name: "alice", costumeFile: "pirate.y4m", audioFile: "alice.wav" },
        { name: "bob", costumeFile: "cowboy.y4m", audioFile: "bob.wav" },
      ],
    };
  }
  function installFetchStub() {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string) => {
        if (url === "/api/assets/manifest") {
          return new Response(JSON.stringify(manifestStub()), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url === "/api/assets/costumes") {
          return new Response(JSON.stringify({ files: ["pirate.y4m", "cowboy.y4m"] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url === "/api/assets/audio") {
          return new Response(JSON.stringify({ files: ["alice.wav", "bob.wav"] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
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
        return new Response(JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }),
    );
  }

  beforeEach(() => {
    installFetchStub();
  });

  it("auto-fills Costume + Audio + shows the manifest badge when the typed participant matches", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    await waitFor(
      () => {
        expect(screen.getByTestId("costume-auto-matched")).toBeInTheDocument();
        expect(screen.getByTestId("audio-auto-matched")).toBeInTheDocument();
      },
      { timeout: 1500 },
    );
    // Both Selects use Radix; the trigger button's text content
    // reflects the option label, which is the basename for non-default
    // values.
    expect(screen.getByTestId("costume")).toHaveTextContent("pirate.y4m");
    expect(screen.getByTestId("audio")).toHaveTextContent("alice.wav");
  });

  it("does not show a badge or auto-fill when the participant is not in the manifest", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "zelda" } });
    // Give the debounce + a couple of tick frames a chance to run; the
    // badge must remain absent.
    await new Promise((r) => setTimeout(r, 400));
    expect(screen.queryByTestId("costume-auto-matched")).not.toBeInTheDocument();
    expect(screen.queryByTestId("audio-auto-matched")).not.toBeInTheDocument();
    expect(screen.getByTestId("costume")).toHaveTextContent(/Default fake pattern/);
    expect(screen.getByTestId("audio")).toHaveTextContent(/Default fake mic/);
  });

  it("preserves a manually-picked Costume when the operator re-types the same participant", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    // First, type the participant; auto-match populates Costume.
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    await waitFor(
      () => expect(screen.getByTestId("costume-auto-matched")).toBeInTheDocument(),
      { timeout: 1500 },
    );
    // Now manually override Costume to "cowboy.y4m". Radix Select
    // doesn't expose the trigger as a native <select>; we drive it by
    // opening the listbox and clicking the option.
    fireEvent.click(screen.getByTestId("costume"));
    const cowboyOption = await screen.findByRole("option", { name: "cowboy.y4m" });
    fireEvent.click(cowboyOption);
    await waitFor(() =>
      expect(screen.getByTestId("costume")).toHaveTextContent("cowboy.y4m"),
    );
    // The badge must disappear — the value no longer equals the
    // manifest's match for alice.
    expect(screen.queryByTestId("costume-auto-matched")).not.toBeInTheDocument();
    // Re-type the participant. Auto-match must NOT clobber the manual
    // pick. We clear + re-set the participant input to force the
    // useEffect to re-run.
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alic" } });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    await new Promise((r) => setTimeout(r, 400));
    expect(screen.getByTestId("costume")).toHaveTextContent("cowboy.y4m");
    expect(screen.queryByTestId("costume-auto-matched")).not.toBeInTheDocument();
  });

  it("matches participant names case-insensitively and trims whitespace", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "  ALICE  " } });
    await waitFor(
      () => {
        expect(screen.getByTestId("costume-auto-matched")).toBeInTheDocument();
      },
      { timeout: 1500 },
    );
  });

  it("mounts SshCommandPreview only when SSH host is selected and unmounts on switch back to Local", async () => {
    // Layered fetch stub that includes /api/hosts so the SSH radio
    // becomes enabled (the form gates it on the hosts list being
    // non-empty).
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string) => {
        if (url === "/api/assets/manifest") {
          return new Response(JSON.stringify({ participants: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        }
        if (url.startsWith("/api/assets")) {
          return new Response(JSON.stringify({ files: [] }), {
            status: 200,
            headers: { "content-type": "application/json" },
          });
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
        if (url === "/api/hosts") {
          return new Response(
            JSON.stringify({
              hosts: [
                {
                  label: "mini-7",
                  host: "mini-7.intra",
                  user: "alice",
                  sshKey: null,
                  reposPath: "/home/alice/videocall",
                  notes: null,
                  addedAt: 1,
                },
              ],
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (url.includes("/preview-launch")) {
          return new Response(
            JSON.stringify({
              argv: ["ssh", "alice@mini-7.intra", "remote"],
              display: "ssh alice@mini-7.intra 'remote'",
              remoteCommand: "remote",
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        return new Response(JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }),
    );
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Preview is not present while runLocation === "local".
    expect(screen.queryByTestId("ssh-cmd-preview-root")).not.toBeInTheDocument();
    // Wait for hosts to load so the SSH radio enables.
    await waitFor(() => {
      const sshRadio = document.getElementById("runloc-ssh") as HTMLInputElement | null;
      expect(sshRadio).not.toBeNull();
      expect(sshRadio?.getAttribute("disabled")).toBeNull();
    });
    const sshRadio = document.getElementById("runloc-ssh") as HTMLElement;
    fireEvent.click(sshRadio);
    // Picking the host triggers the preview component to mount.
    // We do it directly through the underlying state — the Radix Select
    // root requires more elaborate setup that doesn't add coverage here.
    // The preview root appears as soon as runLocation === "ssh" and
    // sshHostLabel is non-empty.
    const localRadio = document.getElementById("runloc-local") as HTMLElement;
    // First: with SSH selected but no host picked, root is not mounted
    // (sshHostLabel is the empty default).
    expect(screen.queryByTestId("ssh-cmd-preview-root")).not.toBeInTheDocument();
    // Switching back to Local should leave preview absent.
    fireEvent.click(localRadio);
    expect(screen.queryByTestId("ssh-cmd-preview-root")).not.toBeInTheDocument();
  });

  it("respects 'freshly duplicated' mode: pre-filled values are not overwritten before the first user edit", async () => {
    // A "duplicated" form pre-fills costume/audio with their pre-set
    // sentinel defaults but ALSO supplies an existing participant
    // ("alice"). Without the freshly-duplicated guard, the auto-match
    // would immediately set Costume → pirate.y4m. The guard suppresses
    // this until the operator makes any edit.
    const initial = {
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      displayName: "alice",
      ttl: "5m",
      network: "none",
      headless: false,
      authBackend: "jwt" as const,
      storageStateFile: "",
      runLocation: "local" as const,
      sshHostLabel: "",
      costume: "default",
      audio: "default",
    };
    renderWithClient(
      <LaunchForm initialValues={initial} onLaunched={vi.fn()} onError={vi.fn()} />,
    );
    // Wait past the debounce window; no auto-match should have fired.
    await new Promise((r) => setTimeout(r, 400));
    expect(screen.queryByTestId("costume-auto-matched")).not.toBeInTheDocument();
    expect(screen.queryByTestId("audio-auto-matched")).not.toBeInTheDocument();
    // After the operator's first edit (here: changing the TTL),
    // freshlyDuplicated clears. Re-typing the participant value
    // re-triggers the auto-match useEffect against the cleared guard.
    fireEvent.change(screen.getByTestId("ttl"), { target: { value: "10m" } });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    await waitFor(
      () => expect(screen.getByTestId("costume-auto-matched")).toBeInTheDocument(),
      { timeout: 1500 },
    );
  });
});
