import { useEffect, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import * as RadioGroup from "@radix-ui/react-radio-group";
import * as Switch from "@radix-ui/react-switch";
import * as Tooltip from "@radix-ui/react-tooltip";
import { Rocket } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { LaunchRequest } from "../api/types";
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
import { HistoryInput } from "./ui/HistoryInput";
import { Select } from "./ui/Select";

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
  costume: "default",
  audio: "default",
};

const INPUT_CLASS =
  "w-full rounded-lg border border-neutral-300 px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500";

export function LaunchForm({ initialValues, onLaunched, onError }: LaunchFormProps) {
  const [values, setValues] = useState<LaunchFormInitial>(initialValues ?? DEFAULT_VALUES);
  const [errors, setErrors] = useState<FieldErrors>({});
  const [submitted, setSubmitted] = useState(false);

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
  };

  const handleSubmit: React.FormEventHandler<HTMLFormElement> = (e) => {
    e.preventDefault();
    setSubmitted(true);
    const v = validateLaunchForm(values);
    setErrors(v);
    if (Object.keys(v).length > 0) return;

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
      runLocation: values.runLocation as LaunchRequest["runLocation"],
    };
    launchMutation.mutate(req);
  };

  return (
    <Tooltip.Provider>
      <form className="grid grid-cols-1 gap-5 md:grid-cols-2" onSubmit={handleSubmit} noValidate>
        <Field label="Meeting URL" required error={errors.meetingURL}>
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

        <Field label="Participant" required error={errors.participant}>
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

        <Field label="Display name (optional)">
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

        <Field label="TTL" required error={errors.ttl}>
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
                    ? "border-sky-300 bg-sky-100 text-sky-700"
                    : "border-neutral-200 bg-neutral-50 text-neutral-600 hover:bg-neutral-100"
                }`}
              >
                {s}
              </button>
            ))}
          </div>
        </Field>

        <Field label="Network profile" error={errors.network}>
          <Select
            value={values.network}
            onValueChange={(v) => setField("network", v)}
            options={NETSIM_PRESETS.map((p) => ({ value: p, label: p }))}
            testId="network"
          />
        </Field>

        <Field label="Headless">
          <div className="flex items-center gap-3 py-2">
            <Switch.Root
              checked={values.headless}
              onCheckedChange={(v) => setField("headless", v)}
              className="relative h-6 w-10 rounded-full bg-neutral-200 data-[state=checked]:bg-sky-500"
              data-testid="headless"
            >
              <Switch.Thumb className="block h-5 w-5 translate-x-0.5 rounded-full bg-white shadow transition-transform data-[state=checked]:translate-x-4" />
            </Switch.Root>
            <span className="text-sm text-neutral-600">
              {values.headless ? "Chrome will run headless" : "Chrome will run headed (default)"}
            </span>
          </div>
        </Field>

        <Field label="Auth backend">
          <RadioGroup.Root
            value={values.authBackend}
            onValueChange={(v) => setField("authBackend", v as AuthBackend)}
            className="flex flex-col gap-2"
          >
            {AUTH_BACKENDS.map((opt) => (
              <label
                key={opt.value}
                className="flex items-center gap-2 text-sm text-neutral-700"
                htmlFor={`auth-${opt.value}`}
              >
                <RadioGroup.Item
                  id={`auth-${opt.value}`}
                  value={opt.value}
                  className="flex h-4 w-4 items-center justify-center rounded-full border border-neutral-300 bg-white data-[state=checked]:border-sky-500"
                >
                  <RadioGroup.Indicator className="h-2 w-2 rounded-full bg-sky-500" />
                </RadioGroup.Item>
                {opt.label}
              </label>
            ))}
          </RadioGroup.Root>
        </Field>

        {values.authBackend === "storage-state" && (
          <Field label="Storage-state file" error={errors.storageStateFile} required>
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

        <Field label="Run location" error={errors.runLocation}>
          <RadioGroup.Root
            value={values.runLocation}
            onValueChange={(v) => setField("runLocation", v as RunLocation)}
            className="flex flex-col gap-2"
          >
            {RUN_LOCATIONS.map((loc) => (
              <Tooltip.Root key={loc.value} delayDuration={150}>
                <Tooltip.Trigger asChild>
                  <label
                    className={`flex items-center gap-2 text-sm ${
                      loc.available ? "text-neutral-700" : "text-neutral-400"
                    }`}
                    htmlFor={`runloc-${loc.value}`}
                  >
                    <RadioGroup.Item
                      id={`runloc-${loc.value}`}
                      value={loc.value}
                      disabled={!loc.available}
                      className="flex h-4 w-4 items-center justify-center rounded-full border border-neutral-300 bg-white data-[state=checked]:border-sky-500 disabled:bg-neutral-100"
                    >
                      <RadioGroup.Indicator className="h-2 w-2 rounded-full bg-sky-500" />
                    </RadioGroup.Item>
                    {loc.label}
                  </label>
                </Tooltip.Trigger>
                {!loc.available && (
                  <Tooltip.Portal>
                    <Tooltip.Content
                      side="right"
                      sideOffset={6}
                      className="z-50 rounded-md bg-neutral-900 px-2 py-1 text-xs text-white shadow-md"
                    >
                      Future feature — see discussion #793
                      <Tooltip.Arrow className="fill-neutral-900" />
                    </Tooltip.Content>
                  </Tooltip.Portal>
                )}
              </Tooltip.Root>
            ))}
          </RadioGroup.Root>
        </Field>

        <Field label="Fake camera (costume)">
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

        <Field label="Fake mic (audio)">
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

        <div className="md:col-span-2 flex items-center justify-end gap-3 border-t border-neutral-100 pt-4">
          <button
            type="submit"
            disabled={launchMutation.isPending}
            className="inline-flex items-center gap-2 rounded-lg bg-sky-500 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300"
            data-testid="launch-button"
          >
            <Rocket className="h-4 w-4" />
            {launchMutation.isPending ? "Launching…" : "Launch Bot"}
          </button>
        </div>
      </form>
    </Tooltip.Provider>
  );
}

interface FieldProps {
  label: string;
  required?: boolean;
  error?: string;
  children: React.ReactNode;
}

function Field({ label, required, error, children }: FieldProps) {
  return (
    <div className="flex flex-col gap-1.5">
      <label className="text-sm font-medium text-neutral-800">
        {label}
        {required && <span className="ml-0.5 text-red-500">*</span>}
      </label>
      {children}
      {error && (
        <p className="text-xs text-red-600" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
