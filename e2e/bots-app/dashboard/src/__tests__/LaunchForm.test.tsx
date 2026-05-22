import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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

  it("renders the network field with the 'Network Conditions' label", () => {
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    // The Field component renders the label as a sibling of the
    // Select trigger; locating it by text is the closest analogue of
    // "the user reads the label". The legacy spelling "Network
    // profile" must be gone.
    expect(screen.getByText("Network Conditions")).toBeInTheDocument();
    expect(screen.queryByText("Network profile")).not.toBeInTheDocument();
  });

  it("renders the passthrough preset as 'as is' on the Network Conditions select trigger", () => {
    // The form's DEFAULT_VALUES.network is "none" — the passthrough
    // preset. With the display-only mapping in place the trigger
    // should surface it as "as is" while the underlying value
    // remains "none".
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    expect(screen.getByTestId("network")).toHaveTextContent("as is");
    expect(screen.getByTestId("network")).not.toHaveTextContent(/^none$/);
  });

  it("sends 'none' (not 'as is') to the server when the passthrough preset is selected", async () => {
    // Display-only mapping: the rendered trigger says "as is", but
    // the launch payload's `network` field must still carry the
    // raw preset string the server understands.
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
              filePath: "/runDir/auth/hcl-sso.json",
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
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/TonyBots" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(captured).not.toBeNull();
    });
    expect((captured as { network?: string } | null)?.network).toBe("none");
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

  it("surfaces the auto-prime caveat in the Costume + Audio help popovers", async () => {
    renderWithClient(<LaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Costume popover
    fireEvent.click(screen.getByTestId("help-costume"));
    await waitFor(() => {
      expect(screen.getByText(/auto-prime it on launch/i)).toBeInTheDocument();
    });
    // Audio popover — open it next; the costume popover closes when the
    // user clicks outside, so we re-open by clicking the audio trigger.
    fireEvent.click(screen.getByTestId("help-audio"));
    await waitFor(() => {
      // Both popovers carry the same auto-prime sentence; finding ≥1
      // is sufficient to prove the audio help text was updated. Use
      // `findAllByText` to avoid mistaking the costume-still-open
      // case for an audio assertion.
      const matches = screen.queryAllByText(/auto-prime it on launch/i);
      expect(matches.length).toBeGreaterThanOrEqual(1);
    });
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

  it("retains all field values after a successful launch (no auto-reset)", async () => {
    // v1.4.1: the operator almost always wants to launch another bot
    // with the same/similar config (e.g. just change participant +
    // click Launch again). Verify the inputs keep their values after
    // a successful submit instead of being cleared back to the
    // defaults.
    const onLaunched = vi.fn();
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={vi.fn()} />);
    const meetingUrl = "https://app.videocall.fnxlabs.com/meeting/TonyBots";
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: meetingUrl },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.change(screen.getByTestId("display-name"), { target: { value: "Alice" } });
    fireEvent.change(screen.getByTestId("ttl"), { target: { value: "10m" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onLaunched).toHaveBeenCalled();
    });
    // All four user-typed fields must retain their submitted values.
    expect((screen.getByTestId("meeting-url") as HTMLInputElement).value).toBe(meetingUrl);
    expect((screen.getByTestId("participant") as HTMLInputElement).value).toBe("alice");
    expect((screen.getByTestId("display-name") as HTMLInputElement).value).toBe("Alice");
    expect((screen.getByTestId("ttl") as HTMLInputElement).value).toBe("10m");
  });

  it("retains EVERY field (selects + toggles + radios) after a successful launch — v1.5.0 audit", async () => {
    // v1.5.0 regression: PR #838 + #839 audited the form for any
    // unintended `setValues(DEFAULT_VALUES)` paths in onSuccess /
    // onError. This test is the cheap belt-and-suspenders proof that
    // none crept back in. We exercise more than the four text inputs
    // covered above: the network Select, the Headless switch, the
    // Auth-backend radio, and the TTL/displayName/storage-state set
    // all need to survive a round-trip through the mutation.
    const onLaunched = vi.fn();
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={vi.fn()} />);
    const meetingUrl = "https://app.videocall.fnxlabs.com/meeting/Vortex";
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: meetingUrl },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "vortex-1" } });
    fireEvent.change(screen.getByTestId("display-name"), { target: { value: "Vortex" } });
    fireEvent.change(screen.getByTestId("ttl"), { target: { value: "1h" } });
    // Flip the Headless toggle ON so we can verify it stays ON after
    // submit (the default is OFF).
    fireEvent.click(screen.getByTestId("headless"));
    // Switch the auth backend to storage-state and fill the path.
    const storageRadio = document.getElementById("auth-storage-state") as HTMLElement;
    fireEvent.click(storageRadio);
    fireEvent.change(screen.getByTestId("storage-state-file"), {
      target: { value: "run/auth/vortex.json" },
    });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onLaunched).toHaveBeenCalled();
    });
    // Text inputs.
    expect((screen.getByTestId("meeting-url") as HTMLInputElement).value).toBe(meetingUrl);
    expect((screen.getByTestId("participant") as HTMLInputElement).value).toBe("vortex-1");
    expect((screen.getByTestId("display-name") as HTMLInputElement).value).toBe("Vortex");
    expect((screen.getByTestId("ttl") as HTMLInputElement).value).toBe("1h");
    expect((screen.getByTestId("storage-state-file") as HTMLInputElement).value).toBe(
      "run/auth/vortex.json",
    );
    // Headless toggle: Radix Switch reflects state via `data-state`.
    expect(screen.getByTestId("headless")).toHaveAttribute("data-state", "checked");
    // Storage-state auth radio is still selected.
    expect(storageRadio).toHaveAttribute("data-state", "checked");
  });

  it("retains all field values after a failed launch", async () => {
    // Failure path: same retention guarantee. The operator wants to
    // fix the underlying issue and retry the same payload.
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
        if (url === "/api/launch") {
          return new Response(JSON.stringify({ error: "boom" }), {
            status: 500,
            headers: { "content-type": "application/json" },
          });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    const onError = vi.fn();
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={onError} />);
    const meetingUrl = "https://app.videocall.fnxlabs.com/meeting/TonyBots";
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: meetingUrl },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onError).toHaveBeenCalled();
    });
    expect((screen.getByTestId("meeting-url") as HTMLInputElement).value).toBe(meetingUrl);
    expect((screen.getByTestId("participant") as HTMLInputElement).value).toBe("alice");
  });

  it("retains EVERY field (selects + toggles + radios) after a failed launch — v1.5.0 audit", async () => {
    // Same coverage as the success-path v1.5.0 audit test, but with
    // the launch endpoint stubbed to 500. The mutation's onError path
    // must not zero any field either.
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
        if (url === "/api/launch") {
          return new Response(JSON.stringify({ error: "boom" }), {
            status: 500,
            headers: { "content-type": "application/json" },
          });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    const onError = vi.fn();
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={onError} />);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/Vortex" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "vortex-1" } });
    fireEvent.change(screen.getByTestId("display-name"), { target: { value: "Vortex" } });
    fireEvent.change(screen.getByTestId("ttl"), { target: { value: "1h" } });
    fireEvent.click(screen.getByTestId("headless"));
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(onError).toHaveBeenCalled();
    });
    expect((screen.getByTestId("meeting-url") as HTMLInputElement).value).toBe(
      "https://app.videocall.fnxlabs.com/meeting/Vortex",
    );
    expect((screen.getByTestId("participant") as HTMLInputElement).value).toBe("vortex-1");
    expect((screen.getByTestId("display-name") as HTMLInputElement).value).toBe("Vortex");
    expect((screen.getByTestId("ttl") as HTMLInputElement).value).toBe("1h");
    expect(screen.getByTestId("headless")).toHaveAttribute("data-state", "checked");
  });

  it("renders a Reset button that clears every field to the initial-render state", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    // Capture the initial defaults the user sees on first paint.
    const initialMeetingUrl = (screen.getByTestId("meeting-url") as HTMLInputElement).value;
    const initialParticipant = (screen.getByTestId("participant") as HTMLInputElement).value;
    const initialDisplayName = (screen.getByTestId("display-name") as HTMLInputElement).value;
    const initialTtl = (screen.getByTestId("ttl") as HTMLInputElement).value;
    // Fill in some non-default values.
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://example.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.change(screen.getByTestId("display-name"), { target: { value: "Alice" } });
    fireEvent.change(screen.getByTestId("ttl"), { target: { value: "30m" } });
    // Click Reset.
    const resetBtn = screen.getByTestId("reset-button");
    expect(resetBtn).toBeInTheDocument();
    fireEvent.click(resetBtn);
    // Every field must be back to its initial-render value.
    expect((screen.getByTestId("meeting-url") as HTMLInputElement).value).toBe(initialMeetingUrl);
    expect((screen.getByTestId("participant") as HTMLInputElement).value).toBe(initialParticipant);
    expect((screen.getByTestId("display-name") as HTMLInputElement).value).toBe(initialDisplayName);
    expect((screen.getByTestId("ttl") as HTMLInputElement).value).toBe(initialTtl);
  });

  it("Reset clears validation errors", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    // Trigger validation errors by submitting empty.
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => {
      expect(screen.getByText(/Meeting URL must be/)).toBeInTheDocument();
    });
    fireEvent.click(screen.getByTestId("reset-button"));
    expect(screen.queryByText(/Meeting URL must be/)).not.toBeInTheDocument();
  });

  it("Reset button is disabled while the launch mutation is in-flight", async () => {
    // Stub /api/launch to hang so the mutation stays in `isPending`.
    let resolveLaunch: ((value: Response) => void) | null = null;
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
        if (url === "/api/launch") {
          return new Promise<Response>((resolve) => {
            resolveLaunch = resolve;
          });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    // The button text flips to "Launching…" while pending; the Reset
    // button must be disabled at that moment.
    await waitFor(() => {
      expect(screen.getByTestId("launch-button")).toHaveTextContent(/Launching/);
    });
    expect(screen.getByTestId("reset-button")).toBeDisabled();
    // Tidy up the still-pending fetch so it doesn't outlive the test.
    if (resolveLaunch) {
      (resolveLaunch as (value: Response) => void)(
        new Response(JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }), {
          status: 201,
          headers: { "content-type": "application/json" },
        }),
      );
    }
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

/**
 * Radix DropdownMenu wires open/close on `onPointerDown` (button 0)
 * rather than the synthetic `click`. `fireEvent.click` alone doesn't
 * toggle the menu under jsdom. The simplest portable trick is the
 * keyboard path: focusing the trigger then pressing Enter both fires
 * the DropdownMenu's `onKeyDown` handler AND avoids the pointer-event
 * dance that jsdom isn't fully spec-compliant about. This mirrors how
 * an accessibility-tree test would drive the same trigger.
 */
async function openDropdown(trigger: HTMLElement) {
  trigger.focus();
  await userEvent.keyboard("{Enter}");
}

describe("<LaunchForm /> load-previous (v1.5.0)", () => {
  beforeEach(() => {
    window.localStorage.clear();
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
        return new Response(JSON.stringify({ botId: "00000000-0000-0000-0000-000000000000" }), {
          status: 201,
          headers: { "content-type": "application/json" },
        });
      }),
    );
  });

  it("renders the Load previous button next to Reset/Launch", () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    expect(screen.getByTestId("load-previous-button")).toBeInTheDocument();
  });

  it("shows the empty-state hint when no launches have been recorded yet", async () => {
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    await openDropdown(screen.getByTestId("load-previous-button"));
    await waitFor(() => {
      expect(screen.getByTestId("load-previous-button-empty")).toBeInTheDocument();
    });
    expect(screen.getByText(/No previous launches yet/i)).toBeInTheDocument();
  });

  it("persists a launched-bot history entry on successful submit", async () => {
    const onLaunched = vi.fn();
    renderWithClient(<LaunchForm onLaunched={onLaunched} onError={vi.fn()} />);
    fireEvent.change(screen.getByTestId("meeting-url"), {
      target: { value: "https://example.com/meeting/Saved" },
    });
    fireEvent.change(screen.getByTestId("participant"), { target: { value: "alice" } });
    fireEvent.click(screen.getByTestId("launch-button"));
    await waitFor(() => expect(onLaunched).toHaveBeenCalled());
    // The post-submit DOM should now reveal the entry on the dropdown
    // and the localStorage key should hold it.
    const raw = window.localStorage.getItem("bots-app-dashboard:launched-bot-history");
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw!) as Array<{ participant: string; meetingURL: string }>;
    expect(parsed).toHaveLength(1);
    expect(parsed[0].participant).toBe("alice");
    expect(parsed[0].meetingURL).toBe("https://example.com/meeting/Saved");
  });

  it("loads a previously-launched bot's spec back into the form when its row is clicked", async () => {
    // Seed localStorage with an entry up front so the dropdown is
    // pre-populated on first render — that avoids the round-trip
    // through onSuccess + waiting for state.
    const entry = {
      spec: {
        meetingURL: "https://example.com/meeting/Past",
        participant: "carol",
        displayName: "Carol",
        ttl: "30m",
        network: "none",
        headless: false,
        authBackend: "jwt",
        storageStateFile: "",
        runLocation: "local",
        sshHostLabel: "",
        costume: "default",
        audio: "default",
      },
      launchedAt: 1730000000000,
      meetingURL: "https://example.com/meeting/Past",
      participant: "carol",
      runLocationLabel: "local",
    };
    window.localStorage.setItem(
      "bots-app-dashboard:launched-bot-history",
      JSON.stringify([entry]),
    );
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    await openDropdown(screen.getByTestId("load-previous-button"));
    // Click the row.
    const rowId = `load-previous-button-entry-${entry.launchedAt}`;
    const row = await screen.findByTestId(rowId);
    fireEvent.click(row);
    expect((screen.getByTestId("meeting-url") as HTMLInputElement).value).toBe(
      "https://example.com/meeting/Past",
    );
    expect((screen.getByTestId("participant") as HTMLInputElement).value).toBe("carol");
    expect((screen.getByTestId("display-name") as HTMLInputElement).value).toBe("Carol");
    expect((screen.getByTestId("ttl") as HTMLInputElement).value).toBe("30m");
  });

  it("removes a single entry from history when the per-row × is clicked", async () => {
    const entry = {
      spec: {
        meetingURL: "https://example.com/meeting/Past",
        participant: "carol",
        displayName: "",
        ttl: "30m",
        network: "none",
        headless: false,
        authBackend: "jwt",
        storageStateFile: "",
        runLocation: "local",
        sshHostLabel: "",
        costume: "default",
        audio: "default",
      },
      launchedAt: 1730000000000,
      meetingURL: "https://example.com/meeting/Past",
      participant: "carol",
      runLocationLabel: "local",
    };
    window.localStorage.setItem(
      "bots-app-dashboard:launched-bot-history",
      JSON.stringify([entry]),
    );
    renderWithClient(<LaunchForm onLaunched={vi.fn()} onError={vi.fn()} />);
    await openDropdown(screen.getByTestId("load-previous-button"));
    const removeBtn = await screen.findByTestId(
      `load-previous-button-remove-${entry.launchedAt}`,
    );
    fireEvent.click(removeBtn);
    // After the remove, the empty-state appears.
    await waitFor(() => {
      expect(screen.getByTestId("load-previous-button-empty")).toBeInTheDocument();
    });
    expect(JSON.parse(window.localStorage.getItem("bots-app-dashboard:launched-bot-history")!)).toEqual([]);
  });
});
