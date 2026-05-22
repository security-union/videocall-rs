import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import * as RadioGroup from "@radix-ui/react-radio-group";
import * as Switch from "@radix-ui/react-switch";
import * as Tooltip from "@radix-ui/react-tooltip";
import { Dices, RotateCcw, Users } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { MultiLaunchRequest, MultiLaunchResponse } from "../api/types";
// Shared `sso/status` query — same cache key as LaunchForm + SsoChip +
// SsoPanel, so opening the multi-launch tab does NOT add a duplicate
// network call. The wire-through below mirrors the single-launch path
// added in v1.5.0 so dashboard-spawned multi-launch bots also pick up
// the captured `<runDir>/auth/hcl-sso.json` automatically.
import {
  recordLaunchedBot,
  runLocationLabelFor,
  type LaunchedBotHistoryEntry,
} from "../lib/botHistory";
import {
  AUTH_BACKENDS,
  NETSIM_PRESETS,
  RUN_LOCATIONS,
  TTL_SUGGESTIONS,
  type AuthBackend,
  type RunLocation,
} from "../lib/constants";
import { useFieldHistory } from "../lib/fieldHistory";
import { isValidMeetingUrl } from "../lib/validation";
import { isValidTtl } from "../lib/ttl";
import { HelpPopover } from "./ui/HelpPopover";
import { HistoryInput } from "./ui/HistoryInput";
import { LoadPreviousButton } from "./LoadPreviousButton";
import { Select } from "./ui/Select";
import { SshCommandPreview } from "./SshCommandPreview";

/**
 * Default + max participants the dashboard surface lets an operator
 * spawn from one click. Mirrors the CLI's `bots-app run --max-users 10`
 * default. Operators who genuinely need more bots can pass an explicit
 * cap on the request — the dashboard form does not expose that knob.
 */
const DEFAULT_COUNT = 3;
const MAX_USERS = 10;
/**
 * Default seconds to wait between consecutive bot spawns. Picked to
 * give each Chrome instance enough headroom to boot + register with
 * NATS before the next one hits the orchestrator — empirically 2s is
 * the sweet spot on a typical SSH host. Operators who want
 * back-to-back behavior can set this to 0.
 */
const DEFAULT_SPAWN_DELAY_SECONDS = 2;
const MAX_SPAWN_DELAY_SECONDS = 60;

interface MultiLaunchFormProps {
  onLaunched: (response: MultiLaunchResponse) => void;
  onError: (message: string) => void;
  /**
   * Optional toast hook used by `handleLoadPrevious` to confirm
   * which previous bot config was loaded into the form. Lets the
   * parent surface a "Loaded previous config" toast so the operator
   * has a visible signal that the click landed and the form was
   * repopulated. When omitted (e.g. tests), the load proceeds
   * silently.
   */
  onToast?: (t: {
    title: string;
    description?: string;
    variant: "success" | "info" | "error";
  }) => void;
}

interface FormErrors {
  count?: string;
  seed?: string;
  meetingURL?: string;
  ttl?: string;
  storageStateFile?: string;
  sshHostLabel?: string;
  spawnDelaySeconds?: string;
}

interface MultiLaunchFormValues {
  mode: "first-n" | "random";
  count: number;
  seed: string; // free-text → optional integer
  includeObservers: boolean;
  meetingURL: string;
  ttl: string;
  network: string;
  headless: boolean;
  authBackend: AuthBackend;
  storageStateFile: string;
  displayNameTemplate: string;
  /** Per-batch run-location; every spawned bot goes to the same host. */
  runLocation: RunLocation;
  sshHostLabel: string;
  /**
   * Seconds to wait between consecutive bot spawns. Sent to the
   * server which paces the launch loop; the delay is applied
   * *between* iterations only (no delay before the first bot).
   */
  spawnDelaySeconds: number;
}

const DEFAULTS: MultiLaunchFormValues = {
  mode: "first-n",
  count: DEFAULT_COUNT,
  seed: "",
  includeObservers: false,
  meetingURL: "",
  ttl: "5m",
  network: "none",
  headless: false,
  authBackend: "jwt",
  storageStateFile: "",
  displayNameTemplate: "",
  runLocation: "local",
  sshHostLabel: "",
  spawnDelaySeconds: DEFAULT_SPAWN_DELAY_SECONDS,
};

