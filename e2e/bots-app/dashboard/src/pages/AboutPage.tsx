export function AboutPage() {
  return (
    <div className="rounded-lg border border-neutral-200 bg-white p-6 shadow-sm dark:border-slate-700 dark:bg-slate-800">
      <h2 className="text-xl font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
        About the bots-app dashboard
      </h2>
      <p className="mt-3 text-sm text-neutral-700 dark:text-slate-300">
        This UI is a thin client over the{" "}
        <code className="rounded bg-neutral-100 px-1.5 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
          bots-app
        </code>{" "}
        Node sidecar (phase 4 control API + phase 5 dashboard server). Every action ends up as a
        request against the same{" "}
        <code className="rounded bg-neutral-100 px-1.5 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
          ctl
        </code>{" "}
        HTTP endpoints exposed by a running{" "}
        <code className="rounded bg-neutral-100 px-1.5 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
          bots-app run --ctl-port auto
        </code>{" "}
        orchestrator.
      </p>

      <h3 className="mt-6 text-base font-semibold text-neutral-900 dark:text-slate-100">
        Security model
      </h3>
      <ul className="mt-2 list-disc space-y-1 pl-5 text-sm text-neutral-700 dark:text-slate-300">
        <li>
          The dashboard backend binds only to{" "}
          <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
            127.0.0.1
          </code>
          .
        </li>
        <li>
          The ctl-API bearer token lives only in the Node sidecar process. Browser requests hit{" "}
          <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
            /api/*
          </code>{" "}
          and the sidecar attaches{" "}
          <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
            Authorization: Bearer …
          </code>{" "}
          before forwarding to the ctl API.
        </li>
        <li>The token never reaches the browser tab and is never logged.</li>
      </ul>

      <h3 className="mt-6 text-base font-semibold text-neutral-900 dark:text-slate-100">Phases</h3>
      <ul className="mt-2 list-disc space-y-1 pl-5 text-sm text-neutral-700 dark:text-slate-300">
        <li>
          <strong>Phase 1–3:</strong> headed/headless bots, fake devices, conversation, netsim
          presets.
        </li>
        <li>
          <strong>Phase 4:</strong> stateful orchestrator with the ctl HTTP API (mute / video /
          ttl / network / duplicate / leave / kill).
        </li>
        <li>
          <strong>Phase 5 (this UI):</strong> launch + manage bots from a browser.
        </li>
      </ul>

      <p className="mt-6 text-sm text-neutral-600 dark:text-slate-400">
        Discussion + design:{" "}
        <a
          className="text-sky-600 hover:underline dark:text-sky-400"
          href="https://github01.hclpnp.com/labs-projects/videocall/discussions/793"
          target="_blank"
          rel="noreferrer"
        >
          videocall discussion #793
        </a>
        .
      </p>
    </div>
  );
}
