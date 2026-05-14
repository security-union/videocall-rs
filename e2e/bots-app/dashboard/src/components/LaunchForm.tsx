import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import * as RadioGroup from "@radix-ui/react-radio-group";
import * as Switch from "@radix-ui/react-switch";
import * as Tooltip from "@radix-ui/react-tooltip";
import { Rocket, Wand2 } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type {
  AssetsManifestParticipant,
  AssetsManifestResponse,
  LaunchRequest,
  SsoStatusResponse,
} from "../api/types";
import {
  AUTH_BACKENDS,
  NETSIM_PRESETS,
  RUN_LOCATIONS,
  TTL_SUGGESTIONS,
  type AuthBackend,
  type RunLocation,
} from "../lib/constants";
import { useFieldHistory } from "../lib/fieldHistory";
import { type FieldErrors, validateLaunchForm } from "../lib/validation";
import { HelpPopover } from "./ui/HelpPopover";
import { HistoryInput } from "./ui/HistoryInput";
import { Select } from "./ui/Select";
import { SshCommandPreview } from "./SshCommandPreview";
import { SsoPanel } from "./SsoPanel";

export interface LaunchFormInitial {
  meetingURL: string;
  participant: string;
  displayName: string;
  ttl: string;
  network: string;
  headless: boolean;
  authBackend: AuthBackend;
  storageStateFile: string;
  runLocation: RunLocation;
  /**
   * Label of the registered SSH host this bot will launch on. Ignored
   * unless `runLocation === "ssh"`. Empty string means "no pick yet".
   */
  sshHostLabel: string;
  costume: string;
  audio: string;
}

interface LaunchFormProps {
  initialValues?: LaunchFormInitial;
  onLaunched: (botId: string) => void;
  onError: (message: string) => void;
}

const DEFAULT_VALUES: LaunchFormInitial = {
  meetingURL: "",
  participant: "",
  displayName: "",
  ttl: "5m",
  network: "none",
  headless: false,
  authBackend: "jwt",
  storageStateFile: "",
  runLocation: "local",
  sshHostLabel: "",
  costume: "default",
  audio: "default",
};

/**
 * Sentinel value the Costume + Audio Selects use for "fall back to
 * Chrome's default fake pattern". The auto-match logic only overrides
 * a field whose current value equals this — it never clobbers an
 * explicit operator selection.
 */
const DEFAULT_COSTUME = "default";
const DEFAULT_AUDIO = "default";

/**
 * Delay between the operator's last keystroke in the Participant field
 * and the auto-match check. Long enough that a fast typer doesn't see
 * the costume/audio fields flicker through partial matches; short
 * enough that the auto-default feels instant once they pause.
 */
const PARTICIPANT_DEBOUNCE_MS = 250;

/**
 * Look up a manifest participant by case-insensitive trimmed name
 * match. The launch form normalises both the typed input and the
 * manifest's `name` before comparing so "Alice", "alice ", and "ALICE"
 * all resolve to the same row. Returns `null` when no row matches.
 */
function findManifestParticipant(
  manifest: AssetsManifestResponse | undefined,
  name: string,
): AssetsManifestParticipant | null {
  if (!manifest || !Array.isArray(manifest.participants)) return null;
  const needle = name.trim().toLowerCase();
  if (needle === "") return null;
  return (
    manifest.participants.find((p) => p.name.trim().toLowerCase() === needle) ?? null
  );
}

const INPUT_CLASS =
  "w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm placeholder:text-neutral-400 focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 disabled:cursor-not-allowed disabled:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-100 dark:placeholder:text-slate-500 dark:focus:border-sky-400 dark:focus:ring-sky-400 dark:disabled:bg-slate-900";