const INPUT_CLASS =
  "w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm placeholder:text-neutral-400 focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 disabled:cursor-not-allowed disabled:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-100 dark:placeholder:text-slate-500 dark:focus:border-sky-400 dark:focus:ring-sky-400 dark:disabled:bg-slate-900";

function validate(values: MultiLaunchFormValues): FormErrors {
  const errors: FormErrors = {};
  if (!Number.isFinite(values.count) || values.count <= 0) {
    errors.count = "Count must be a positive integer";
  } else if (values.count > MAX_USERS) {
    errors.count = `Count must be ≤ ${MAX_USERS}`;
  }
  if (values.mode === "random" && values.seed.trim() !== "") {
    const n = Number.parseInt(values.seed.trim(), 10);
    if (!Number.isFinite(n)) {
      errors.seed = "Seed must be an integer (or leave blank for a fresh seed)";
    }
  }
  if (!isValidMeetingUrl(values.meetingURL)) {
    errors.meetingURL = "Meeting URL must be a full http(s) URL with a /meeting/<id> path";
  }
  if (!isValidTtl(values.ttl)) {
    errors.ttl = `TTL must be "<int>s|m|h" or "infinite" (got "${values.ttl}")`;
  }
  if (values.authBackend === "storage-state" && values.storageStateFile.trim() === "") {
    errors.storageStateFile = "Storage-state file path is required when auth=storage-state";
  }
  if (values.runLocation === "ssh" && values.sshHostLabel.trim() === "") {
    errors.sshHostLabel = "Pick a registered SSH host";
  }
  if (
    !Number.isFinite(values.spawnDelaySeconds) ||
    values.spawnDelaySeconds < 0 ||
    values.spawnDelaySeconds > MAX_SPAWN_DELAY_SECONDS
  ) {
    errors.spawnDelaySeconds = `Delay must be between 0 and ${MAX_SPAWN_DELAY_SECONDS} seconds`;
  }
  return errors;
}

