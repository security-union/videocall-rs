import { BrowserContext, Page } from "@playwright/test";

const RECORDER_KEY = "__e2269_media_sockets";

const RECORDER_INIT_SCRIPT = `(() => {
  const Native = window.WebSocket;
  if (typeof Native !== "function" || window.${RECORDER_KEY}) return;
  const sockets = [];
  window.${RECORDER_KEY} = sockets;
  window.WebSocket = new Proxy(Native, {
    construct(target, args) {
      const ws = Reflect.construct(target, args);
      sockets.push(ws);
      return ws;
    },
  });
})();`;

export async function installMediaSocketRecorder(context: BrowserContext): Promise<void> {
  await context.addInitScript(RECORDER_INIT_SCRIPT);
}

export interface SeverResult {
  recorded: number;
  severed: number;
  endpoints: string[];
}

// Callers MUST assert `severed >= 1`.
// Media sockets are matched on `/lobby`; the dev server's own hot-reload socket
// does not match and is left alone. Close code 3001 is used because the
// WebSocket API only accepts 1000 or 3000-4999 from script.
export async function severMediaWebSocket(page: Page, closeCode = 3001): Promise<SeverResult> {
  return page.evaluate(
    ({ key, code }) => {
      const sockets = (window as unknown as Record<string, WebSocket[] | undefined>)[key] ?? [];
      const live = sockets.filter((ws) => ws.readyState === 1 && ws.url.includes("/lobby"));
      live.forEach((ws) => ws.close(code, "e2e-2269-sever"));
      return {
        recorded: sockets.length,
        severed: live.length,
        endpoints: live.map((ws) => ws.url.split("?")[0]),
      };
    },
    { key: RECORDER_KEY, code: closeCode },
  );
}
