import { useRef, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { FileUp, Loader2, Play, Upload, Wand2 } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type {
  LaunchFromConfigPreviewResponse,
  LaunchFromConfigResponse,
} from "../api/types";
import type { ToastEntry } from "./ToastShelf";
import { HelpPopover } from "./ui/HelpPopover";

interface ConfigImportPanelProps {
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

const EXAMPLE_YAML = `meeting_url: https://app.videocall.fnxlabs.com/meeting/TonyBots
ttl: 5m
network: none
bots:
  - participant: alice
  - participant: bob
  - participant: carol
`;

/**
 * "Import YAML config" card for the Tools page. Mirrors the CLI's
 * `bots-app run --config <path>` flow: paste (or upload) a meeting
 * config YAML, optionally preview the parsed shape, then launch the
 * full fleet with one click. The server's parser is the source of
 * truth — the dashboard just hands it the raw YAML string.
 */
export function ConfigImportPanel({ onToast }: ConfigImportPanelProps) {
  const [yaml, setYaml] = useState("");
  const [preview, setPreview] = useState<LaunchFromConfigPreviewResponse | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);

  const previewMutation = useMutation({
    mutationFn: (configYaml: string) => api.previewFromConfig(configYaml),
    onSuccess: (data) => {
      setPreview(data);
      setPreviewError(null);
    },
    onError: (err) => {
      setPreview(null);
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      setPreviewError(msg);
    },
  });

  const launchMutation = useMutation({
    mutationFn: (configYaml: string) => api.launchFromConfig({ configYaml }),
    onSuccess: (data: LaunchFromConfigResponse) => {
      const launched = data.botIds.length;
      onToast({
        title:
          launched === data.count
            ? `Launched ${launched} bots from config`
            : `Launched ${launched}/${data.count} bots from config`,
        description: `meeting: ${data.meetingUrl}`,
        variant: data.errors.length > 0 ? "info" : "success",
      });
      if (data.errors.length > 0) {
        for (const err of data.errors) {
          onToast({
            title: `Skipped bot ${err.participant ?? `[${err.index}]`}`,
            description: err.message,
            variant: "error",
          });
        }
      }
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast({ title: "Launch from config failed", description: msg, variant: "error" });
    },
  });

  const handleFileChange: React.ChangeEventHandler<HTMLInputElement> = (e) => {
    const file = e.target.files?.[0];
    if (!file) return;
    file.text().then((text) => {
      setYaml(text);
      setPreview(null);
      setPreviewError(null);
    });
    // Reset so picking the same file again re-fires onChange.
    e.target.value = "";
  };

  return (
    <section
      className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
      data-testid="config-import-section"
    >
      <div className="flex items-center gap-2 px-6 py-4">
        <FileUp className="h-5 w-5 text-sky-500" aria-hidden="true" />
        <div className="flex-1">
          <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
            Import YAML config
          </h2>
          <p className="text-sm text-neutral-500 dark:text-slate-400">
            Paste or upload a meeting-config YAML and launch all bots from it. Matches the
            CLI&apos;s{" "}
            <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
              bots-app run --config &lt;path&gt;
            </code>
            .
          </p>
        </div>
        <HelpPopover fieldLabel="YAML config import" testId="help-config-import">
          <p>The YAML must include a top-level meeting_url and a bots[] list.</p>
          <p className="mt-1">Per-bot ttl, network, and auth fields override the meeting-level defaults.</p>
          <p className="mt-1">
            Generate a starting template with{" "}
            <code className="font-mono text-[11px]">bots-app gen --count N --seed S</code>.
          </p>
        </HelpPopover>
      </div>
      <div className="border-t border-neutral-200 px-6 py-5 dark:border-slate-700">
        <div className="flex flex-col gap-4">
          <div className="flex items-center justify-between">
            <label
              htmlFor="config-yaml"
              className="text-sm font-medium text-neutral-800 dark:text-slate-200"
            >
              YAML
            </label>
            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={() => fileInputRef.current?.click()}
                className="inline-flex items-center gap-1 rounded-md border border-neutral-300 px-2.5 py-1 text-xs text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
                data-testid="config-import-upload"
              >
                <Upload className="h-3 w-3" />
                Upload file
              </button>
              <button
                type="button"
                onClick={() => {
                  setYaml(EXAMPLE_YAML);
                  setPreview(null);
                  setPreviewError(null);
                }}
                className="inline-flex items-center gap-1 rounded-md border border-neutral-300 px-2.5 py-1 text-xs text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
                data-testid="config-import-example"
              >
                <Wand2 className="h-3 w-3" />
                Insert example
              </button>
              <input
                ref={fileInputRef}
                type="file"
                accept=".yaml,.yml,application/yaml,text/yaml,text/plain"
                onChange={handleFileChange}
                className="hidden"
                data-testid="config-import-file"
              />
            </div>
          </div>

          <textarea
            id="config-yaml"
            value={yaml}
            onChange={(e) => {
              setYaml(e.target.value);
              setPreview(null);
              setPreviewError(null);
            }}
            spellCheck={false}
            rows={12}
            placeholder={`# Example:\n${EXAMPLE_YAML}`}
            className="w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 font-mono text-xs text-neutral-900 shadow-sm placeholder:text-neutral-400 focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 dark:placeholder:text-slate-500"
            data-testid="config-import-textarea"
          />

          {previewError && (
            <p
              className="rounded-md border border-red-200 bg-red-50 p-3 text-xs text-red-700 dark:border-red-800 dark:bg-red-900/30 dark:text-red-300"
              role="alert"
              data-testid="config-import-preview-error"
            >
              {previewError}
            </p>
          )}

          {preview && (
            <div
              className="rounded-md border border-neutral-200 bg-neutral-50 p-3 text-xs text-neutral-700 dark:border-slate-600 dark:bg-slate-900/40 dark:text-slate-200"
              data-testid="config-import-preview"
            >
              <p className="font-semibold">Preview</p>
              <dl className="mt-2 grid grid-cols-[8rem_1fr] gap-x-3 gap-y-1">
                <dt className="text-neutral-500 dark:text-slate-400">Meeting URL</dt>
                <dd className="break-all font-mono text-[11px]">{preview.meetingUrl}</dd>
                <dt className="text-neutral-500 dark:text-slate-400">Default TTL</dt>
                <dd>{preview.ttl ?? "(none)"}</dd>
                <dt className="text-neutral-500 dark:text-slate-400">Default network</dt>
                <dd>{preview.network ?? "(none)"}</dd>
                <dt className="text-neutral-500 dark:text-slate-400">Default auth</dt>
                <dd>{preview.auth ?? "(none)"}</dd>
                <dt className="text-neutral-500 dark:text-slate-400">Bot count</dt>
                <dd data-testid="config-import-preview-count">{preview.botCount}</dd>
              </dl>
              <details className="mt-2">
                <summary className="cursor-pointer select-none text-[11px] text-neutral-500 hover:text-neutral-700 dark:text-slate-400 dark:hover:text-slate-200">
                  Show bot list
                </summary>
                <ul className="mt-2 max-h-40 overflow-y-auto rounded border border-neutral-200 bg-white p-2 dark:border-slate-700 dark:bg-slate-800">
                  {preview.bots.map((b, i) => (
                    <li
                      key={`${i}-${b.participant}`}
                      className="font-mono text-[11px] text-neutral-700 dark:text-slate-200"
                    >
                      [{i}] {b.participant}
                      {b.ttl ? ` ttl=${b.ttl}` : ""}
                      {b.network ? ` net=${b.network}` : ""}
                      {b.auth ? ` auth=${b.auth}` : ""}
                    </li>
                  ))}
                </ul>
              </details>
            </div>
          )}

          <div className="flex items-center justify-end gap-2 border-t border-neutral-100 pt-4 dark:border-slate-700">
            <button
              type="button"
              onClick={() => previewMutation.mutate(yaml)}
              disabled={yaml.trim() === "" || previewMutation.isPending}
              className="inline-flex items-center gap-2 rounded-md border border-neutral-300 px-3 py-1.5 text-sm font-medium text-neutral-700 shadow-sm hover:bg-neutral-50 disabled:cursor-not-allowed disabled:bg-neutral-50 disabled:text-neutral-400 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700 dark:disabled:bg-slate-800 dark:disabled:text-slate-500"
              data-testid="config-import-preview-button"
            >
              {previewMutation.isPending && (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              )}
              Preview
            </button>
            <button
              type="button"
              onClick={() => launchMutation.mutate(yaml)}
              disabled={yaml.trim() === "" || launchMutation.isPending}
              className="inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600"
              data-testid="config-import-launch-button"
            >
              {launchMutation.isPending ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <Play className="h-3.5 w-3.5" />
              )}
              {launchMutation.isPending ? "Launching…" : "Launch all"}
            </button>
          </div>
        </div>
      </div>
    </section>
  );
}
