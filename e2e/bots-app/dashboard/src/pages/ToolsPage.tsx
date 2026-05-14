import { ConfigImportPanel } from "../components/ConfigImportPanel";
import { OauthSessionsPanel } from "../components/OauthSessionsPanel";
import { ToastShelf, useToastShelf } from "../components/ToastShelf";

/**
 * "Tools" route — admin-style features adjacent to the bot launch flow.
 * Each card is self-contained so the Tools page can grow with future
 * dashboard-vs-CLI parity work without re-architecting.
 *
 * Today's cards:
 *   - OAuth Sessions — capture / list / delete per-account storage-state
 *     files used by `--auth=storage-state`. Sibling of the HCL SSO
 *     flow (which stays in the header chip).
 *   - Import YAML Config — paste/upload a meeting-config YAML and
 *     launch the whole fleet. Mirrors `bots-app run --config <path>`.
 */
export function ToolsPage() {
  const toast = useToastShelf();
  return (
    <div className="flex flex-col gap-6">
      <OauthSessionsPanel onToast={(t) => toast.push(t)} />
      <ConfigImportPanel onToast={(t) => toast.push(t)} />
      <ToastShelf entries={toast.entries} onDismiss={toast.dismiss} />
    </div>
  );
}