export function LaunchForm({ initialValues, onLaunched, onError }: LaunchFormProps) {
  const [values, setValues] = useState<LaunchFormInitial>(initialValues ?? DEFAULT_VALUES);
  const [errors, setErrors] = useState<FieldErrors>({});
  const [submitted, setSubmitted] = useState(false);
  const [ssoPanelOpen, setSsoPanelOpen] = useState(false);
  // Per-field flags toggled by manual edits to the Costume or Audio
  // dropdowns. Once flipped, the manifest auto-match logic stops
  // touching that field until the form is reset via `initialValues`.
  // Tracked separately from `values` so the auto-match useEffect can
  // distinguish "operator chose this exact filename" from "we
  // happened to set this exact filename for them".
  const [manualTouched, setManualTouched] = useState<{ costume: boolean; audio: boolean }>({
    costume: false,
    audio: false,
  });
  // True for one "lifetime" — the window between `initialValues`
  // changing (a duplicate pre-fill) and the operator's first edit.
  // While set, the manifest auto-match logic is suppressed so the
  // duplicated bot's explicit settings win. Cleared on any setField
  // call (including a Participant edit).
  const [freshlyDuplicated, setFreshlyDuplicated] = useState<boolean>(initialValues !== undefined);

  // Surface the captured SSO state in the Identity section when the
  // operator picks JWT auth (which is the path that consumes it). The
  // query shares its cache with the header chip + SsoPanel, so this
  // does NOT add a duplicate network call.
  const ssoStatusQuery = useQuery({
    queryKey: ["sso", "status"],
    queryFn: api.ssoStatus,
    refetchInterval: 60_000,
  });

  // Fetch the registered SSH hosts so the "Run location: SSH-able
  // host" option can populate its sub-Select. Refetch every 60s so
  // additions from the Tools page show up without a manual reload.
  const hostsQuery = useQuery({
    queryKey: ["ssh", "hosts"],
    queryFn: api.listHosts,
    refetchInterval: 60_000,
  });

  // Per-field history controllers. Each gets its own localStorage
  // namespace via the key passed to `useFieldHistory`. On successful
  // submit we push each field's value to its history.
  const meetingUrlHistory = useFieldHistory("meetingURL");
  const participantHistory = useFieldHistory("participant");
  const displayNameHistory = useFieldHistory("displayName");
  const ttlHistory = useFieldHistory("ttl");
  const storageStateHistory = useFieldHistory("storageStateFile");

  useEffect(() => {
    if (initialValues) {
      setValues(initialValues);
      setErrors({});
      setSubmitted(false);
      // The duplicate's existing values must win over manifest
      // auto-match until the operator makes their first edit. Reset
      // the per-field "manual touch" tracking too so future edits
      // get a clean slate.
      setManualTouched({ costume: false, audio: false });
      setFreshlyDuplicated(true);
    }
  }, [initialValues]);

  const costumesQuery = useQuery({
    queryKey: ["assets", "costumes"],
    queryFn: () =>
      fetch("/api/assets/costumes")
        .then((r) => r.json() as Promise<{ files: string[] }>)
        .then((j) => j.files ?? []),
  });
  const audioQuery = useQuery({
    queryKey: ["assets", "audio"],
    queryFn: () =>
      fetch("/api/assets/audio")
        .then((r) => r.json() as Promise<{ files: string[] }>)
        .then((j) => j.files ?? []),
  });
  // Participant → costume / audio mapping. 60s refetch matches the
  // other asset endpoints — the manifest is sticky during a dashboard
  // session (operators rerun prep-assets out-of-band).
  const assetsManifestQuery = useQuery({
    queryKey: ["assets", "manifest"],
    queryFn: api.assetsManifest,
    refetchInterval: 60_000,
  });

  // Debounced manifest auto-match. When the Participant input matches
  // a manifest row AND the corresponding Costume / Audio field is
  // still at its sentinel default AND the operator hasn't manually
  // pinned that field AND we're not in "freshly duplicated" mode,
  // populate the field with the manifest's match. Re-typing the same
  // participant is idempotent; switching to a participant with no
  // manifest entry is a no-op (the existing value remains).
  //
  // The 250ms debounce keeps the costume/audio dropdowns from
  // flickering through partial matches while the operator is mid-type
  // (e.g. "ali" → "alic" → "alice" should only fire one match).
  const manifestMatch = useMemo(
    () => findManifestParticipant(assetsManifestQuery.data, values.participant),
    [assetsManifestQuery.data, values.participant],
  );
  const debounceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (freshlyDuplicated) return;
    if (manifestMatch === null) return;
    // Stash a copy of the current per-field state so the timer
    // callback runs against the snapshot it was scheduled with.
    const m = manifestMatch;
    if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);
    debounceTimerRef.current = setTimeout(() => {
      setValues((prev) => {
        const next = { ...prev };
        if (!manualTouched.costume && prev.costume === DEFAULT_COSTUME && m.costumeFile) {
          next.costume = m.costumeFile;
        }
        if (!manualTouched.audio && prev.audio === DEFAULT_AUDIO && m.audioFile) {
          next.audio = m.audioFile;
        }
        return next;
      });
    }, PARTICIPANT_DEBOUNCE_MS);
    return () => {
      if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);
    };
  }, [manifestMatch, manualTouched.costume, manualTouched.audio, freshlyDuplicated]);

  // Per-render derivation: is the current Costume / Audio value the
  // exact manifest default for the current participant? Drives the
  // small "Auto-matched from manifest" badge next to each Select.
  // Falls back to false when no participant matches — the badge stays
  // hidden in that case.
  const costumeIsAutoMatched =
    manifestMatch !== null &&
    manifestMatch.costumeFile !== null &&
    values.costume === manifestMatch.costumeFile;
  const audioIsAutoMatched =
    manifestMatch !== null &&
    manifestMatch.audioFile !== null &&
    values.audio === manifestMatch.audioFile;

  const launchMutation = useMutation({
    mutationFn: (req: LaunchRequest) => api.launch(req),
    onSuccess: (data, variables) => {
      // Persist the submitted values into each field's history.
      // Display name + storage-state are optional; push only when
      // non-empty so we don't poison the history list with blanks.
      meetingUrlHistory.push(variables.meetingURL);
      participantHistory.push(variables.participant);
      if (values.displayName.trim()) displayNameHistory.push(values.displayName);
      ttlHistory.push(variables.ttl);
      if (variables.storageStateFile) storageStateHistory.push(variables.storageStateFile);
      onLaunched(data.botId);
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onError(msg);
    },
  });

  const setField = <K extends keyof LaunchFormInitial>(key: K, val: LaunchFormInitial[K]) => {
    setValues((prev) => ({ ...prev, [key]: val }));
    if (submitted) {
      setErrors(validateLaunchForm({ ...values, [key]: val }));
    }
    // Any explicit user edit clears the "freshly duplicated" guard so
    // subsequent participant edits resume normal auto-match behavior.
    if (freshlyDuplicated) setFreshlyDuplicated(false);
    // Explicit picks of Costume / Audio are sticky — the manifest
    // auto-match must never overwrite the operator's choice from
    // here on (until `initialValues` resets the form).
    if (key === "costume") setManualTouched((m) => ({ ...m, costume: true }));
    if (key === "audio") setManualTouched((m) => ({ ...m, audio: true }));
  };

  const handleSubmit: React.FormEventHandler<HTMLFormElement> = (e) => {
    e.preventDefault();
    setSubmitted(true);
    const v = validateLaunchForm(values);
    setErrors(v);
    if (Object.keys(v).length > 0) return;

    // When the operator is using JWT auth and we have a captured SSO
    // state file on disk, forward its path so the orchestrator
    // pre-loads its cookies into the bot's BrowserContext. This is
    // exactly the wire-through the launch form was missing — without
    // it dashboard-spawned bots would ignore `run/auth/hcl-sso.json`
    // even though the file existed.
    const ssoStateFile =
      values.authBackend === "jwt" && ssoStatusQuery.data?.exists
        ? ssoStatusQuery.data.filePath
        : undefined;

    // Costume / audio are pipe-through fields: the orchestrator's
    // `/launch` endpoint validates the basename against
    // `ASSET_FILENAME_PATTERN` and rejects any path-like value. The
    // sentinel `"default"` (and the empty string) collapse to
    // `undefined` so the server doesn't run the basename regex on a
    // non-file value.
    const costume =
      values.costume && values.costume !== "default" ? values.costume : undefined;
    const audio = values.audio && values.audio !== "default" ? values.audio : undefined;

    // Translate the radio + sub-Select into the structured wire
    // shape the Node sidecar expects (`{ kind: "local" }` or
    // `{ kind: "ssh", hostLabel }`). Anything other than "local" or
    // "ssh" is blocked by the client-side validator above, so we
    // narrow defensively.
    let runLocation: LaunchRequest["runLocation"];
    if (values.runLocation === "ssh") {
      runLocation = { kind: "ssh", hostLabel: values.sshHostLabel.trim() };
    } else {
      runLocation = { kind: "local" };
    }

    const req: LaunchRequest = {
      meetingURL: values.meetingURL.trim(),
      participant: values.participant.trim(),
      displayName: values.displayName.trim() || undefined,
      ttl: values.ttl.trim(),
      network: values.network,
      headless: values.headless,
      authBackend: values.authBackend,
      storageStateFile:
        values.authBackend === "storage-state"
          ? values.storageStateFile.trim() || undefined
          : undefined,
      ssoStateFile,
      costume,
      audio,
      runLocation,
    };
    launchMutation.mutate(req);
  };

  return (
    <Tooltip.Provider>
      <form className="flex flex-col gap-5" onSubmit={handleSubmit} noValidate>
        {/* Section: Runtime — rendered first so the operator picks the
            run location before downstream choices (asset availability,
            network reachability, applicable auth flow) depend on it. */}
        <Section title="Runtime" description="Where the bot's Chrome runs.">
          <Field
            label="Run location"
            error={errors.runLocation}
            help={
              <HelpPopover fieldLabel="Run location" testId="help-runLocation">
                <p>Where the bot&apos;s Chrome runs.</p>
                <p className="mt-1">
                  Local and SSH are supported; Cloud VM and Docker slots remain disabled until
                  those backends ship.
                </p>
                <p className="mt-1">
                  SSH-able host requires at least one entry in the host registry (Tools page).
                  Asset prep + most ctl actions are not proxied for remote bots in v1 — see the
                  panel under the radio for the action matrix.
                </p>
              </HelpPopover>
            }
          >
            <RadioGroup.Root
              value={values.runLocation}
              onValueChange={(v) => setField("runLocation", v as RunLocation)}
              className="flex flex-col gap-2"
            >
              {RUN_LOCATIONS.map((loc) => {
                const sshUnavailable =
                  loc.value === "ssh" && (hostsQuery.data?.hosts?.length ?? 0) === 0;
                const itemDisabled = !loc.available || sshUnavailable;
                const tooltip = !loc.available
                  ? "Future feature — see discussion #793"
                  : sshUnavailable
                    ? "No hosts registered — add one in Tools"
                    : null;
                return (
                  <Tooltip.Root key={loc.value} delayDuration={150}>
                    <Tooltip.Trigger asChild>
                      <label
                        className={`flex items-center gap-2 text-sm ${
                          itemDisabled
                            ? "text-neutral-400 dark:text-slate-500"
                            : "text-neutral-700 dark:text-slate-200"
                        }`}
                        htmlFor={`runloc-${loc.value}`}
                        data-testid={`runloc-label-${loc.value}`}
                      >
                        <RadioGroup.Item
                          id={`runloc-${loc.value}`}
                          value={loc.value}
                          disabled={itemDisabled}
                          className="flex h-4 w-4 items-center justify-center rounded-full border border-neutral-300 bg-white data-[state=checked]:border-sky-500 disabled:bg-neutral-100 dark:border-slate-500 dark:bg-slate-800 dark:data-[state=checked]:border-sky-400 dark:disabled:bg-slate-900"
                        >
                          <RadioGroup.Indicator className="h-2 w-2 rounded-full bg-sky-500 dark:bg-sky-400" />
                        </RadioGroup.Item>
                        {loc.label}
                      </label>
                    </Tooltip.Trigger>
                    {tooltip !== null && (
                      <Tooltip.Portal>
                        <Tooltip.Content
                          side="right"
                          sideOffset={6}
                          className="z-50 rounded-md bg-neutral-900 px-2 py-1 text-xs text-white shadow-md dark:bg-slate-700 dark:text-slate-100"
                        >
                          {tooltip}
                          <Tooltip.Arrow className="fill-neutral-900 dark:fill-slate-700" />
                        </Tooltip.Content>
                      </Tooltip.Portal>
                    )}
                  </Tooltip.Root>
                );
              })}
            </RadioGroup.Root>
          </Field>

          {values.runLocation === "ssh" && (
            <Field
              label="SSH host"
              required
              error={errors.sshHostLabel}
              help={
                <HelpPopover fieldLabel="SSH host" testId="help-sshHostLabel">
                  <p>
                    Picks one of the hosts registered under Tools → Remote Hosts. The bot is
                    launched via{" "}
                    <code className="font-mono text-[11px]">ssh user@host &apos;npm run bot …&apos;</code>
                    .
                  </p>
                  <p className="mt-1">
                    v1 limitations: assets are NOT rsync&apos;d to the remote host (the bot
                    falls back to Chrome&apos;s default fake patterns unless you&apos;ve prep&apos;d
                    them out-of-band), and Mute / Camera / Share / Tune-network / Duplicate /
                    Extend-TTL are not proxied — Leave + Force-kill ARE wired (via SIGTERM /
                    SIGKILL over SSH).
                  </p>
                </HelpPopover>
              }
            >
              <Select
                value={values.sshHostLabel || "__none__"}
                onValueChange={(v) => setField("sshHostLabel", v === "__none__" ? "" : v)}
                options={[
                  { value: "__none__", label: "Pick a host…" },
                  ...(hostsQuery.data?.hosts ?? []).map((h: { label: string; user: string; host: string }) => ({
                    value: h.label,
                    label: `${h.label}  (${h.user}@${h.host})`,
                  })),
                ]}
                testId="ssh-host-select"
              />
              <p className="mt-1 text-xs text-neutral-500 dark:text-slate-400">
                Add or edit hosts under{" "}
                <code className="font-mono text-[11px]">Tools → Remote Hosts</code>.
              </p>
            </Field>
          )}

          {values.runLocation === "ssh" && (
            <SshCommandPreview
              hostLabel={values.sshHostLabel.trim() || null}
              spec={{
                meetingURL: values.meetingURL.trim(),
                participant: values.participant.trim(),
                displayName: values.displayName.trim() || undefined,
                ttl: values.ttl.trim(),
                headless: values.headless,
                network: values.network,
                authBackend: values.authBackend,
              }}
              testIdPrefix="ssh-cmd-preview"
            />
          )}
        </Section>

        {/* Section: Meeting */}
        <Section title="Meeting" description="Where the bot connects.">
          <Field
            label="Meeting URL"
            required
            error={errors.meetingURL}
            help={
              <HelpPopover fieldLabel="Meeting URL" testId="help-meetingURL">
                <p>The full URL the bot will navigate to.</p>
                <p className="mt-1">
                  Example:{" "}
                  <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                    http://localhost:3001/meeting/Test123
                  </code>
                  . Must include the{" "}
                  <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                    /meeting/&lt;id&gt;
                  </code>{" "}
                  path segment.
                </p>
              </HelpPopover>
            }
          >
            <HistoryInput
              fieldKey="meetingURL"
              value={values.meetingURL}
              onChange={(v) => setField("meetingURL", v)}
              placeholder="https://app.videocall.fnxlabs.com/meeting/TonyBots"
              className={INPUT_CLASS}
              testId="meeting-url"
              ariaInvalid={!!errors.meetingURL}
              ariaLabel="Meeting URL"
              type="url"
            />
          </Field>
        </Section>

        {/* Section: Identity */}
        <Section title="Identity" description="Who the bot is in the room.">
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <Field
              label="Participant"
              required
              error={errors.participant}
              help={
                <HelpPopover fieldLabel="Participant" testId="help-participant">
                  <p>Bot identity.</p>
                  <p className="mt-1">
                    For JWT auth this becomes the email subject{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      &lt;participant&gt;@bots-app.local
                    </code>
                    . For Guest, this becomes the display name shown in the meeting.
                  </p>
                </HelpPopover>
              }
            >
              <HistoryInput
                fieldKey="participant"
                value={values.participant}
                onChange={(v) => setField("participant", v)}
                placeholder="alice"
                className={INPUT_CLASS}
                testId="participant"
                ariaInvalid={!!errors.participant}
                ariaLabel="Participant"
              />
            </Field>

            <Field
              label="Display name (optional)"
              help={
                <HelpPopover fieldLabel="Display name" testId="help-displayName">
                  <p>What other meeting participants see.</p>
                  <p className="mt-1">Defaults to the participant handle when unset.</p>
                </HelpPopover>
              }
            >
              <HistoryInput
                fieldKey="displayName"
                value={values.displayName}
                onChange={(v) => setField("displayName", v)}
                placeholder="Alice"
                className={INPUT_CLASS}
                testId="display-name"
                ariaLabel="Display name"
              />
            </Field>
          </div>

          <Field
            label="Auth backend"
            help={
              <HelpPopover fieldLabel="Auth backend" testId="help-authBackend">
                <p>How the bot proves identity to the server.</p>
                <ul className="mt-1 list-disc space-y-0.5 pl-4">
                  <li>
                    <strong>JWT:</strong> inject a session cookie signed with the dev{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      JWT_SECRET
                    </code>
                    . Works on local + HCL daily + PR previews.
                  </li>
                  <li>
                    <strong>Storage State:</strong> replay a previously-captured session (use{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      bots-app login
                    </code>{" "}
                    to capture). Works against real-OAuth deployments.
                  </li>
                  <li>
                    <strong>Guest (no auth):</strong> skip auth entirely; works only when the
                    meeting allows guest join.
                  </li>
                </ul>
              </HelpPopover>
            }
          >
            <RadioGroup.Root
              value={values.authBackend}
              onValueChange={(v) => setField("authBackend", v as AuthBackend)}
              className="flex flex-col gap-2"
            >
              {AUTH_BACKENDS.map((opt) => (
                <label
                  key={opt.value}
                  className="flex items-center gap-2 text-sm text-neutral-700 dark:text-slate-200"
                  htmlFor={`auth-${opt.value}`}
                >
                  <RadioGroup.Item
                    id={`auth-${opt.value}`}
                    value={opt.value}
                    className="flex h-4 w-4 items-center justify-center rounded-full border border-neutral-300 bg-white data-[state=checked]:border-sky-500 dark:border-slate-500 dark:bg-slate-800 dark:data-[state=checked]:border-sky-400"
                  >
                    <RadioGroup.Indicator className="h-2 w-2 rounded-full bg-sky-500 dark:bg-sky-400" />
                  </RadioGroup.Item>
                  {opt.label}
                </label>
              ))}
            </RadioGroup.Root>
          </Field>

          {values.authBackend === "jwt" && (
            <SsoStateLine
              data={ssoStatusQuery.data}
              onConfigure={() => setSsoPanelOpen(true)}
            />
          )}

          {values.authBackend === "storage-state" && (
            <Field
              label="Storage-state file"
              error={errors.storageStateFile}
              required
              help={
                <HelpPopover fieldLabel="Storage state file" testId="help-storageStateFile">
                  <p>
                    Path to the storage-state JSON captured by{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      bots-app login
                    </code>
                    .
                  </p>
                  <p className="mt-1">Only used in Storage State mode.</p>
                </HelpPopover>
              }
            >
              <HistoryInput
                fieldKey="storageStateFile"
                value={values.storageStateFile}
                onChange={(v) => setField("storageStateFile", v)}
                placeholder="run/auth/alice.json"
                className={INPUT_CLASS}
                testId="storage-state-file"
                ariaInvalid={!!errors.storageStateFile}
                ariaLabel="Storage-state file path"
              />
            </Field>
          )}
        </Section>

        {/* Section: Behavior */}
        <Section title="Behavior" description="How the bot behaves while in the meeting.">
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <Field
              label="TTL"
              required
              error={errors.ttl}
              help={
                <HelpPopover fieldLabel="TTL" testId="help-ttl">
                  <p>How long the bot stays in the meeting before leaving cleanly.</p>
                  <p className="mt-1">
                    Use <code className="font-mono text-[11px]">5m</code>,{" "}
                    <code className="font-mono text-[11px]">30s</code>,{" "}
                    <code className="font-mono text-[11px]">2h</code>, or{" "}
                    <code className="font-mono text-[11px]">infinite</code>.
                  </p>
                </HelpPopover>
              }
            >
              <HistoryInput
                fieldKey="ttl"
                value={values.ttl}
                onChange={(v) => setField("ttl", v)}
                placeholder='"5m", "30s", "1h", or "infinite"'
                className={INPUT_CLASS}
                testId="ttl"
                ariaInvalid={!!errors.ttl}
                ariaLabel="TTL"
              />
              <div className="mt-2 flex flex-wrap gap-1">
                {TTL_SUGGESTIONS.map((s) => (
                  <button
                    key={s}
                    type="button"
                    onClick={() => setField("ttl", s)}
                    className={`rounded-full border px-2.5 py-0.5 text-xs ${
                      values.ttl === s
                        ? "border-sky-300 bg-sky-100 text-sky-700 dark:border-sky-700 dark:bg-sky-900/40 dark:text-sky-200"
                        : "border-neutral-200 bg-neutral-50 text-neutral-600 hover:bg-neutral-100 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700"
                    }`}
                  >
                    {s}
                  </button>
                ))}
              </div>
            </Field>

            <Field
              label="Network profile"
              error={errors.network}
              help={
                <HelpPopover fieldLabel="Network profile" testId="help-network">
                  <p>Simulated network conditions applied client-side.</p>
                  <p className="mt-1">
                    Requires{" "}
                    <code className="font-mono text-[11px]">videocall-client</code> to be built
                    with <code className="font-mono text-[11px]">--features netsim</code>. See
                    the Help page for profile details.
                  </p>
                </HelpPopover>
              }
            >
              <Select
                value={values.network}
                onValueChange={(v) => setField("network", v)}
                options={NETSIM_PRESETS.map((p) => ({ value: p, label: p }))}
                testId="network"
              />
            </Field>
          </div>

          <Field
            label="Headless"
            help={
              <HelpPopover fieldLabel="Headless" testId="help-headless">
                <p>Run Chrome without a visible window.</p>
                <p className="mt-1">
                  Less reliable for WebRTC; the default is headed for a reason.
                </p>
              </HelpPopover>
            }
          >
            <div className="flex items-center gap-3 py-1">
              <Switch.Root
                checked={values.headless}
                onCheckedChange={(v) => setField("headless", v)}
                className="relative h-6 w-10 rounded-full bg-neutral-200 data-[state=checked]:bg-sky-500 dark:bg-slate-600 dark:data-[state=checked]:bg-sky-500"
                data-testid="headless"
              >
                <Switch.Thumb className="block h-5 w-5 translate-x-0.5 rounded-full bg-white shadow transition-transform data-[state=checked]:translate-x-4" />
              </Switch.Root>
              <span className="text-sm text-neutral-600 dark:text-slate-300">
                {values.headless ? "Chrome will run headless" : "Chrome will run headed (default)"}
              </span>
            </div>
          </Field>
        </Section>

        {/* Section: Assets */}
        <Section title="Assets" description="Optional pre-rendered media inputs.">
          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <Field
              label="Fake camera (costume)"
              help={
                <HelpPopover fieldLabel="Costume" testId="help-costume">
                  <p>Pre-rendered .y4m fake camera.</p>
                  <p className="mt-1">
                    Run{" "}
                    <code className="font-mono text-[11px]">bots-app prep-assets</code> to
                    generate options.
                  </p>
                  <p className="mt-1">
                    If the selected file isn&apos;t present in{" "}
                    <code className="font-mono text-[11px]">&lt;runDir&gt;</code>, the bot
                    will auto-prime it on launch (local bots only — SSH bots need assets
                    pre-staged on the remote).
                  </p>
                </HelpPopover>
              }
              badge={
                costumeIsAutoMatched ? <AutoMatchedBadge testId="costume-auto-matched" /> : null
              }
            >
              <Select
                value={values.costume}
                onValueChange={(v) => setField("costume", v)}
                options={[
                  { value: "default", label: "Default fake pattern" },
                  ...(costumesQuery.data ?? []).map((f) => ({ value: f, label: f })),
                ]}
                testId="costume"
              />
            </Field>

            <Field
              label="Fake mic (audio)"
              help={
                <HelpPopover fieldLabel="Audio" testId="help-audio">
                  <p>Pre-stitched .wav fake mic.</p>
                  <p className="mt-1">Same prep step as the costume.</p>
                  <p className="mt-1">
                    If the selected file isn&apos;t present in{" "}
                    <code className="font-mono text-[11px]">&lt;runDir&gt;</code>, the bot
                    will auto-prime it on launch (local bots only — SSH bots need assets
                    pre-staged on the remote).
                  </p>
                </HelpPopover>
              }
              badge={audioIsAutoMatched ? <AutoMatchedBadge testId="audio-auto-matched" /> : null}
            >
              <Select
                value={values.audio}
                onValueChange={(v) => setField("audio", v)}
                options={[
                  { value: "default", label: "Default fake mic" },
                  ...(audioQuery.data ?? []).map((f) => ({ value: f, label: f })),
                ]}
                testId="audio"
              />
            </Field>
          </div>
        </Section>

        <div className="flex items-center justify-end gap-3 border-t border-neutral-100 pt-4 dark:border-slate-700">
          <button
            type="submit"
            disabled={launchMutation.isPending}
            className="inline-flex items-center gap-2 rounded-lg bg-sky-500 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600 dark:disabled:text-slate-400"
            data-testid="launch-button"
          >
            <Rocket className="h-4 w-4" />
            {launchMutation.isPending ? "Launching…" : "Launch Bot"}
          </button>
        </div>
      </form>
      <SsoPanel open={ssoPanelOpen} onOpenChange={setSsoPanelOpen} />
    </Tooltip.Provider>
  );
}

interface SsoStateLineProps {
  data?: SsoStatusResponse;
  onConfigure: () => void;
}

/**
 * Compact status line shown inside the Launch form's Identity section
 * when auth=JWT. Surfaces whether the captured `hcl-sso.json` exists
 * (and its age) so the operator knows what the bot will pick up. The
 * "Configure SSO" link opens the same SsoPanel dialog the header chip
 * uses — one source of truth for the recapture flow.
 */
function SsoStateLine({ data, onConfigure }: SsoStateLineProps) {
  if (data === undefined) {
    return (
      <p
        className="text-xs text-neutral-500 dark:text-slate-400"
        data-testid="launch-sso-line"
      >
        Checking SSO state…
      </p>
    );
  }
  if (!data.exists) {
    return (
      <p
        className="flex items-center gap-2 text-xs text-amber-700 dark:text-amber-300"
        data-testid="launch-sso-line"
      >
        <span aria-hidden="true">⚠️</span>
        No SSO state captured — bots will hit the HCL SSO portal.{" "}
        <button
          type="button"
          onClick={onConfigure}
          className="underline underline-offset-2 hover:text-amber-900 dark:hover:text-amber-200"
          data-testid="launch-sso-capture-now"
        >
          Capture now
        </button>
      </p>
    );
  }
  const age = data.ageHours !== null ? `${data.ageHours.toFixed(1)}h` : "?";
  return (
    <p
      className="flex flex-wrap items-center gap-x-2 text-xs text-neutral-600 dark:text-slate-300"
      data-testid="launch-sso-line"
    >
      Uses SSO state:{" "}
      <code
        className="rounded bg-neutral-100 px-1.5 py-0.5 font-mono text-[11px] text-neutral-700 dark:bg-slate-900 dark:text-slate-200"
        data-testid="launch-sso-path"
      >
        {data.filePath}
      </code>
      <span className="text-neutral-500 dark:text-slate-400">(captured {age} ago)</span>
      <button
        type="button"
        onClick={onConfigure}
        className="underline underline-offset-2 hover:text-neutral-900 dark:hover:text-slate-100"
        data-testid="launch-sso-configure"
      >
        Configure SSO
      </button>
    </p>
  );
}

interface SectionProps {
  title: string;
  description?: string;
  children: React.ReactNode;
}

function Section({ title, description, children }: SectionProps) {
  return (
    <fieldset
      className="rounded-lg border border-neutral-200 bg-neutral-50/40 p-4 dark:border-slate-700 dark:bg-slate-900/30"
      data-testid={`launch-section-${title.toLowerCase()}`}
    >
      <legend className="px-1 text-sm font-semibold tracking-tight text-neutral-800 dark:text-slate-100">
        {title}
      </legend>
      {description && (
        <p className="mb-3 text-xs text-neutral-500 dark:text-slate-400">{description}</p>
      )}
      <div className="flex flex-col gap-4">{children}</div>
    </fieldset>
  );
}

interface FieldProps {
  label: string;
  required?: boolean;
  error?: string;
  help?: React.ReactNode;
  /**
   * Optional inline annotation rendered immediately after the help
   * popover trigger. Used by the Assets section's Costume + Audio
   * fields to surface the {@link AutoMatchedBadge} when the current
   * value equals the manifest's match for the current participant.
   */
  badge?: React.ReactNode;
  children: React.ReactNode;
}

function Field({ label, required, error, help, badge, children }: FieldProps) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-center gap-1.5">
        <label className="text-sm font-medium text-neutral-800 dark:text-slate-200">
          {label}
          {required && <span className="ml-0.5 text-red-500 dark:text-red-400">*</span>}
        </label>
        {help}
        {badge}
      </div>
      {children}
      {error && (
        <p className="text-xs text-red-600 dark:text-red-400" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

/**
 * Tiny "wand" icon + tooltip rendered next to a Select whose value
 * was auto-defaulted from the manifest mapping. Disappears the moment
 * the operator picks a different value — the form drops the badge
 * because `<value> !== manifestMatch.{costume,audio}File` no longer
 * holds.
 */
function AutoMatchedBadge({ testId }: { testId: string }) {
  return (
    <Tooltip.Root delayDuration={150}>
      <Tooltip.Trigger asChild>
        <span
          data-testid={testId}
          aria-label="Auto-matched from manifest"
          className="inline-flex items-center rounded-full bg-sky-50 px-1.5 py-0.5 text-sky-700 dark:bg-sky-900/40 dark:text-sky-200"
        >
          <Wand2 className="h-3 w-3" aria-hidden="true" />
        </span>
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content
          side="top"
          sideOffset={6}
          className="z-50 rounded-md bg-neutral-900 px-2 py-1 text-xs text-white shadow-md dark:bg-slate-700 dark:text-slate-100"
        >
          Auto-matched from manifest
          <Tooltip.Arrow className="fill-neutral-900 dark:fill-slate-700" />
        </Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}
