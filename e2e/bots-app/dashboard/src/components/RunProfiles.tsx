import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { Bookmark, Play, Save, Trash2 } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { ProfileSummary } from "../api/types";
import { useFieldHistory } from "../lib/fieldHistory";
import type { ToastEntry } from "./ToastShelf";
import { ConfirmDialog } from "./ConfirmDialog";

interface RunProfilesProps {
  /** True when at least one bot is in the orchestrator's registry. */
  hasBots: boolean;
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

export function RunProfiles({ hasBots, onToast }: RunProfilesProps) {
  const qc = useQueryClient();
  const profilesQuery = useQuery({
    queryKey: ["profiles"],
    queryFn: api.listProfiles,
    refetchInterval: 10_000,
  });
  const refresh = () => qc.invalidateQueries({ queryKey: ["profiles"] });

  const [showSave, setShowSave] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<ProfileSummary | null>(null);

  const launch = useMutation({
    mutationFn: (name: string) => api.launchProfile(name),
    onSuccess: (data) => {
      onToast({
        title: `Launched profile "${data.name}"`,
        description: `${data.botIds.length} bot(s) starting up.`,
        variant: "success",
      });
      qc.invalidateQueries({ queryKey: ["bots"] });
    },
    onError: (err) =>
      onToast({
        title: "Profile launch failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });

  const remove = useMutation({
    mutationFn: (name: string) => api.deleteProfile(name),
    onSuccess: (data) => {
      onToast({ title: `Profile "${data.name}" deleted`, variant: "success" });
      refresh();
    },
    onError: (err) =>
      onToast({
        title: "Delete failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });

  return (
    <section
      aria-label="Run Profiles"
      className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
      data-testid="run-profiles"
    >
      <div className="flex items-center justify-between px-6 py-4">
        <div>
          <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
            Run Profiles
          </h2>
          <p className="text-sm text-neutral-500 dark:text-slate-400">
            Save the current set of bots, then re-launch the whole group with one click.
          </p>
        </div>
        <button
          type="button"
          onClick={() => {
            if (!hasBots) {
              onToast({
                title: "No bots to save",
                description: "Launch some first, then come back here.",
                variant: "info",
              });
              return;
            }
            setShowSave(true);
          }}
          className="inline-flex items-center gap-2 rounded-lg border border-neutral-200 bg-white px-3 py-1.5 text-sm font-medium text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
          data-testid="run-profiles-save-button"
        >
          <Save className="h-4 w-4" />
          Save current as profile
        </button>
      </div>
      <div className="border-t border-neutral-200 dark:border-slate-700">
        {profilesQuery.isLoading ? (
          <div className="px-6 py-6 text-sm text-neutral-500 dark:text-slate-400">
            Loading profiles…
          </div>
        ) : (profilesQuery.data?.profiles ?? []).length === 0 ? (
          <div
            className="px-6 py-6 text-sm text-neutral-500 dark:text-slate-400"
            data-testid="run-profiles-empty"
          >
            No saved profiles yet.
          </div>
        ) : (
          <ul className="divide-y divide-neutral-100 dark:divide-slate-700">
            {(profilesQuery.data?.profiles ?? []).map((profile) => (
              <li
                key={profile.name}
                className="flex items-center gap-3 px-6 py-3"
                data-testid={`run-profile-row-${profile.name}`}
              >
                <Bookmark className="h-4 w-4 text-sky-500 dark:text-sky-400" />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-medium text-neutral-900 dark:text-slate-100">
                    {profile.name}
                  </p>
                  <p className="text-xs text-neutral-500 dark:text-slate-400">
                    {profile.botCount} bot{profile.botCount === 1 ? "" : "s"} ·{" "}
                    saved {formatSavedAt(profile.savedAt)}
                  </p>
                </div>
                <button
                  type="button"
                  onClick={() => launch.mutate(profile.name)}
                  disabled={launch.isPending}
                  className="inline-flex items-center gap-1 rounded-md bg-sky-500 px-2.5 py-1 text-sm font-medium text-white hover:bg-sky-600 disabled:opacity-50"
                  data-testid={`run-profile-launch-${profile.name}`}
                >
                  <Play className="h-3.5 w-3.5" />
                  Launch
                </button>
                <button
                  type="button"
                  onClick={() => setConfirmDelete(profile)}
                  className="inline-flex items-center rounded-md border border-red-200 bg-white p-1.5 text-red-600 hover:bg-red-50 dark:border-red-800 dark:bg-slate-800 dark:text-red-400 dark:hover:bg-red-900/30"
                  aria-label={`Delete profile ${profile.name}`}
                  data-testid={`run-profile-delete-${profile.name}`}
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>

      <SaveProfileDialog
        open={showSave}
        onClose={() => setShowSave(false)}
        onSaved={() => {
          setShowSave(false);
          refresh();
        }}
        onError={(msg) =>
          onToast({ title: "Save failed", description: msg, variant: "error" })
        }
        onToast={onToast}
      />
      <ConfirmDialog
        open={confirmDelete !== null}
        title="Delete profile?"
        body={
          confirmDelete
            ? `Profile "${confirmDelete.name}" (${confirmDelete.botCount} bot${confirmDelete.botCount === 1 ? "" : "s"}) will be permanently removed.`
            : ""
        }
        confirmLabel="Delete"
        destructive
        onCancel={() => setConfirmDelete(null)}
        onConfirm={() => {
          if (confirmDelete) remove.mutate(confirmDelete.name);
          setConfirmDelete(null);
        }}
      />
    </section>
  );
}

interface SaveProfileDialogProps {
  open: boolean;
  onClose: () => void;
  onSaved: () => void;
  onError: (msg: string) => void;
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

function SaveProfileDialog({ open, onClose, onSaved, onError, onToast }: SaveProfileDialogProps) {
  const [name, setName] = useState("");
  const nameHistory = useFieldHistory("profileName");

  const save = useMutation({
    mutationFn: (n: string) => api.saveProfile({ name: n, source: "current" }),
    onSuccess: (data) => {
      nameHistory.push(data.name);
      onToast({
        title: `Saved profile "${data.name}"`,
        description: `${data.bots.length} bot(s) captured.`,
        variant: "success",
      });
      setName("");
      onSaved();
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onError(msg);
    },
  });

  const submit = () => {
    const trimmed = name.trim();
    if (trimmed === "") {
      onError("Profile name is required");
      return;
    }
    save.mutate(trimmed);
  };

  return (
    <Dialog.Root open={open} onOpenChange={(o) => (o ? null : onClose())}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(28rem,90vw)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white p-5 shadow-xl dark:border-slate-700 dark:bg-slate-800"
          data-testid="save-profile-dialog"
        >
          <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
            Save current bots as a profile
          </Dialog.Title>
          <Dialog.Description className="mt-1 text-sm text-neutral-600 dark:text-slate-300">
            Snapshots every bot currently in the orchestrator&apos;s registry. Pick a unique
            name to avoid overwriting an existing profile.
          </Dialog.Description>
          <div className="mt-4">
            <label className="text-sm font-medium text-neutral-800 dark:text-slate-200">
              Profile name
            </label>
            <input
              autoFocus
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. demo-3-jwt-bots"
              className="mt-1 w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
              data-testid="save-profile-name"
              pattern="[A-Za-z0-9][A-Za-z0-9-]*"
              maxLength={64}
            />
            <p className="mt-1 text-xs text-neutral-500 dark:text-slate-400">
              Alphanumeric and hyphens; up to 64 chars.
            </p>
          </div>
          <div className="mt-5 flex justify-end gap-2">
            <button
              type="button"
              onClick={onClose}
              className="rounded-md border border-neutral-200 bg-white px-3 py-1.5 text-sm text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={submit}
              disabled={save.isPending}
              className="rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-600 disabled:opacity-50"
              data-testid="save-profile-submit"
            >
              {save.isPending ? "Saving…" : "Save profile"}
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function formatSavedAt(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleString();
  } catch {
    return iso;
  }
}
