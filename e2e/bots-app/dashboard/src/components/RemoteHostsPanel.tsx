import { useCallback, useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { Check, Copy, Pencil, PlugZap, Server, Terminal, Trash2, X } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type {
  AddSshHostRequest,
  SshHost,
  SshPreviewHostRequest,
  TestSshHostResponse,
  UpdateSshHostRequest,
} from "../api/types";
import type { ToastEntry } from "./ToastShelf";
import { ConfirmDialog } from "./ConfirmDialog";
import { HelpPopover } from "./ui/HelpPopover";

/**
 * Client-side mirror of the server-side regexes in
 * `e2e/bots-app/src/control/ssh-hosts.ts`. Authoritative validation
 * runs on the server; these duplicates give the operator fast feedback
 * before a network round-trip.
 */
const LABEL_PATTERN = /^[A-Za-z0-9][A-Za-z0-9-]{0,62}$/;
const USER_PATTERN = /^[A-Za-z0-9._-]{1,32}$/;
const HOST_FORBIDDEN_RE = /[\s'"`$;&|<>(){}\\]/;
const SHELL_PATTERN = /^[A-Za-z0-9_/.-]{1,128}$/;
const PROFILE_FILE_PATTERN = /^[~/A-Za-z0-9_./-]{1,256}$/;

/**
 * Debounce window for the live preview fetch. Long enough to absorb
 * keystroke-by-keystroke edits to the form's text inputs; short enough
 * that the preview catches up once the operator pauses typing.
 */
const PREVIEW_DEBOUNCE_MS = 200;

/**
 * Default profile file mirrored from {@link defaultProfileFileForShell}
 * in `ssh-hosts.ts`. Applied client-side as a hint when the operator
 * picks a shell; not enforced server-side (operators can override).
 */
function defaultProfileFileForShell(shell: string): string {
  if (shell === "bash") return "~/.bash_profile";
  if (shell === "zsh") return "~/.zshrc";
  return "";
}

/**
 * Shell choices surfaced as radio options in the Add/Edit Host dialog.
 * `custom` switches the shell input into a free-form text field where
 * the operator can paste an absolute path (e.g. `/opt/homebrew/bin/zsh`).
 */
const SHELL_OPTIONS = [
  { value: "bash", label: "bash" },
  { value: "zsh", label: "zsh" },
  { value: "sh", label: "sh" },
  { value: "custom", label: "Custom path" },
] as const;
type ShellOptionValue = (typeof SHELL_OPTIONS)[number]["value"];

interface RemoteHostsPanelProps {
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

/**
 * "Remote Hosts (SSH)" Tools-page card. Lists every host the
 * operator has registered, with per-row Test / Edit / Delete buttons
 * and a top-right "Add host" button that opens the same Dialog the
 * Edit flow reuses.
 *
 * The full host config is sensitive — hostnames, user names, and
 * absolute key paths are all leaked into this card. Use font-mono for
 * the key-path display so it's obvious operators are looking at a
 * real filesystem path, and never echo the path in a toast.
 */
export function RemoteHostsPanel({ onToast }: RemoteHostsPanelProps) {
  const qc = useQueryClient();
  const hostsQuery = useQuery({
    queryKey: ["ssh", "hosts"],
    queryFn: api.listHosts,
    refetchInterval: 60_000,
  });
  const [addOpen, setAddOpen] = useState(false);
  const [editHost, setEditHost] = useState<SshHost | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<SshHost | null>(null);
  // Per-host test result, keyed by label. `null` slot means "test in
  // flight"; absent slot means "never tested this session".
  const [testResults, setTestResults] = useState<
    Record<string, TestSshHostResponse | null>
  >({});

  const refresh = useCallback(
    () => qc.invalidateQueries({ queryKey: ["ssh", "hosts"] }),
    [qc],
  );

  const addMutation = useMutation({
    mutationFn: (req: AddSshHostRequest) => api.addHost(req),
    onSuccess: (data) => {
      setAddOpen(false);
      refresh();
      onToast({
        title: "Host registered",
        description: `Added "${data.host.label}"`,
        variant: "success",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast({ title: "Add host failed", description: msg, variant: "error" });
    },
  });

  const updateMutation = useMutation({
    mutationFn: ({ label, patch }: { label: string; patch: UpdateSshHostRequest }) =>
      api.updateHost(label, patch),
    onSuccess: (data) => {
      setEditHost(null);
      refresh();
      onToast({
        title: "Host updated",
        description: `Updated "${data.host.label}"`,
        variant: "success",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast({ title: "Update host failed", description: msg, variant: "error" });
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (label: string) => api.removeHost(label),
    onSuccess: (_data, label) => {
      setConfirmDelete(null);
      refresh();
      onToast({
        title: "Host removed",
        description: `Removed "${label}"`,
        variant: "success",
      });
    },
    onError: (err) => {
      setConfirmDelete(null);
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast({ title: "Remove host failed", description: msg, variant: "error" });
    },
  });

  const testMutation = useMutation({
    mutationFn: (label: string) => api.testHost(label),
    onMutate: (label) => {
      setTestResults((prev) => ({ ...prev, [label]: null }));
    },
    onSuccess: (data, label) => {
      setTestResults((prev) => ({ ...prev, [label]: data }));
      if (data.ok) {
        onToast({
          title: `Host "${label}" reachable`,
          description: `Latency ${data.latencyMs ?? "?"}ms`,
          variant: "success",
        });
      } else {
        onToast({
          title: `Host "${label}" unreachable`,
          description: data.error ?? "unknown error",
          variant: "error",
        });
      }
    },
    onError: (err, label) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      setTestResults((prev) => ({ ...prev, [label]: { ok: false, error: msg } }));
      onToast({ title: "Test failed", description: msg, variant: "error" });
    },
  });

  const hosts = hostsQuery.data?.hosts ?? [];

  return (
    <section
      className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
      data-testid="remote-hosts-section"
    >
      <div className="flex items-center justify-between px-6 py-4">
        <div className="flex items-center gap-2">
          <Server className="h-5 w-5 text-sky-500" aria-hidden="true" />
          <div>
            <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
              Remote Hosts (SSH)
            </h2>
            <p className="text-sm text-neutral-500 dark:text-slate-400">
              Hosts the Launch form can target via{" "}
              <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                ssh user@host
              </code>
              . v1: leave/kill + status only — no asset sync, no remote ctl-API proxy.
            </p>
          </div>
          <HelpPopover fieldLabel="Remote Hosts" testId="help-remote-hosts">
            <p>
              Each host row is persisted under{" "}
              <code className="font-mono text-[11px]">&lt;runDir&gt;/hosts.json</code> (mode
              0o600). Credentials are NOT stored — we rely on your local{" "}
              <code className="font-mono text-[11px]">ssh-agent</code> + ~/.ssh/config for
              auth.
            </p>
            <p className="mt-1">
              Click <strong>Test</strong> to probe a host with{" "}
              <code className="font-mono text-[11px]">ssh -o ConnectTimeout=5 ... uname -a</code>
              . The remote bot ttl/leave path also relies on the same local SSH binary.
            </p>
          </HelpPopover>
        </div>
        <button
          type="button"
          onClick={() => setAddOpen(true)}
          className="inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600"
          data-testid="remote-hosts-add"
        >
          <PlugZap className="h-4 w-4" />
          Add host
        </button>
      </div>
      <div className="border-t border-neutral-200 dark:border-slate-700">
        {hostsQuery.isLoading ? (
          <p className="px-6 py-4 text-sm text-neutral-500 dark:text-slate-400">Loading…</p>
        ) : hosts.length === 0 ? (
          <p
            className="px-6 py-4 text-sm text-neutral-500 dark:text-slate-400"
            data-testid="remote-hosts-empty"
          >
            No hosts registered yet. Click <strong>Add host</strong> to register one — the
            Launch form&apos;s SSH-able host option will activate automatically.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-neutral-50 text-xs uppercase tracking-wide text-neutral-500 dark:bg-slate-900 dark:text-slate-400">
                <tr>
                  <th className="px-4 py-2 text-left font-medium">Label</th>
                  <th className="px-4 py-2 text-left font-medium">user@host</th>
                  <th className="px-4 py-2 text-left font-medium">Repos path</th>
                  <th className="px-4 py-2 text-left font-medium">Key</th>
                  <th className="px-4 py-2 text-left font-medium">Last test</th>
                  <th className="px-4 py-2 text-right font-medium">Actions</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-neutral-100 dark:divide-slate-700">
                {hosts.map((h) => {
                  const result = testResults[h.label];
                  return (
                    <tr
                      key={h.label}
                      className="hover:bg-neutral-50 dark:hover:bg-slate-700/50"
                      data-testid={`remote-host-row-${h.label}`}
                    >
                      <td className="px-4 py-2 font-medium text-neutral-800 dark:text-slate-200">
                        {h.label}
                      </td>
                      <td className="px-4 py-2 font-mono text-xs text-neutral-700 dark:text-slate-300">
                        {h.user}@{h.host}
                      </td>
                      <td className="px-4 py-2 font-mono text-xs text-neutral-600 dark:text-slate-400">
                        {h.reposPath}
                      </td>
                      <td className="px-4 py-2 font-mono text-xs text-neutral-600 dark:text-slate-400">
                        {h.sshKey ?? "agent"}
                      </td>
                      <td className="px-4 py-2 text-xs">
                        {result === undefined ? (
                          <span className="text-neutral-400 dark:text-slate-500">—</span>
                        ) : result === null ? (
                          <span className="text-amber-700 dark:text-amber-300">
                            Testing…
                          </span>
                        ) : result.ok ? (
                          <span
                            className="inline-flex rounded-full border border-emerald-200 bg-emerald-100 px-2.5 py-0.5 font-medium text-emerald-800 dark:border-emerald-800 dark:bg-emerald-900/30 dark:text-emerald-300"
                            data-testid={`remote-host-test-ok-${h.label}`}
                          >
                            OK ({result.latencyMs ?? "?"}ms)
                          </span>
                        ) : (
                          <span
                            className="inline-flex rounded-full border border-red-200 bg-red-100 px-2.5 py-0.5 font-medium text-red-800 dark:border-red-800 dark:bg-red-900/30 dark:text-red-300"
                            title={result.error}
                            data-testid={`remote-host-test-fail-${h.label}`}
                          >
                            Fail
                          </span>
                        )}
                      </td>
                      <td className="px-4 py-2">
                        <div className="flex justify-end gap-1">
                          <button
                            type="button"
                            onClick={() => testMutation.mutate(h.label)}
                            disabled={testMutation.isPending && result === null}
                            className="inline-flex h-8 items-center gap-1 rounded-md border border-neutral-200 px-2 text-xs text-neutral-700 hover:bg-neutral-50 disabled:opacity-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
                            data-testid={`remote-host-test-${h.label}`}
                          >
                            Test
                          </button>
                          <button
                            type="button"
                            onClick={() => setEditHost(h)}
                            className="inline-flex h-8 items-center rounded-md border border-neutral-200 px-2 text-xs text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
                            data-testid={`remote-host-edit-${h.label}`}
                          >
                            <Pencil className="mr-1 h-3 w-3" />
                            Edit
                          </button>
                          <button
                            type="button"
                            onClick={() => setConfirmDelete(h)}
                            className="inline-flex h-8 items-center rounded-md border border-red-200 px-2 text-xs text-red-700 hover:bg-red-50 dark:border-red-800 dark:text-red-300 dark:hover:bg-red-900/30"
                            data-testid={`remote-host-delete-${h.label}`}
                          >
                            <Trash2 className="mr-1 h-3 w-3" />
                            Delete
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <HostDialog
        open={addOpen}
        mode="add"
        initial={null}
        submitting={addMutation.isPending}
        onClose={() => setAddOpen(false)}
        onSubmit={(payload) => addMutation.mutate(payload)}
      />
      <HostDialog
        open={editHost !== null}
        mode="edit"
        initial={editHost}
        submitting={updateMutation.isPending}
        onClose={() => setEditHost(null)}
        onSubmit={(payload) => {
          if (!editHost) return;
          const patch: UpdateSshHostRequest = {
            host: payload.host,
            user: payload.user,
            sshKey: payload.sshKey ?? null,
            reposPath: payload.reposPath,
            notes: payload.notes ?? null,
            shell: payload.shell ?? null,
            profileFile: payload.profileFile ?? null,
            preCommand: payload.preCommand ?? null,
          };
          updateMutation.mutate({ label: editHost.label, patch });
        }}
      />

      {confirmDelete && (
        <ConfirmDialog
          open={true}
          title="Remove SSH host?"
          body={`The registry entry "${confirmDelete.label}" will be removed. Any currently-running bots on that host will keep running, but the Launch form will no longer offer it.`}
          confirmLabel={deleteMutation.isPending ? "Removing…" : "Remove"}
          destructive
          onCancel={() => setConfirmDelete(null)}
          onConfirm={() => deleteMutation.mutate(confirmDelete.label)}
        />
      )}
    </section>
  );
}

interface HostDialogProps {
  open: boolean;
  mode: "add" | "edit";
  initial: SshHost | null;
  submitting: boolean;
  onClose: () => void;
  onSubmit: (payload: AddSshHostRequest) => void;
}

function HostDialog({ open, mode, initial, submitting, onClose, onSubmit }: HostDialogProps) {
  const [label, setLabel] = useState(initial?.label ?? "");
  const [host, setHost] = useState(initial?.host ?? "");
  const [user, setUser] = useState(initial?.user ?? "");
  const [sshKey, setSshKey] = useState(initial?.sshKey ?? "");
  const [reposPath, setReposPath] = useState(initial?.reposPath ?? "");
  const [notes, setNotes] = useState(initial?.notes ?? "");
  // The shell selection is a radio between bare names + a `custom`
  // mode that exposes a free-form text input. We track the radio
  // choice and the custom-path string separately so toggling between
  // `custom` and a bare name doesn't clobber the operator's pasted
  // path.
  const [shellChoice, setShellChoice] = useState<ShellOptionValue>("bash");
  const [shellCustom, setShellCustom] = useState("");
  const [profileFile, setProfileFile] = useState("");
  const [preCommand, setPreCommand] = useState("");
  const [error, setError] = useState<string | null>(null);

  // Re-seed inputs when the dialog opens or when the row changes. We
  // depend on the whole `initial` object reference so eslint is happy;
  // because the parent only mutates `initial` when the operator picks
  // a different row, this effectively re-fires on open-cycle and on
  // row-switch within an open dialog.
  useEffect(() => {
    if (!open) return;
    setLabel(initial?.label ?? "");
    setHost(initial?.host ?? "");
    setUser(initial?.user ?? "");
    setSshKey(initial?.sshKey ?? "");
    setReposPath(initial?.reposPath ?? "");
    setNotes(initial?.notes ?? "");
    // Map the stored shell string back to one of the radio options.
    // Bare names that match a built-in choice land on that radio;
    // anything else (including absolute paths) lands on `custom`.
    const storedShell = initial?.shell ?? "";
    if (storedShell === "" || storedShell === "bash") {
      setShellChoice("bash");
      setShellCustom("");
    } else if (storedShell === "zsh") {
      setShellChoice("zsh");
      setShellCustom("");
    } else if (storedShell === "sh") {
      setShellChoice("sh");
      setShellCustom("");
    } else {
      setShellChoice("custom");
      setShellCustom(storedShell);
    }
    // Add mode (initial === null): pre-populate with the bash default
    // hint (~/.bash_profile) so the live preview reflects something
    // sensible on first open. Edit mode: respect whatever the row had,
    // including a null/empty profileFile (operator may have explicitly
    // cleared it).
    if (initial === null) {
      setProfileFile(defaultProfileFileForShell("bash"));
    } else {
      setProfileFile(initial.profileFile ?? "");
    }
    setPreCommand(initial?.preCommand ?? "");
    setError(null);
  }, [open, initial]);

  // Resolve the current shell name from the radio + custom-path state.
  // Returns `null` for an empty custom-path slot so we don't post an
  // empty string the server would reject.
  const currentShell = useMemo<string | null>(() => {
    if (shellChoice === "custom") {
      return shellCustom.trim() === "" ? null : shellCustom.trim();
    }
    return shellChoice;
  }, [shellChoice, shellCustom]);

  // Hint the canonical profile file when the operator clicks a built-in
  // shell radio AND the profileFile slot is still empty. Handled inline
  // on the radio's onChange (not as a `useEffect`) so the hint only fires
  // in response to a real user click — not as a side effect of the
  // dialog opening on a row that already has a non-empty profileFile.
  const hintProfileFileFor = useCallback(
    (choice: ShellOptionValue) => {
      if (choice === "custom") return;
      if (profileFile !== "") return;
      const hint = defaultProfileFileForShell(choice);
      if (hint !== "") setProfileFile(hint);
    },
    [profileFile],
  );

  const handleSubmit: React.FormEventHandler<HTMLFormElement> = (e) => {
    e.preventDefault();
    const trimmedLabel = label.trim();
    const trimmedHost = host.trim();
    const trimmedUser = user.trim();
    const trimmedReposPath = reposPath.trim();
    const trimmedSshKey = sshKey.trim();
    const trimmedProfileFile = profileFile.trim();
    const trimmedPreCommand = preCommand.trim();
    const resolvedShell = currentShell; // already trimmed

    if (mode === "add" && !LABEL_PATTERN.test(trimmedLabel)) {
      setError("Label must be alphanumeric + hyphen, 1–63 chars, no leading hyphen.");
      return;
    }
    if (trimmedHost === "" || HOST_FORBIDDEN_RE.test(trimmedHost)) {
      setError("Host must be non-empty and free of whitespace/shell metacharacters.");
      return;
    }
    if (trimmedUser !== "" && !USER_PATTERN.test(trimmedUser)) {
      setError("User must be 1–32 alphanumerics, '.', '_', or '-'.");
      return;
    }
    if (trimmedReposPath === "") {
      setError("Repos path is required.");
      return;
    }
    if (!trimmedReposPath.startsWith("/") && !trimmedReposPath.startsWith("~")) {
      setError("Repos path must be an absolute path (start with '/' or '~').");
      return;
    }
    if (
      trimmedSshKey !== "" &&
      !trimmedSshKey.startsWith("/") &&
      !trimmedSshKey.startsWith("~")
    ) {
      setError("SSH key must be an absolute path (or leave empty to use ssh-agent).");
      return;
    }
    if (resolvedShell !== null && !SHELL_PATTERN.test(resolvedShell)) {
      setError(
        "Shell must match a-z, 0-9, '_', '/', '.', '-' (max 128 chars). No metacharacters.",
      );
      return;
    }
    if (trimmedProfileFile !== "" && !PROFILE_FILE_PATTERN.test(trimmedProfileFile)) {
      setError("Profile file must be a `~`-prefixed or absolute path; no metacharacters.");
      return;
    }
    if (trimmedPreCommand.length > 512) {
      setError("Pre-command too long (max 512 chars).");
      return;
    }
    if (/[\r\n]/.test(trimmedPreCommand)) {
      setError("Pre-command must not contain newlines.");
      return;
    }
    setError(null);
    onSubmit({
      label: trimmedLabel,
      host: trimmedHost,
      user: trimmedUser === "" ? undefined : trimmedUser,
      sshKey: trimmedSshKey === "" ? null : trimmedSshKey,
      reposPath: trimmedReposPath,
      notes: notes.trim() === "" ? null : notes.trim(),
      shell: resolvedShell,
      profileFile: trimmedProfileFile === "" ? null : trimmedProfileFile,
      preCommand: trimmedPreCommand === "" ? null : trimmedPreCommand,
    });
  };

  return (
    <Dialog.Root open={open} onOpenChange={(o) => !o && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(95vw,640px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white p-6 shadow-xl focus:outline-none dark:border-slate-700 dark:bg-slate-800"
          data-testid="remote-host-dialog"
        >
          <div className="flex items-start justify-between">
            <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
              {mode === "add" ? "Add SSH host" : `Edit host "${initial?.label}"`}
            </Dialog.Title>
            <Dialog.Close className="rounded p-1 text-neutral-400 hover:bg-neutral-100 dark:text-slate-500 dark:hover:bg-slate-700">
              <X className="h-4 w-4" />
            </Dialog.Close>
          </div>

          <form className="mt-4 grid grid-cols-1 gap-4 md:grid-cols-2" onSubmit={handleSubmit}>
            <DialogField
              label="Label"
              required
              testIdSuffix="label"
              help={
                <HelpPopover fieldLabel="Label" testId="help-label">
                  <p>
                    A short identifier for this host (alphanumeric + hyphen). Used everywhere
                    in the dashboard to refer to this machine.
                  </p>
                  <p className="mt-1">
                    Example:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      lab-mac-1
                    </code>
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                disabled={mode === "edit"}
                placeholder="lab-mini-7"
                className={DIALOG_INPUT_CLASS}
                data-testid="remote-host-dialog-label"
              />
            </DialogField>

            <DialogField
              label="Host / IP"
              required
              testIdSuffix="host"
              help={
                <HelpPopover fieldLabel="Host" testId="help-host">
                  <p>
                    DNS name or IP of the remote machine. Optionally include the SSH port
                    after a colon.
                  </p>
                  <p className="mt-1">
                    Example:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      192.168.1.20
                    </code>{" "}
                    or{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      my-host.lan:2222
                    </code>
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="lab-mini-7.intra or 10.0.0.5:2222"
                className={DIALOG_INPUT_CLASS}
                data-testid="remote-host-dialog-host"
              />
            </DialogField>

            <DialogField
              label="User"
              testIdSuffix="user"
              help={
                <HelpPopover fieldLabel="User" testId="help-user">
                  <p>
                    Username on the remote machine. The dashboard will SSH in as this user.
                  </p>
                  <p className="mt-1">
                    Example:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      alice
                    </code>
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={user}
                onChange={(e) => setUser(e.target.value)}
                placeholder="Default: $USER"
                className={DIALOG_INPUT_CLASS}
                data-testid="remote-host-dialog-user"
              />
            </DialogField>

            <DialogField
              label="SSH key (optional)"
              testIdSuffix="sshKey"
              help={
                <HelpPopover fieldLabel="SSH Key" testId="help-sshKey">
                  <p>
                    Optional. Absolute path to a private key file. Leave blank to use your
                    system SSH agent +{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      ~/.ssh/config
                    </code>{" "}
                    defaults (recommended).
                  </p>
                  <p className="mt-1">
                    Example:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      /Users/alice/.ssh/id_ed25519
                    </code>
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={sshKey}
                onChange={(e) => setSshKey(e.target.value)}
                placeholder="/home/alice/.ssh/id_ed25519 (or leave empty for ssh-agent)"
                className={`${DIALOG_INPUT_CLASS} font-mono`}
                data-testid="remote-host-dialog-sshKey"
              />
            </DialogField>

            <DialogField
              label="Repos path"
              required
              testIdSuffix="reposPath"
              colSpan={2}
              help={
                <HelpPopover fieldLabel="Repo path" testId="help-reposPath">
                  <p>
                    Absolute path on the remote machine where the videocall repository is
                    checked out. The dashboard runs{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      cd &lt;reposPath&gt;/e2e &amp;&amp; npm run bot -- run ...
                    </code>{" "}
                    there.
                  </p>
                  <p className="mt-1">
                    Example:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      /home/alice/videocall
                    </code>
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={reposPath}
                onChange={(e) => setReposPath(e.target.value)}
                placeholder="/home/alice/videocall"
                className={`${DIALOG_INPUT_CLASS} font-mono`}
                data-testid="remote-host-dialog-reposPath"
              />
            </DialogField>

            <DialogField
              label="Notes (optional)"
              testIdSuffix="notes"
              colSpan={2}
              help={
                <HelpPopover fieldLabel="Notes" testId="help-notes">
                  <p>
                    Optional free-form notes about this host — e.g. machine specs, network
                    role, who owns it. Visible only in the dashboard.
                  </p>
                </HelpPopover>
              }
            >
              <textarea
                value={notes}
                onChange={(e) => setNotes(e.target.value)}
                placeholder="Mac mini in the rack near the printer"
                rows={2}
                className={DIALOG_INPUT_CLASS}
                data-testid="remote-host-dialog-notes"
              />
            </DialogField>

            <DialogField
              label="Shell"
              testIdSuffix="shell"
              colSpan={2}
              help={
                <HelpPopover fieldLabel="Shell" testId="help-shell">
                  <p>
                    Shell to use on the remote host for the bot launch. Default:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      bash
                    </code>{" "}
                    (good for nvm + homebrew). Pick{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      zsh
                    </code>{" "}
                    if your PATH lives only in{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      ~/.zshrc
                    </code>
                    , or specify an absolute path.
                  </p>
                </HelpPopover>
              }
            >
              <div
                className="flex flex-wrap items-center gap-3"
                data-testid="remote-host-dialog-shell"
                role="radiogroup"
              >
                {SHELL_OPTIONS.map((opt) => (
                  <label
                    key={opt.value}
                    className="inline-flex items-center gap-1 text-sm text-neutral-700 dark:text-slate-200"
                  >
                    <input
                      type="radio"
                      name="shell-choice"
                      value={opt.value}
                      checked={shellChoice === opt.value}
                      onChange={() => {
                        setShellChoice(opt.value);
                        hintProfileFileFor(opt.value);
                      }}
                      data-testid={`remote-host-dialog-shell-${opt.value}`}
                    />
                    <span>{opt.label}</span>
                  </label>
                ))}
                {shellChoice === "custom" && (
                  <input
                    type="text"
                    value={shellCustom}
                    onChange={(e) => setShellCustom(e.target.value)}
                    placeholder="/opt/homebrew/bin/zsh"
                    className={`${DIALOG_INPUT_CLASS} font-mono ml-1 w-72`}
                    data-testid="remote-host-dialog-shell-custom-path"
                  />
                )}
              </div>
            </DialogField>

            <DialogField
              label="Profile file (optional)"
              testIdSuffix="profileFile"
              colSpan={2}
              help={
                <HelpPopover fieldLabel="Profile file" testId="help-profileFile">
                  <p>
                    Profile file the remote shell will source before running the bot. The
                    default depends on the chosen shell (
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      ~/.bash_profile
                    </code>{" "}
                    for bash,{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      ~/.zshrc
                    </code>{" "}
                    for zsh). The file is sourced with{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      [ -f &lt;path&gt; ] &amp;&amp; . &lt;path&gt;
                    </code>{" "}
                    so a missing file is a silent no-op.
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={profileFile}
                onChange={(e) => setProfileFile(e.target.value)}
                placeholder="~/.bash_profile  (or ~/.zshrc / /etc/profile)"
                className={`${DIALOG_INPUT_CLASS} font-mono`}
                data-testid="remote-host-dialog-profileFile"
              />
            </DialogField>

            <DialogField
              label="Pre-command (optional)"
              testIdSuffix="preCommand"
              colSpan={2}
              help={
                <HelpPopover fieldLabel="Pre-command" testId="help-preCommand">
                  <p>
                    Optional. Commands to run on the remote AFTER sourcing the profile but
                    BEFORE the bot launch. Useful for nvm version pinning:{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      . ~/.nvm/nvm.sh &amp;&amp; nvm use 22
                    </code>
                    . Terminated with{" "}
                    <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                      ;
                    </code>{" "}
                    so it can&apos;t abort the launch.
                  </p>
                </HelpPopover>
              }
            >
              <input
                type="text"
                value={preCommand}
                onChange={(e) => setPreCommand(e.target.value)}
                placeholder=". ~/.nvm/nvm.sh && nvm use 22"
                className={`${DIALOG_INPUT_CLASS} font-mono`}
                data-testid="remote-host-dialog-preCommand"
              />
            </DialogField>

            <div className="md:col-span-2">
              <SampleCommandPreview
                label={label.trim()}
                host={host.trim()}
                user={user.trim()}
                sshKey={sshKey.trim()}
                reposPath={reposPath.trim()}
                shell={currentShell}
                profileFile={profileFile.trim()}
                preCommand={preCommand.trim()}
              />
            </div>

            {error && (
              <p
                className="md:col-span-2 text-xs text-red-600 dark:text-red-400"
                role="alert"
                data-testid="remote-host-dialog-error"
              >
                {error}
              </p>
            )}

            <div className="md:col-span-2 flex justify-end gap-2">
              <button
                type="button"
                onClick={onClose}
                className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
              >
                Cancel
              </button>
              <button
                type="submit"
                disabled={submitting}
                className="inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300"
                data-testid="remote-host-dialog-submit"
              >
                {submitting ? "Saving…" : mode === "add" ? "Add host" : "Save changes"}
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

const DIALOG_INPUT_CLASS =
  "w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm placeholder:text-neutral-400 focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 disabled:cursor-not-allowed disabled:bg-neutral-100 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 dark:placeholder:text-slate-500 dark:disabled:bg-slate-700";

interface DialogFieldProps {
  label: string;
  required?: boolean;
  testIdSuffix: string;
  colSpan?: 1 | 2;
  /**
   * Optional help-popover trigger rendered next to the label. Mirrors
   * the `help` slot on {@link LaunchForm}'s {@link Field} component so
   * each field can carry a `(?)` info button with field-specific copy.
   */
  help?: React.ReactNode;
  children: React.ReactNode;
}

function DialogField({
  label,
  required,
  testIdSuffix,
  colSpan = 1,
  help,
  children,
}: DialogFieldProps) {
  return (
    <div className={`flex flex-col gap-1.5 ${colSpan === 2 ? "md:col-span-2" : ""}`}>
      <div className="flex items-center gap-1.5">
        <label
          className="text-sm font-medium text-neutral-800 dark:text-slate-200"
          data-testid={`remote-host-dialog-field-${testIdSuffix}`}
        >
          {label}
          {required && <span className="ml-0.5 text-red-500 dark:text-red-400">*</span>}
        </label>
        {help}
      </div>
      {children}
    </div>
  );
}

interface SampleCommandPreviewProps {
  label: string;
  host: string;
  user: string;
  sshKey: string;
  reposPath: string;
  shell: string | null;
  profileFile: string;
  preCommand: string;
}

/**
 * Live "Sample command" card embedded in the Add/Edit Host dialog.
 * Posts the current form values to `POST /api/hosts/preview` (debounced
 * 200ms) and renders the resulting `display` string in a copy-able
 * monospace block.
 *
 * The preview uses the new `/hosts/preview` endpoint (which doesn't
 * require the host to be saved first) so the operator sees their
 * unsaved edits as they type. Placeholder tokens (`<participant>`,
 * `<meeting-url>`, etc.) are visible in the rendered command so the
 * operator understands those slots get filled at launch time.
 */
function SampleCommandPreview({
  label,
  host,
  user,
  sshKey,
  reposPath,
  shell,
  profileFile,
  preCommand,
}: SampleCommandPreviewProps) {
  // Build the request payload up-front so the dependency for the
  // debounce effect is a stable object reference.
  const payload = useMemo<SshPreviewHostRequest | null>(() => {
    // The server rejects empty `host` / `reposPath`; bail out early
    // so we don't issue a doomed fetch on every keystroke.
    if (host === "" || reposPath === "") return null;
    return {
      host: {
        // Server-side validation requires a label even though the
        // preview endpoint doesn't persist anything. Use a stable
        // sentinel when the operator hasn't typed one yet so the
        // server's label regex still passes.
        label: label === "" ? "preview" : label,
        host,
        user: user === "" ? undefined : user,
        sshKey: sshKey === "" ? null : sshKey,
        reposPath,
        shell,
        profileFile: profileFile === "" ? null : profileFile,
        preCommand: preCommand === "" ? null : preCommand,
      },
    };
  }, [label, host, user, sshKey, reposPath, shell, profileFile, preCommand]);

  // Debounce the payload — bumped after the 200ms quiet window
  // expires. TanStack Query keys off the debounced value so a burst
  // of keystrokes coalesces into a single fetch.
  const [debounced, setDebounced] = useState<SshPreviewHostRequest | null>(payload);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(payload), PREVIEW_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [payload]);

  const previewQuery = useQuery({
    // Stringify the payload for the cache key — TanStack Query's
    // structural equality already handles the nested object, but
    // making the key explicit keeps the devtools output readable.
    queryKey: ["ssh", "hosts", "preview", debounced],
    queryFn: () => api.previewSshHost(debounced as SshPreviewHostRequest),
    enabled: debounced !== null,
    staleTime: 0,
    retry: false,
  });

  const [copied, setCopied] = useState<boolean>(false);
  useEffect(() => {
    setCopied(false);
  }, [previewQuery.data]);

  const display = previewQuery.data?.display ?? "";
  const errorMsg =
    previewQuery.error instanceof DashboardApiError
      ? previewQuery.error.message
      : previewQuery.error
        ? (previewQuery.error as Error).message
        : null;

  const handleCopy = async (): Promise<void> => {
    if (!display) return;
    try {
      await navigator.clipboard.writeText(display);
      setCopied(true);
      setTimeout(() => setCopied(false), 2_000);
    } catch {
      // Clipboard rejections are rare in production (HTTPS + secure
      // contexts on the dashboard) but still possible in some embed
      // scenarios; the operator can re-select the text manually.
    }
  };

  return (
    <div
      className="rounded-lg border border-neutral-200 bg-neutral-50/40 p-3 text-sm dark:border-slate-700 dark:bg-slate-900/30"
      data-testid="remote-host-dialog-sample-cmd"
    >
      <div className="flex items-center gap-2">
        <Terminal
          className="h-4 w-4 text-neutral-500 dark:text-slate-400"
          aria-hidden="true"
        />
        <span className="text-sm font-medium text-neutral-800 dark:text-slate-200">
          Sample command
        </span>
        <HelpPopover fieldLabel="Sample command" testId="help-sample-cmd">
          <p>
            Shows the exact <code className="font-mono text-[11px]">ssh</code> command the
            dashboard will run to launch a bot on this host.
          </p>
          <p className="mt-1">
            Placeholders (
            <code className="font-mono text-[11px]">&lt;participant&gt;</code>,{" "}
            <code className="font-mono text-[11px]">&lt;meeting-url&gt;</code>, etc.) are
            filled in from the Launch form at runtime.
          </p>
        </HelpPopover>
      </div>
      <div className="mt-2 flex flex-col gap-2">
        {payload === null ? (
          <p
            className="text-xs text-neutral-500 dark:text-slate-400"
            data-testid="remote-host-dialog-sample-cmd-empty"
          >
            Fill in <strong>Host</strong> + <strong>Repos path</strong> to see the preview.
          </p>
        ) : previewQuery.isPending ? (
          <p
            className="text-xs text-neutral-500 dark:text-slate-400"
            data-testid="remote-host-dialog-sample-cmd-loading"
          >
            Building preview…
          </p>
        ) : errorMsg !== null ? (
          <p
            className="rounded-md border border-red-200 bg-red-50 px-2 py-1 text-xs text-red-700 dark:border-red-900/40 dark:bg-red-900/20 dark:text-red-300"
            role="alert"
            data-testid="remote-host-dialog-sample-cmd-error"
          >
            {errorMsg}
          </p>
        ) : display !== "" ? (
          <>
            <pre
              className="max-h-48 overflow-auto rounded-md bg-neutral-900 px-3 py-2 font-mono text-[11px] leading-relaxed text-slate-100 dark:bg-slate-950"
              data-testid="remote-host-dialog-sample-cmd-display"
            >
              {display}
            </pre>
            <div className="flex items-center justify-end">
              <button
                type="button"
                onClick={handleCopy}
                className="inline-flex items-center gap-1 rounded-md border border-neutral-300 bg-white px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
                data-testid="remote-host-dialog-sample-cmd-copy"
                aria-label="Copy sample command to clipboard"
              >
                {copied ? (
                  <>
                    <Check className="h-3 w-3" aria-hidden="true" />
                    Copied
                  </>
                ) : (
                  <>
                    <Copy className="h-3 w-3" aria-hidden="true" />
                    Copy
                  </>
                )}
              </button>
            </div>
          </>
        ) : null}
      </div>
    </div>
  );
}
