import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
  /**
   * Optional override for the `/api/sso/status` response. When unset
   * the stub replies with `exists: false` (the safe default that
   * mirrors a fresh worktree where the operator hasn't captured the
   * HCL SSO state yet). Tests that exercise the SSO wire-through pass
   * `{ exists: true, filePath, ... }` here.
   */
  ssoStatusResponse?: {
    filePath: string;
    exists: boolean;
    capturedAt: number | null;
    ageHours: number | null;
    size: number | null;
  };
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
      // The form's `ssoStatusQuery` (added v1.8.2) fires on every
      // render regardless of auth backend. Stub a default "no captured
      // SSO state" reply so tests that don't care about SSO don't see
      // an unhandled-fetch warning in the console.
      if (url === "/api/sso/status") {
        const body = state.ssoStatusResponse ?? {
          filePath: "/runDir/auth/hcl-sso.json",
          exists: false,
          capturedAt: null,
          ageHours: null,
          size: null,
        };
        return new Response(JSON.stringify(body), {
          status: 200,
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

  it("retains all field values after a successful launch (no auto-reset)", async () => {
    // v1.4.1 parity with LaunchForm: an operator who just spawned N
    // bots almost always wants to spawn another batch with the same
    // shared config. The fields must keep their values after success.
    const onLaunched = vi.fn();
    renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
    const meetingUrl = "https://app.videocall.fnxlabs.com/meeting/Foo";
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: meetingUrl },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "5" } });
    fireEvent.change(screen.getByTestId("multi-ttl"), { target: { value: "15m" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(onLaunched).toHaveBeenCalled();
    });
    expect((screen.getByTestId("multi-meeting-url") as HTMLInputElement).value).toBe(meetingUrl);
    expect((screen.getByTestId("multi-count") as HTMLInputElement).value).toBe("5");
    expect((screen.getByTestId("multi-ttl") as HTMLInputElement).value).toBe("15m");
  });

  it("retains all field values after a failed launch", async () => {
    state.multiLaunchResponse = { status: 500, body: { error: "boom" } };
    const onError = vi.fn();
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={onError} />);
    const meetingUrl = "https://app.videocall.fnxlabs.com/meeting/Foo";
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: meetingUrl },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "4" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(onError).toHaveBeenCalled();
    });
    expect((screen.getByTestId("multi-meeting-url") as HTMLInputElement).value).toBe(meetingUrl);
    expect((screen.getByTestId("multi-count") as HTMLInputElement).value).toBe("4");
  });

  it("renders a Reset button that clears every field to the initial-render state", () => {
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Capture the initial defaults the user sees on first paint.
    const initialMeetingUrl = (screen.getByTestId("multi-meeting-url") as HTMLInputElement).value;
    const initialCount = (screen.getByTestId("multi-count") as HTMLInputElement).value;
    const initialTtl = (screen.getByTestId("multi-ttl") as HTMLInputElement).value;
    // Mutate the fields.
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://example.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "7" } });
    fireEvent.change(screen.getByTestId("multi-ttl"), { target: { value: "1h" } });
    fireEvent.click(screen.getByTestId("multi-display-name-template"));
    fireEvent.change(screen.getByTestId("multi-display-name-template"), {
      target: { value: "Bot {participant}" },
    });
    // Click Reset.
    const resetBtn = screen.getByTestId("multi-reset-button");
    expect(resetBtn).toBeInTheDocument();
    fireEvent.click(resetBtn);
    // Every field must be back to its initial-render value.
    expect((screen.getByTestId("multi-meeting-url") as HTMLInputElement).value).toBe(
      initialMeetingUrl,
    );
    expect((screen.getByTestId("multi-count") as HTMLInputElement).value).toBe(initialCount);
    expect((screen.getByTestId("multi-ttl") as HTMLInputElement).value).toBe(initialTtl);
    expect((screen.getByTestId("multi-display-name-template") as HTMLInputElement).value).toBe("");
  });

  it("Reset clears validation errors", async () => {
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Submit with the default blank meeting URL to trigger validation.
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(screen.getByRole("alert")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByTestId("multi-reset-button"));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("renders the Load previous button next to Reset/Launch (v1.5.0)", () => {
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    expect(screen.getByTestId("multi-load-previous-button")).toBeInTheDocument();
  });

  it("persists a launched-bot history entry on successful multi-submit (v1.5.0)", async () => {
    window.localStorage.clear();
    const onLaunched = vi.fn();
    renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://example.com/meeting/MultiSave" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "4" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => expect(onLaunched).toHaveBeenCalled());
    const raw = window.localStorage.getItem("bots-app-dashboard:launched-bot-history");
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw!) as Array<{ participant: string; meetingURL: string }>;
    expect(parsed).toHaveLength(1);
    expect(parsed[0].meetingURL).toBe("https://example.com/meeting/MultiSave");
    // The synthetic participant label encodes the batch shape.
    expect(parsed[0].participant).toMatch(/^multi:first-n-4$/);
    window.localStorage.clear();
  });

  it("loads only common fields when a previous launch is picked (v1.5.0)", async () => {
    // Seed localStorage with a single-bot entry; loading it from
    // multi-launch must apply meetingURL/ttl/network/headless/etc.
    // but leave count, mode, seed, displayNameTemplate alone.
    window.localStorage.clear();
    const entry = {
      spec: {
        meetingURL: "https://example.com/meeting/Past",
        participant: "carol",
        displayName: "Carol",
        ttl: "30m",
        network: "none",
        headless: true,
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
    window.localStorage.setItem("bots-app-dashboard:launched-bot-history", JSON.stringify([entry]));
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Capture the multi-specific defaults BEFORE the click so we can
    // assert they didn't change.
    const initialCount = (screen.getByTestId("multi-count") as HTMLInputElement).value;
    // Radix DropdownMenu listens to pointerDown (button 0) — clicks
    // alone don't open it under jsdom. Use the keyboard path instead:
    // focus the trigger and press Enter, which fires the menu's
    // onKeyDown handler. (Mirrors the openDropdown helper in
    // LaunchForm.test.tsx.)
    const trigger = screen.getByTestId("multi-load-previous-button");
    trigger.focus();
    await userEvent.keyboard("{Enter}");
    const row = await screen.findByTestId(`multi-load-previous-button-entry-${entry.launchedAt}`);
    fireEvent.click(row);
    // Common fields applied.
    expect((screen.getByTestId("multi-meeting-url") as HTMLInputElement).value).toBe(
      "https://example.com/meeting/Past",
    );
    expect((screen.getByTestId("multi-ttl") as HTMLInputElement).value).toBe("30m");
    // Headless got mirrored ON from the snapshot.
    expect(screen.getByTestId("multi-headless")).toHaveAttribute("data-state", "checked");
    // Multi-specific knob untouched: count stays at its initial value.
    expect((screen.getByTestId("multi-count") as HTMLInputElement).value).toBe(initialCount);
    window.localStorage.clear();
  });

  it("defaults the spawn-delay field to 2 seconds (v1.7.5)", () => {
    // The dashboard's "Delay between launches (seconds)" knob (added
    // v1.7.5) must render with a default of 2 so a fresh form click
    // produces the staggered behavior the operator now expects without
    // any manual tweaking. Changing this constant requires updating
    // bots-app/dashboard release notes — operators rely on the default.
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    const field = screen.getByTestId("multi-spawn-delay-seconds") as HTMLInputElement;
    expect(field).toBeInTheDocument();
    expect(field.value).toBe("2");
  });

  it("submits the spawn-delay value on the multi-launch request (v1.7.5)", async () => {
    // The default value of 2 must reach the server unchanged; this
    // pins the client-side wire format so a future refactor that
    // accidentally drops the field is caught immediately.
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.spawnDelaySeconds).toBe(2);
  });

  it("propagates an edited spawn-delay value (v1.7.5)", async () => {
    // Mutating the field must flow through to the request body. This
    // is the smoking gun for the un-fixed code path: if the wiring
    // breaks (form value not threaded into `req`), the request still
    // succeeds with the default 2 and a user-set "5" never reaches
    // the server.
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "3" } });
    fireEvent.change(screen.getByTestId("multi-spawn-delay-seconds"), {
      target: { value: "5" },
    });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.spawnDelaySeconds).toBe(5);
  });

  it("accepts spawn-delay of 0 (no artificial wait) and sends it explicitly (v1.7.5)", async () => {
    // Setting the delay to 0 must be a *valid* submission (not a
    // validation error) and must land on the wire as 0 — server
    // treats 0 as the legacy "fire back-to-back" path. We assert on
    // the explicit 0 rather than `undefined` so a future change that
    // drops the field on 0 (treating it as "omit") is caught.
    const onError = vi.fn();
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={onError} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.change(screen.getByTestId("multi-spawn-delay-seconds"), {
      target: { value: "0" },
    });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.spawnDelaySeconds).toBe(0);
    expect(onError).not.toHaveBeenCalled();
  });

  it("rejects an out-of-range spawn-delay with a validation error (v1.7.5)", async () => {
    // The dashboard caps the knob at 60s to match the server's
    // accepted range; submitting a higher value must fail client-side
    // with a visible alert and never hit `/api/launch/multi`.
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-spawn-delay-seconds"), {
      target: { value: "999" },
    });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(screen.getByRole("alert")).toBeInTheDocument();
    });
    expect(state.multiLaunchCalls).toHaveLength(0);
  });

  // --- SSO state wire-through (v1.8.2) -------------------------------
  //
  // The single-launch form gained an SSO state wire-through in v1.5.0
  // so dashboard-spawned JWT bots pick up the captured
  // `<runDir>/auth/hcl-sso.json` without the operator having to
  // re-capture per launch. Multi-launch was missing the same hook —
  // batches spawned via this form ignored the captured SSO state and
  // their browsers hit the HCL SSO portal on every page-load. v1.8.2
  // ports the same pattern into MultiLaunchForm; the two tests below
  // pin the wire shape so a future refactor that accidentally drops
  // the field is caught immediately.

  it("forwards ssoStateFile in the multi-launch payload when JWT + SSO file exists (v1.8.2)", async () => {
    state.ssoStatusResponse = {
      filePath: "/run/auth/hcl-sso.json",
      exists: true,
      capturedAt: Date.now(),
      ageHours: 1.25,
      size: 3210,
    };
    stubFetch(state);
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Wait for the SSO status query to settle before clicking Launch
    // so the submit handler sees `ssoStatusQuery.data.exists === true`.
    await waitFor(() => {
      const calls = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls;
      expect(calls.some((c) => c[0] === "/api/sso/status")).toBe(true);
    });
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.ssoStateFile).toBe("/run/auth/hcl-sso.json");
  });

  it("omits ssoStateFile from the multi-launch payload when auth is not JWT (v1.8.2)", async () => {
    // Mirror of LaunchForm.test.tsx's "omits ssoStateFile when auth is
    // not JWT" guard. Even when the SSO file exists, switching the
    // batch to Guest auth must drop the field — otherwise we'd be
    // pushing the SSO state to bots that don't need it (and that the
    // server would pre-load uselessly into every spawned context).
    state.ssoStatusResponse = {
      filePath: "/run/auth/hcl-sso.json",
      exists: true,
      capturedAt: Date.now(),
      ageHours: 1,
      size: 1024,
    };
    stubFetch(state);
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    // Switch auth to Guest (none). Radix RadioGroup items are
    // accessible by their generated id (`multi-auth-none`).
    const guestRadio = document.getElementById("multi-auth-none") as HTMLElement;
    fireEvent.click(guestRadio);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://example.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.ssoStateFile).toBeUndefined();
  });

  it("omits ssoStateFile from the multi-launch payload when no SSO file is captured (v1.8.2)", async () => {
    // Default ssoStatusResponse is `exists: false`. A JWT batch under
    // that condition must still submit cleanly with the field
    // *absent* (not `null`, not the empty string) — the server's
    // validator rejects non-string `ssoStateFile`, and a no-SSO env
    // (fresh checkout, expired capture) is the steady-state for
    // first-run operators.
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://example.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(state.multiLaunchCalls).toHaveLength(1);
    });
    const sent = state.multiLaunchCalls[0] as Record<string, unknown>;
    expect(sent.ssoStateFile).toBeUndefined();
  });

  it("Reset button is disabled while the launch mutation is in-flight", async () => {
    // Replace the fetch stub with one whose /api/launch/multi handler
    // hangs so the mutation stays in `isPending`.
    let resolveLaunch: ((value: Response) => void) | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (url: string) => {
        if (url === "/api/launch/multi") {
          return new Promise<Response>((resolve) => {
            resolveLaunch = resolve;
          });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
    fireEvent.change(screen.getByTestId("multi-meeting-url"), {
      target: { value: "https://app.videocall.fnxlabs.com/meeting/X" },
    });
    fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
    fireEvent.click(screen.getByTestId("multi-launch-button"));
    await waitFor(() => {
      expect(screen.getByTestId("multi-launch-button")).toHaveTextContent(/Launching/);
    });
    expect(screen.getByTestId("multi-reset-button")).toBeDisabled();
    if (resolveLaunch) {
      (resolveLaunch as (value: Response) => void)(
        new Response(
          JSON.stringify({
            mode: "first-n",
            count: 2,
            seed: null,
            participants: ["alice", "bob"],
            botIds: ["bot-1", "bot-2"],
            errors: [],
          }),
          { status: 202, headers: { "content-type": "application/json" } },
        ),
      );
    }
  });

  // --- Per-field history (v1.8.1) ------------------------------------
  //
  // MultiLaunchForm's free-text inputs must mirror the single-bot
  // LaunchForm's per-field-history behavior so an operator who has
  // typed a meeting URL once doesn't have to re-type it the next time
  // they spawn a batch. Storage keys for the three *shared-semantics*
  // fields (`meetingURL`, `ttl`, `storageStateFile`) are intentionally
  // identical to LaunchForm's so suggestions cross-pollinate between
  // the two forms. The two multi-specific fields (`displayNameTemplate`
  // and `seed`) get their own buckets because their values are not
  // interchangeable with the single-bot equivalents.
  describe("per-field history (v1.8.1)", () => {
    function historyKey(field: string): string {
      return `bots-app-dashboard:history:${field}`;
    }
    function seedHistory(field: string, values: string[]): void {
      const entries = values.map((value, i) => ({ value, lastUsed: 1000 + i }));
      entries.sort((a, b) => b.lastUsed - a.lastUsed);
      window.localStorage.setItem(historyKey(field), JSON.stringify(entries));
    }
    function readHistoryValues(field: string): string[] {
      const raw = window.localStorage.getItem(historyKey(field));
      if (!raw) return [];
      const parsed = JSON.parse(raw) as Array<{ value: string }>;
      return parsed.map((e) => e.value);
    }

    beforeEach(() => {
      window.localStorage.clear();
    });

    it("shows a pre-seeded meetingURL suggestion on focus", async () => {
      // Pre-populate the *shared* meetingURL bucket — same key used by
      // LaunchForm so launching one bot with a URL elsewhere makes it
      // available here. The popover should open on focus and the
      // seeded value should appear in the suggestion list.
      seedHistory("meetingURL", ["https://example.com/meeting/Seeded"]);
      renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
      const input = screen.getByTestId("multi-meeting-url");
      fireEvent.focus(input);
      await waitFor(() =>
        expect(screen.getByTestId("multi-meeting-url-history")).toBeInTheDocument(),
      );
      expect(screen.getByText("https://example.com/meeting/Seeded")).toBeInTheDocument();
    });

    it("pushes meetingURL + ttl to history on successful submit", async () => {
      const onLaunched = vi.fn();
      renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
      fireEvent.change(screen.getByTestId("multi-meeting-url"), {
        target: { value: "https://example.com/meeting/Pushed" },
      });
      fireEvent.change(screen.getByTestId("multi-ttl"), { target: { value: "15m" } });
      fireEvent.change(screen.getByTestId("multi-count"), { target: { value: "2" } });
      fireEvent.click(screen.getByTestId("multi-launch-button"));
      await waitFor(() => expect(onLaunched).toHaveBeenCalled());
      // Both required free-text fields land in their shared buckets.
      expect(readHistoryValues("meetingURL")).toContain("https://example.com/meeting/Pushed");
      expect(readHistoryValues("ttl")).toContain("15m");
    });

    it("does NOT push to history when validation fails", async () => {
      // Submit with no meeting URL — validator must fire, request
      // must not go out, and nothing should land in history. This
      // pins the convention that history mirrors *server-confirmed*
      // submissions only, not failed attempts.
      renderWithClient(<MultiLaunchForm onLaunched={() => {}} onError={() => {}} />);
      // Mutate ttl to a unique value so we can prove it wasn't pushed.
      fireEvent.change(screen.getByTestId("multi-ttl"), { target: { value: "999h" } });
      fireEvent.click(screen.getByTestId("multi-launch-button"));
      await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
      expect(state.multiLaunchCalls).toHaveLength(0);
      expect(readHistoryValues("ttl")).toEqual([]);
      expect(readHistoryValues("meetingURL")).toEqual([]);
    });

    it("pushes displayNameTemplate to its own bucket (not displayName)", async () => {
      // Template semantics differ from single-bot display names, so
      // they MUST live in a dedicated bucket. Asserting on both keys
      // catches a future refactor that accidentally aliases them.
      const onLaunched = vi.fn();
      renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
      fireEvent.change(screen.getByTestId("multi-meeting-url"), {
        target: { value: "https://example.com/meeting/X" },
      });
      fireEvent.change(screen.getByTestId("multi-display-name-template"), {
        target: { value: "Bot {participant}" },
      });
      fireEvent.click(screen.getByTestId("multi-launch-button"));
      await waitFor(() => expect(onLaunched).toHaveBeenCalled());
      expect(readHistoryValues("displayNameTemplate")).toContain("Bot {participant}");
      // The single-bot `displayName` bucket must remain untouched.
      expect(readHistoryValues("displayName")).toEqual([]);
    });

    it("pushes seed to its own bucket only when mode=random and value non-empty", async () => {
      // Seeds are only meaningful in random mode — pushing one from
      // first-n would surface a number that the operator can't even
      // see in the UI (the seed field is hidden). Wire the push
      // accordingly and pin both halves of the contract here.
      const onLaunched = vi.fn();
      renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
      // Switch to random mode so the seed input is rendered.
      fireEvent.click(screen.getByLabelText(/Random N/i));
      fireEvent.change(screen.getByTestId("multi-meeting-url"), {
        target: { value: "https://example.com/meeting/X" },
      });
      fireEvent.change(screen.getByTestId("multi-seed"), { target: { value: "1729" } });
      fireEvent.click(screen.getByTestId("multi-launch-button"));
      await waitFor(() => expect(onLaunched).toHaveBeenCalled());
      expect(readHistoryValues("seed")).toEqual(["1729"]);
      // None of the shared-key buckets should pick up the seed string.
      expect(readHistoryValues("displayName")).toEqual([]);
      expect(readHistoryValues("participant")).toEqual([]);
    });

    it("does NOT push an empty optional field (storageStateFile) on success", async () => {
      // Optional fields left blank must stay out of history so the
      // suggestion list never surfaces an empty row. The auth backend
      // stays at the default `jwt` here so the storage-state input
      // isn't even rendered — but the push guard must hold either way.
      const onLaunched = vi.fn();
      renderWithClient(<MultiLaunchForm onLaunched={onLaunched} onError={() => {}} />);
      fireEvent.change(screen.getByTestId("multi-meeting-url"), {
        target: { value: "https://example.com/meeting/X" },
      });
      fireEvent.click(screen.getByTestId("multi-launch-button"));
      await waitFor(() => expect(onLaunched).toHaveBeenCalled());
      expect(readHistoryValues("storageStateFile")).toEqual([]);
    });
  });
});
