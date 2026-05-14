import { NETSIM_PRESET_META } from "../lib/constants";

/**
 * In-app help / cheat-sheet. Keep this scannable — section headings,
 * short bullets, tables where structure beats prose. Operators land
 * here when something behaves unexpectedly; long expository text is
 * the wrong shape.
 */
export function HelpPage() {
  return (
    <div
      className="flex flex-col gap-6 rounded-lg border border-neutral-200 bg-white p-6 shadow-sm dark:border-slate-700 dark:bg-slate-800"
      data-testid="help-page"
    >
      <Section title="Getting started">
        <p className="text-sm text-neutral-700 dark:text-slate-300">
          Run{" "}
          <Code>npm run bot -- dashboard</Code>. That&apos;s it. The launch form below adds bots;
          the table shows running ones. No separate{" "}
          <Code>bots-app run</Code> terminal is required — the dashboard spawns the orchestrator
          + ctl server in-process by default.
        </p>
      </Section>

      <Section title="Auth backends">
        <ul className="space-y-2 text-sm text-neutral-700 dark:text-slate-300">
          <li>
            <strong>Guest (no auth):</strong> skip auth entirely. Works only when the meeting
            allows guest joining (no session cookie required to land on{" "}
            <Code>/meeting/&lt;id&gt;</Code>). The fastest way to bring a test bot into a public
            room.
          </li>
          <li>
            <strong>JWT (cookie injection):</strong> inject a session cookie signed with the
            dev <Code>JWT_SECRET</Code>. Works on local, HCL daily, and PR previews — anywhere
            we own the secret.
          </li>
          <li>
            <strong>Storage State:</strong> replay a previously-captured session via{" "}
            <Code>bots-app login</Code>. Use this for real-OAuth deployments like{" "}
            <Code>app.videocall.rs</Code>.
          </li>
        </ul>
      </Section>

      <Section title="Network profiles">
        <p className="mb-3 text-sm text-neutral-700 dark:text-slate-300">
          Simulated network conditions applied via <Code>?netsim=&lt;profile&gt;</Code>. Requires
          the client to be built with <Code>--features netsim</Code>.
        </p>
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead className="bg-neutral-50 text-xs uppercase tracking-wide text-neutral-500 dark:bg-slate-900 dark:text-slate-400">
              <tr>
                <th className="px-3 py-2 text-left font-medium">Profile</th>
                <th className="px-3 py-2 text-left font-medium">Description</th>
                <th className="px-3 py-2 text-left font-medium">Latency</th>
                <th className="px-3 py-2 text-left font-medium">Loss</th>
                <th className="px-3 py-2 text-left font-medium">Bandwidth</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-neutral-100 dark:divide-slate-700">
              {NETSIM_PRESET_META.map((preset) => (
                <tr key={preset.name}>
                  <td className="px-3 py-2 font-mono text-xs text-neutral-700 dark:text-slate-300">
                    {preset.name}
                  </td>
                  <td className="px-3 py-2 text-neutral-700 dark:text-slate-300">
                    {preset.description}
                  </td>
                  <td className="px-3 py-2 text-neutral-600 dark:text-slate-400">
                    {preset.latencyMs} ms
                  </td>
                  <td className="px-3 py-2 text-neutral-600 dark:text-slate-400">
                    {preset.loss}
                  </td>
                  <td className="px-3 py-2 text-neutral-600 dark:text-slate-400">
                    {preset.bandwidth}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Section>

      <Section title="Run profiles">
        <p className="text-sm text-neutral-700 dark:text-slate-300">
          A <em>run profile</em> is a snapshot of a group of bot configurations that can be
          re-launched with one click. Use the <strong>Run Profiles</strong> section on the Bots
          page:
        </p>
        <ul className="mt-2 list-disc space-y-1 pl-5 text-sm text-neutral-700 dark:text-slate-300">
          <li>
            Click <strong>Save current as profile</strong> after launching the bots you want to
            replay later.
          </li>
          <li>
            Pick a profile from the dropdown and click <strong>Launch</strong> to spawn the same
            set again.
          </li>
          <li>
            Profiles live on disk at{" "}
            <Code>e2e/bots-app/run/profiles/&lt;name&gt;.json</Code> so they survive restarts.
          </li>
        </ul>
      </Section>

      <Section title="Troubleshooting">
        <dl className="space-y-3 text-sm text-neutral-700 dark:text-slate-300">
          <TroubleshootRow
            question="Bot stuck on the join screen"
            answer="Open the headed Chrome and check the console. Most often: the meeting URL is wrong, or JWT auth was used against a host that requires real OAuth. Try Guest mode or Storage State."
          />
          <TroubleshootRow
            question="Token file not found"
            answer="In self-hosted mode the dashboard writes its own ctl-<pid>.token file under the run dir. If you passed --ctl-token-file pointing somewhere else, check that path. Stale token files from old daemons can confuse auto-discovery — delete ctl-*.token files for dead PIDs."
          />
          <TroubleshootRow
            question="Meeting handshake fails locally"
            answer="Confirm the relay/UI compose stack is up (make dev or docker-compose up in the local stack) and that the meeting URL hostname resolves to 127.0.0.1. If you see WebTransport handshake errors but WebSockets work, your local cert is likely expired."
          />
          <TroubleshootRow
            question="Asset dropdowns are empty"
            answer="Run `npm run bot -- prep-assets` to generate the y4m + wav files under run/costumes/ and run/audio/. Without those the bot falls back to Chrome's default fake camera/mic pattern."
          />
        </dl>
      </Section>

      <Section title="Architecture">
        <ol className="list-decimal space-y-1 pl-5 text-sm text-neutral-700 dark:text-slate-300">
          <li>
            <strong>Dashboard sidecar</strong> — Node HTTP server (this process) on{" "}
            <Code>127.0.0.1</Code>; serves the React UI and proxies <Code>/api/*</Code> to the
            ctl API.
          </li>
          <li>
            <strong>Ctl API</strong> — phase-4 HTTP control surface that mutates bot state
            (launch, leave, kill, mute, ttl, …). Bearer-token authenticated.
          </li>
          <li>
            <strong>Browser bot</strong> — one Playwright-driven Chrome per launched bot. Logs
            stream to the same terminal.
          </li>
        </ol>
      </Section>

      <Section title="Links">
        <ul className="list-disc space-y-1 pl-5 text-sm text-neutral-700 dark:text-slate-300">
          <li>
            <a
              className="text-sky-600 hover:underline dark:text-sky-400"
              href="https://github01.hclpnp.com/labs-projects/videocall/discussions/793"
              target="_blank"
              rel="noreferrer"
            >
              Discussion #793 — videocall bots-app design + roadmap
            </a>
          </li>
        </ul>
      </Section>
    </div>
  );
}

interface SectionProps {
  title: string;
  children: React.ReactNode;
}

function Section({ title, children }: SectionProps) {
  return (
    <section>
      <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
        {title}
      </h2>
      <div className="mt-2">{children}</div>
    </section>
  );
}

function Code({ children }: { children: React.ReactNode }) {
  return (
    <code className="rounded bg-neutral-100 px-1.5 py-0.5 font-mono text-xs text-neutral-800 dark:bg-slate-900 dark:text-slate-200">
      {children}
    </code>
  );
}

interface TroubleshootRowProps {
  question: string;
  answer: string;
}

function TroubleshootRow({ question, answer }: TroubleshootRowProps) {
  return (
    <div>
      <dt className="font-medium text-neutral-800 dark:text-slate-200">{question}</dt>
      <dd className="mt-0.5 text-neutral-600 dark:text-slate-400">{answer}</dd>
    </div>
  );
}