export function MultiLaunchForm({ onLaunched, onError, onToast }: MultiLaunchFormProps) {
  const [values, setValues] = useState<MultiLaunchFormValues>(DEFAULTS);
  const [errors, setErrors] = useState<FormErrors>({});
  const [submitted, setSubmitted] = useState(false);

  // SSH host registry (used to populate the host Select and to gate
  // the SSH radio option). Same query key as the LaunchForm so the
  // cache is shared.
  const hostsQuery = useQuery({
    queryKey: ["ssh", "hosts"],
    queryFn: api.listHosts,
    refetchInterval: 60_000,
  });

  // Captured SSO state status. Shares its cache with the header chip,
  // SsoPanel, and LaunchForm so this hook does NOT trigger an extra
  // network call. The submit handler below reads `data.filePath` to
  // forward the captured `<runDir>/auth/hcl-sso.json` to every spawned
  // bot when `authBackend === "jwt"` — same behavior as single-launch.
  // Without this, multi-launch bots would silently ignore the captured
  // SSO state even though the file existed (matching the v1.4.x
  // pre-fix single-launch regression).
  const ssoStatusQuery = useQuery({
    queryKey: ["sso", "status"],
    queryFn: api.ssoStatus,
    refetchInterval: 60_000,
  });

  // Per-field history controllers, mirroring the single-bot LaunchForm
  // wiring so the multi-launch form's free-text inputs remember what
  // the operator has previously typed. Three keys (`meetingURL`, `ttl`,
  // `storageStateFile`) are *shared* with LaunchForm — same semantics
  // means a value entered in one form surfaces as a suggestion in the
  // other. Two keys (`displayNameTemplate`, `seed`) are multi-only;
  // they have different semantics from the single-bot equivalents
  // (template vs. exact name, seed integer vs. n/a) and would pollute
  // the shared bucket if reused.
  const meetingUrlHistory = useFieldHistory("meetingURL");
  const ttlHistory = useFieldHistory("ttl");
  const storageStateHistory = useFieldHistory("storageStateFile");
  const displayNameTemplateHistory = useFieldHistory("displayNameTemplate");
  const seedHistory = useFieldHistory("seed");

  const mutation = useMutation({
    mutationFn: (req: MultiLaunchRequest) => api.launchMulti(req),
    onSuccess: (data) => {
      // Persist the submitted free-text values into each field's
      // history (mirrors LaunchForm). Optional fields (storageStateFile,
      // displayNameTemplate, seed) are pushed only when non-empty so we
      // don't poison the suggestion list with blanks. meetingURL and
      // ttl are required, so they're always pushed on success.
      meetingUrlHistory.push(values.meetingURL);
      ttlHistory.push(values.ttl);
      if (values.storageStateFile.trim()) storageStateHistory.push(values.storageStateFile);
      if (values.displayNameTemplate.trim())
        displayNameTemplateHistory.push(values.displayNameTemplate);
      if (values.mode === "random" && values.seed.trim()) seedHistory.push(values.seed);
      // Capture a *common-fields* spec into the shared launched-bot
      // history. Multi-launch has no single participant, so we
      // synthesize a `multi:<mode>-<count>` label and leave the
      // single-bot-only fields (participant, displayName, costume,
      // audio) at their defaults. When the operator later picks this
      // entry from the Load-previous dropdown in single-launch, the
      // common fields pre-fill and they fill in participant by hand;
      // when picked in multi-launch, only the common subset applies.
      const syntheticLabel = `multi:${values.mode}-${values.count}`;
      const spec = {
        meetingURL: values.meetingURL,
        participant: "",
        displayName: "",
        ttl: values.ttl,
        network: values.network,
        headless: values.headless,
        authBackend: values.authBackend,
        storageStateFile: values.storageStateFile,
        runLocation: values.runLocation,
        sshHostLabel: values.sshHostLabel,
        costume: "default",
        audio: "default",
      };
      recordLaunchedBot({
        spec,
        launchedAt: Date.now(),
        meetingURL: values.meetingURL,
        participant: syntheticLabel,
        runLocationLabel: runLocationLabelFor(spec),
      });
      // Keep all field values intact so the operator can immediately
      // launch another batch with the same shared config (e.g.
      // "launched 3, want to launch 2 more"). Re-arm by clearing
      // `submitted` so the next click validates fresh. The parent
      // emits the success toast via `onLaunched`.
      setSubmitted(false);
      onLaunched(data);
    },
    onError: (err) => {
      // Preserve all field values on failure too — operators almost
      // always want to retry with the same shared config after an
      // error. The parent emits the error toast.
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onError(msg);
    },
  });

  /**
   * Reset every field to its empty/default value (matches the
   * `DEFAULTS` constant). Also clears validation errors and re-arms
   * `submitted`. Disabled while a launch is in-flight.
   */
  const handleReset = () => {
    setValues(DEFAULTS);
    setErrors({});
    setSubmitted(false);
  };

  /**
   * Pull the *common-fields* subset from a previously-launched bot
   * (single OR multi) back into the form. The multi-specific knobs
   * (count, seed, mode, includeObservers, displayNameTemplate) stay
   * at their current values so the operator's batch shape isn't
   * disturbed by a load that came from a single-bot snapshot.
   */
  const handleLoadPrevious = (entry: LaunchedBotHistoryEntry) => {
    setValues((prev) => ({
      ...prev,
      meetingURL: entry.spec.meetingURL,
      ttl: entry.spec.ttl,
      network: entry.spec.network,
      headless: entry.spec.headless,
      authBackend: entry.spec.authBackend,
      storageStateFile: entry.spec.storageStateFile,
      runLocation: entry.spec.runLocation,
      sshHostLabel: entry.spec.sshHostLabel,
    }));
    setErrors({});
    setSubmitted(false);
    // Surface a confirmation toast naming the loaded config so the
    // operator has a visible signal that the click landed. Multi-only
    // fields (count, seed, mode, includeObservers, displayNameTemplate)
    // stay as-is — the toast description reflects ONLY the shared
    // fields actually loaded, so the operator knows what changed.
    onToast?.({
      title: "Loaded previous bot config (shared fields only)",
      description: `${entry.meetingURL}`,
      variant: "info",
    });
  };

  const setField = <K extends keyof MultiLaunchFormValues>(
    key: K,
    val: MultiLaunchFormValues[K],
  ) => {
    setValues((prev) => ({ ...prev, [key]: val }));
    if (submitted) {
      setErrors(validate({ ...values, [key]: val }));
    }
  };

  const handleSubmit: React.FormEventHandler<HTMLFormElement> = (e) => {
    e.preventDefault();
    setSubmitted(true);
    const v = validate(values);
    setErrors(v);
    if (Object.keys(v).length > 0) return;

    // When the operator is using JWT auth and we have a captured SSO
    // state file on disk, forward its path so every spawned bot's
    // BrowserContext loads its cookies before the JWT session cookie
    // is injected. This is the multi-launch counterpart of the
    // single-launch wire-through added in v1.5.0 — without it,
    // multi-launch bots ignore `<runDir>/auth/hcl-sso.json` even when
    // the file exists, and the page-load hits the HCL SSO portal on
    // every spawn (which is exactly the bug the user reported).
    const ssoStateFile =
      values.authBackend === "jwt" && ssoStatusQuery.data?.exists
        ? ssoStatusQuery.data.filePath
        : undefined;

    const req: MultiLaunchRequest = {
      mode: values.mode,
      count: values.count,
      meetingURL: values.meetingURL.trim(),
      ttl: values.ttl.trim(),
      network: values.network,
      headless: values.headless,
      authBackend: values.authBackend,
      ssoStateFile,
      runLocation:
        values.runLocation === "ssh"
          ? { kind: "ssh", hostLabel: values.sshHostLabel.trim() }
          : { kind: "local" },
      // Always send; server treats 0 as "no delay" and any positive
      // value as a between-iteration wait. Default is 2 in the UI.
      spawnDelaySeconds: values.spawnDelaySeconds,
    };
    if (values.mode === "random") {
      if (values.seed.trim() !== "") {
        req.seed = Number.parseInt(values.seed.trim(), 10);
      }
      req.includeObservers = values.includeObservers;
    }
    if (values.authBackend === "storage-state" && values.storageStateFile.trim() !== "") {
      req.storageStateFile = values.storageStateFile.trim();
    }
    if (values.displayNameTemplate.trim() !== "") {
      req.displayNameTemplate = values.displayNameTemplate.trim();
    }
    mutation.mutate(req);
  };

  return (
    <Tooltip.Provider>
      <form className="flex flex-col gap-5" onSubmit={handleSubmit} noValidate>
        {/* Section: Runtime — rendered first so the operator picks the
            run location before downstream choices (asset availability,
            network reachability, applicable auth flow) depend on it.
            All N bots share one runtime / one host. */}
        <Section title="Runtime" description="Where every spawned bot's Chrome runs.">
          <Field
            label="Run location"
            help={
              <HelpPopover fieldLabel="Run location" testId="help-multi-run-location">
                <p>
                  Local runs the bots in this orchestrator&apos;s own Node process. SSH-able host
                  runs them all on the same registered remote machine via{" "}
                  <code className="font-mono text-[11px]">ssh user@host npm run bot …</code>.
                </p>
                <p className="mt-1">
                  v1 does not fan out across hosts; pick one. Cloud VM and Docker remain future
                  features.
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
                        htmlFor={`multi-runloc-${loc.value}`}
                      >
                        <RadioGroup.Item
                          id={`multi-runloc-${loc.value}`}
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
                <HelpPopover fieldLabel="SSH host" testId="help-multi-ssh-host">
                  <p>
                    All {values.count} bots will be launched on this host. Same v1 caveats as the
                    single-launch flow.
                  </p>
                </HelpPopover>
              }
            >
              <Select
                value={values.sshHostLabel || "__none__"}
                onValueChange={(v) => setField("sshHostLabel", v === "__none__" ? "" : v)}
                options={[
                  { value: "__none__", label: "Pick a host…" },
                  ...(hostsQuery.data?.hosts ?? []).map((h) => ({
                    value: h.label,
                    label: `${h.label}  (${h.user}@${h.host})`,
                  })),
                ]}
                testId="multi-ssh-host-select"
              />
            </Field>
          )}

          {values.runLocation === "ssh" && (
            <SshCommandPreview
              hostLabel={values.sshHostLabel.trim() || null}
              spec={{
                meetingURL: values.meetingURL.trim(),
                participant: "alice",
                ttl: values.ttl.trim(),
                headless: values.headless,
                network: values.network,
                authBackend: values.authBackend,
              }}
              subtitle="Preview for first participant — every bot in the batch runs on this same host with this same command shape; only --participant differs."
              testIdPrefix="multi-ssh-cmd-preview"
            />
          )}
        </Section>

        {/* Section: Pick mode */}
        <Section title="Pick mode" description="How to choose participants from the manifest.">
          <Field
            label="Mode"
            help={
              <HelpPopover fieldLabel="Mode" testId="help-multi-mode">
                <p>How participants are chosen from the manifest:</p>
                <ul className="mt-1 list-disc space-y-0.5 pl-4">
                  <li>
                    <strong>First-N:</strong> deterministic; pick the first N named participants in
                    manifest order (alice, bob, carol, …). Matches{" "}
                    <code className="font-mono text-[11px]">bots-app run --users N</code>.
                  </li>
                  <li>
                    <strong>Random N:</strong> seeded shuffle of eligible participants. Matches{" "}
                    <code className="font-mono text-[11px]">bots-app gen --count N --seed S</code>.
                  </li>
                </ul>
              </HelpPopover>
            }
          >
            <RadioGroup.Root
              value={values.mode}
              onValueChange={(v) => setField("mode", v as "first-n" | "random")}
              className="flex flex-col gap-2"
            >
              <label
                className="flex items-center gap-2 text-sm text-neutral-700 dark:text-slate-200"
                htmlFor="multi-mode-first-n"
              >
                <RadioGroup.Item
                  id="multi-mode-first-n"
                  value="first-n"
                  className="flex h-4 w-4 items-center justify-center rounded-full border border-neutral-300 bg-white data-[state=checked]:border-sky-500 dark:border-slate-500 dark:bg-slate-800 dark:data-[state=checked]:border-sky-400"
                >
                  <RadioGroup.Indicator className="h-2 w-2 rounded-full bg-sky-500 dark:bg-sky-400" />
                </RadioGroup.Item>
                <Users className="h-3.5 w-3.5 text-neutral-400" aria-hidden="true" />
                First-N from manifest
              </label>
              <label
                className="flex items-center gap-2 text-sm text-neutral-700 dark:text-slate-200"
                htmlFor="multi-mode-random"
              >
                <RadioGroup.Item
                  id="multi-mode-random"
                  value="random"
                  className="flex h-4 w-4 items-center justify-center rounded-full border border-neutral-300 bg-white data-[state=checked]:border-sky-500 dark:border-slate-500 dark:bg-slate-800 dark:data-[state=checked]:border-sky-400"
                >
                  <RadioGroup.Indicator className="h-2 w-2 rounded-full bg-sky-500 dark:bg-sky-400" />
                </RadioGroup.Item>
                <Dices className="h-3.5 w-3.5 text-neutral-400" aria-hidden="true" />
                Random N (seeded)
              </label>
            </RadioGroup.Root>
          </Field>

          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <Field
              label="Count"
              required
              error={errors.count}
              help={
                <HelpPopover fieldLabel="Count" testId="help-multi-count">
                  <p>How many bots to launch.</p>
                  <p className="mt-1">Capped at {MAX_USERS} server-side.</p>
                </HelpPopover>
              }
            >
              <input
                type="number"
                min={1}
                max={MAX_USERS}
                value={values.count}
                onChange={(e) => setField("count", Number.parseInt(e.target.value, 10) || 0)}
                className={INPUT_CLASS}
                data-testid="multi-count"
                aria-label="Count"
                aria-invalid={!!errors.count}
              />
            </Field>

            <Field
              label="Delay between launches (seconds)"
              error={errors.spawnDelaySeconds}
              help={
                <HelpPopover fieldLabel="Delay between launches" testId="help-multi-spawn-delay">
                  <p>
                    Seconds to wait between consecutive bot spawns. The orchestrator paces the
                    launch loop so each Chrome instance has headroom to boot and register before the
                    next one starts.
                  </p>
                  <p className="mt-1">
                    Total added wait is{" "}
                    <code className="font-mono text-[11px]">(count − 1) × delay</code> — e.g. count
                    5, delay 2 = ~8s of staggering. Set to 0 to fire spawns back-to-back (legacy
                    behavior).
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="number"
                min={0}
                max={MAX_SPAWN_DELAY_SECONDS}
                step={1}
                value={values.spawnDelaySeconds}
                onChange={(e) => {
                  const n = Number.parseInt(e.target.value, 10);
                  setField("spawnDelaySeconds", Number.isFinite(n) ? n : 0);
                }}
                className={INPUT_CLASS}
                data-testid="multi-spawn-delay-seconds"
                aria-label="Delay between launches in seconds"
                aria-invalid={!!errors.spawnDelaySeconds}
              />
            </Field>

            {values.mode === "random" && (
              <Field
                label="Seed (optional)"
                error={errors.seed}
                help={
                  <HelpPopover fieldLabel="Seed" testId="help-multi-seed">
                    <p>
                      Integer seed for the deterministic shuffle. Same seed + same count + same
                      manifest = same picks.
                    </p>
                    <p className="mt-1">Leave blank for a fresh random seed each click.</p>
                  </HelpPopover>
                }
              >
                <HistoryInput
                  fieldKey="seed"
                  value={values.seed}
                  onChange={(v) => setField("seed", v)}
                  placeholder="e.g. 42"
                  className={INPUT_CLASS}
                  testId="multi-seed"
                  ariaInvalid={!!errors.seed}
                  ariaLabel="Seed"
                  inputMode="numeric"
                />
              </Field>
            )}
          </div>

          {values.mode === "random" && (
            <Field
              label="Include observers"
              help={
                <HelpPopover fieldLabel="Include observers" testId="help-multi-observers">
                  <p>
                    By default the random shuffle only picks costumed participants (named characters
                    with a y4m + WAV). Enable to allow observer-NN slots — useful for meetings with
                    many receive-only seats.
                  </p>
                </HelpPopover>
              }
            >
              <div className="flex items-center gap-3 py-1">
                <Switch.Root
                  checked={values.includeObservers}
                  onCheckedChange={(v) => setField("includeObservers", v)}
                  className="relative h-6 w-10 rounded-full bg-neutral-200 data-[state=checked]:bg-sky-500 dark:bg-slate-600 dark:data-[state=checked]:bg-sky-500"
                  data-testid="multi-include-observers"
                >
                  <Switch.Thumb className="block h-5 w-5 translate-x-0.5 rounded-full bg-white shadow transition-transform data-[state=checked]:translate-x-4" />
                </Switch.Root>
                <span className="text-sm text-neutral-600 dark:text-slate-300">
                  {values.includeObservers
                    ? "Will include observer-NN slots"
                    : "Costumed participants only (default)"}
                </span>
              </div>
            </Field>
          )}
        </Section>

        {/* Section: Meeting */}
        <Section title="Meeting" description="Shared across all spawned bots.">
          <Field
            label="Meeting URL"
            required
            error={errors.meetingURL}
            help={
              <HelpPopover fieldLabel="Meeting URL" testId="help-multi-meetingURL">
                <p>The full URL all spawned bots will navigate to.</p>
                <p className="mt-1">
                  Example:{" "}
                  <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                    https://app.videocall.fnxlabs.com/meeting/Test123
                  </code>
                  .
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
              testId="multi-meeting-url"
              ariaInvalid={!!errors.meetingURL}
              ariaLabel="Meeting URL"
              type="url"
            />
          </Field>

          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <Field
              label="TTL"
              required
              error={errors.ttl}
              help={
                <HelpPopover fieldLabel="TTL" testId="help-multi-ttl">
                  <p>How long each bot stays in the meeting before leaving cleanly.</p>
                  <p className="mt-1">
                    Same lifetime applies to every spawned bot. Use{" "}
                    <code className="font-mono text-[11px]">5m</code>,{" "}
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
                testId="multi-ttl"
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
              help={
                <HelpPopover fieldLabel="Network profile" testId="help-multi-network">
                  <p>Simulated network conditions applied to all spawned bots.</p>
                </HelpPopover>
              }
            >
              <Select
                value={values.network}
                onValueChange={(v) => setField("network", v)}
                options={NETSIM_PRESETS.map((p) => ({ value: p, label: p }))}
                testId="multi-network"
              />
            </Field>
          </div>

          <Field
            label="Display name template (optional)"
            help={
              <HelpPopover fieldLabel="Display name template" testId="help-multi-display-name">
                <p>
                  Optional template applied to every spawned bot. Use{" "}
                  <code className="font-mono text-[11px]">{"{participant}"}</code> to insert the
                  handle (e.g. <code className="font-mono text-[11px]">{"Bot {participant}"}</code>{" "}
                  → <code className="font-mono text-[11px]">Bot alice</code>).
                </p>
              </HelpPopover>
            }
          >
            <HistoryInput
              fieldKey="displayNameTemplate"
              value={values.displayNameTemplate}
              onChange={(v) => setField("displayNameTemplate", v)}
              placeholder="Bot {participant}"
              className={INPUT_CLASS}
              testId="multi-display-name-template"
              ariaLabel="Display name template"
            />
          </Field>

          <Field label="Headless">
            <div className="flex items-center gap-3 py-1">
              <Switch.Root
                checked={values.headless}
                onCheckedChange={(v) => setField("headless", v)}
                className="relative h-6 w-10 rounded-full bg-neutral-200 data-[state=checked]:bg-sky-500 dark:bg-slate-600 dark:data-[state=checked]:bg-sky-500"
                data-testid="multi-headless"
              >
                <Switch.Thumb className="block h-5 w-5 translate-x-0.5 rounded-full bg-white shadow transition-transform data-[state=checked]:translate-x-4" />
              </Switch.Root>
              <span className="text-sm text-neutral-600 dark:text-slate-300">
                {values.headless ? "Chrome will run headless" : "Chrome will run headed (default)"}
              </span>
            </div>
          </Field>
        </Section>

        {/* Section: Identity */}
        <Section title="Identity" description="Shared auth across all spawned bots.">
          <Field
            label="Auth backend"
            help={
              <HelpPopover fieldLabel="Auth backend" testId="help-multi-auth">
                <p>How each spawned bot proves identity to the server.</p>
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
                  htmlFor={`multi-auth-${opt.value}`}
                >
                  <RadioGroup.Item
                    id={`multi-auth-${opt.value}`}
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

          {values.authBackend === "storage-state" && (
            <Field
              label="Storage-state file"
              error={errors.storageStateFile}
              required
              help={
                <HelpPopover fieldLabel="Storage-state file" testId="help-multi-storage">
                  <p>
                    Shared storage-state JSON for every spawned bot. For per-bot OAuth use the YAML
                    config import or single-bot launch flow.
                  </p>
                </HelpPopover>
              }
            >
              <HistoryInput
                fieldKey="storageStateFile"
                value={values.storageStateFile}
                onChange={(v) => setField("storageStateFile", v)}
                placeholder="run/auth/alice.json"
                className={INPUT_CLASS}
                testId="multi-storage-state-file"
                ariaInvalid={!!errors.storageStateFile}
                ariaLabel="Storage-state file"
              />
            </Field>
          )}
        </Section>

        <div className="flex items-center justify-end gap-3 border-t border-neutral-100 pt-4 dark:border-slate-700">
          <Tooltip.Root delayDuration={300}>
            <Tooltip.Trigger asChild>
              <button
                type="button"
                onClick={handleReset}
                disabled={mutation.isPending}
                className="inline-flex items-center gap-2 rounded-lg border border-neutral-300 bg-white px-4 py-2 text-sm font-medium text-neutral-700 shadow-sm transition-colors hover:bg-neutral-50 disabled:cursor-not-allowed disabled:bg-neutral-100 disabled:text-neutral-400 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700 dark:disabled:bg-slate-900 dark:disabled:text-slate-500"
                data-testid="multi-reset-button"
                aria-label="Reset form"
              >
                <RotateCcw className="h-4 w-4" />
                Reset
              </button>
            </Tooltip.Trigger>
            <Tooltip.Portal>
              <Tooltip.Content
                side="top"
                sideOffset={6}
                className="z-50 rounded-md bg-neutral-900 px-2 py-1 text-xs text-white shadow-md dark:bg-slate-700 dark:text-slate-100"
              >
                Clear all fields and start fresh.
                <Tooltip.Arrow className="fill-neutral-900 dark:fill-slate-700" />
              </Tooltip.Content>
            </Tooltip.Portal>
          </Tooltip.Root>
          <LoadPreviousButton
            onSelect={handleLoadPrevious}
            disabled={mutation.isPending}
            testId="multi-load-previous-button"
          />
          <button
            type="submit"
            disabled={mutation.isPending}
            className="inline-flex items-center gap-2 rounded-lg bg-sky-500 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600 dark:disabled:text-slate-400"
            data-testid="multi-launch-button"
          >
            {values.mode === "random" ? (
              <Dices className="h-4 w-4" />
            ) : (
              <Users className="h-4 w-4" />
            )}
            {mutation.isPending
              ? `Launching ${values.count} bot${values.count === 1 ? "" : "s"}…`
              : `Launch ${values.count} bot${values.count === 1 ? "" : "s"}`}
          </button>
        </div>
      </form>
    </Tooltip.Provider>
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
      data-testid={`multi-section-${title.toLowerCase().replace(/\s+/g, "-")}`}
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
  children: React.ReactNode;
}

function Field({ label, required, error, help, children }: FieldProps) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-center gap-1.5">
        <label className="text-sm font-medium text-neutral-800 dark:text-slate-200">
          {label}
          {required && <span className="ml-0.5 text-red-500 dark:text-red-400">*</span>}
        </label>
        {help}
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
