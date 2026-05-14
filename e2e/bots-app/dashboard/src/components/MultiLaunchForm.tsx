import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import * as RadioGroup from "@radix-ui/react-radio-group";
import * as Switch from "@radix-ui/react-switch";
import * as Tooltip from "@radix-ui/react-tooltip";
import { Dices, Users } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { MultiLaunchRequest, MultiLaunchResponse } from "../api/types";
import {
  AUTH_BACKENDS,
  NETSIM_PRESETS,
  TTL_SUGGESTIONS,
  type AuthBackend,
} from "../lib/constants";
import { isValidMeetingUrl } from "../lib/validation";
import { isValidTtl } from "../lib/ttl";
import { HelpPopover } from "./ui/HelpPopover";
import { Select } from "./ui/Select";

/**
 * Default + max participants the dashboard surface lets an operator
 * spawn from one click. Mirrors the CLI's `bots-app run --max-users 10`
 * default. Operators who genuinely need more bots can pass an explicit
 * cap on the request — the dashboard form does not expose that knob.
 */
const DEFAULT_COUNT = 3;
const MAX_USERS = 10;

interface MultiLaunchFormProps {
  onLaunched: (response: MultiLaunchResponse) => void;
  onError: (message: string) => void;
}

interface FormErrors {
  count?: string;
  seed?: string;
  meetingURL?: string;
  ttl?: string;
  storageStateFile?: string;
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
  return errors;
}

export function MultiLaunchForm({ onLaunched, onError }: MultiLaunchFormProps) {
  const [values, setValues] = useState<MultiLaunchFormValues>(DEFAULTS);
  const [errors, setErrors] = useState<FormErrors>({});
  const [submitted, setSubmitted] = useState(false);

  const mutation = useMutation({
    mutationFn: (req: MultiLaunchRequest) => api.launchMulti(req),
    onSuccess: (data) => {
      onLaunched(data);
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onError(msg);
    },
  });

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

    const req: MultiLaunchRequest = {
      mode: values.mode,
      count: values.count,
      meetingURL: values.meetingURL.trim(),
      ttl: values.ttl.trim(),
      network: values.network,
      headless: values.headless,
      authBackend: values.authBackend,
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
        {/* Section: Pick mode */}
        <Section
          title="Pick mode"
          description="How to choose participants from the manifest."
        >
          <Field
            label="Mode"
            help={
              <HelpPopover fieldLabel="Mode" testId="help-multi-mode">
                <p>How participants are chosen from the manifest:</p>
                <ul className="mt-1 list-disc space-y-0.5 pl-4">
                  <li>
                    <strong>First-N:</strong> deterministic; pick the first N named
                    participants in manifest order (alice, bob, carol, …). Matches{" "}
                    <code className="font-mono text-[11px]">bots-app run --users N</code>.
                  </li>
                  <li>
                    <strong>Random N:</strong> seeded shuffle of eligible participants.
                    Matches <code className="font-mono text-[11px]">bots-app gen --count N --seed S</code>.
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
                <input
                  type="text"
                  inputMode="numeric"
                  value={values.seed}
                  onChange={(e) => setField("seed", e.target.value)}
                  className={INPUT_CLASS}
                  placeholder="e.g. 42"
                  data-testid="multi-seed"
                  aria-label="Seed"
                  aria-invalid={!!errors.seed}
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
                    By default the random shuffle only picks costumed participants (named
                    characters with a y4m + WAV). Enable to allow observer-NN slots — useful
                    for meetings with many receive-only seats.
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
            <input
              type="url"
              value={values.meetingURL}
              onChange={(e) => setField("meetingURL", e.target.value)}
              className={INPUT_CLASS}
              placeholder="https://app.videocall.fnxlabs.com/meeting/TonyBots"
              data-testid="multi-meeting-url"
              aria-label="Meeting URL"
              aria-invalid={!!errors.meetingURL}
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
              <input
                type="text"
                value={values.ttl}
                onChange={(e) => setField("ttl", e.target.value)}
                className={INPUT_CLASS}
                placeholder='"5m", "30s", "1h", or "infinite"'
                data-testid="multi-ttl"
                aria-label="TTL"
                aria-invalid={!!errors.ttl}
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
                  handle (e.g.{" "}
                  <code className="font-mono text-[11px]">{"Bot {participant}"}</code> →{" "}
                  <code className="font-mono text-[11px]">Bot alice</code>).
                </p>
              </HelpPopover>
            }
          >
            <input
              type="text"
              value={values.displayNameTemplate}
              onChange={(e) => setField("displayNameTemplate", e.target.value)}
              className={INPUT_CLASS}
              placeholder="Bot {participant}"
              data-testid="multi-display-name-template"
              aria-label="Display name template"
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
                    Shared storage-state JSON for every spawned bot. For per-bot OAuth use
                    the YAML config import or single-bot launch flow.
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={values.storageStateFile}
                onChange={(e) => setField("storageStateFile", e.target.value)}
                className={INPUT_CLASS}
                placeholder="run/auth/alice.json"
                data-testid="multi-storage-state-file"
                aria-label="Storage-state file"
                aria-invalid={!!errors.storageStateFile}
              />
            </Field>
          )}
        </Section>

        <div className="flex items-center justify-end gap-3 border-t border-neutral-100 pt-4 dark:border-slate-700">
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
