import { BrowserContext, Page, WebSocketRoute } from "@playwright/test";

export const PEER_TILE = '[id^="peer-video-"][id$="-div"]';

const TOAST_KEY = "__e2e_resume_toasts";

export async function startToastRecorder(page: Page): Promise<void> {
  await page.evaluate((key) => {
    const seen: string[] = [];
    (window as unknown as Record<string, string[]>)[key] = seen;
    const capture = () => {
      document.querySelectorAll(".peer-toast .toast-action").forEach((el) => {
        const text = (el.textContent || "").trim();
        if (text && !seen.includes(text)) {
          seen.push(text);
        }
      });
    };
    capture();
    new MutationObserver(capture).observe(document.body, { childList: true, subtree: true });
  }, TOAST_KEY);
}

/** `null` when the recorder was never installed, so it cannot pass for "no toasts". */
export async function recordedToasts(page: Page): Promise<string[] | null> {
  return page.evaluate(
    (key) => (window as unknown as Record<string, string[] | undefined>)[key] ?? null,
    TOAST_KEY,
  );
}

export async function peerTileIds(page: Page): Promise<string[]> {
  return page.locator(PEER_TILE).evaluateAll((els) => els.map((el) => el.id));
}

export function instanceId(url: string): string | null {
  return new URL(url).searchParams.get("instance_id");
}

export function connSeq(url: string): string | null {
  return new URL(url).searchParams.get("conn_seq");
}

export interface HeldCandidate {
  severedUrl: string;
  url: string;
  drop(): Promise<void>;
}

export interface ReconnectHold {
  sever(timeoutMs?: number): Promise<HeldCandidate>;
  release(): void;
}

/**
 * Routes `url` on `context`. The relay sees the severed session's close
 * `settleMs` before the candidate's upgrade, so the candidate always lands
 * inside the departed session's grace.
 */
export async function holdReconnectCandidate(
  context: BrowserContext,
  url: RegExp,
  settleMs = 500,
): Promise<ReconnectHold> {
  let phase: "forward" | "candidate" | "hold" = "forward";
  let release!: () => void;
  const released = new Promise<void>((resolve) => {
    release = resolve;
  });
  const open = new Map<WebSocketRoute, WebSocketRoute>();
  let activeServer: WebSocketRoute | undefined;
  let candidateJoined!: (c: HeldCandidate) => void;
  const candidate = new Promise<HeldCandidate>((resolve) => {
    candidateJoined = resolve;
  });
  let severedUrl = "";

  await context.routeWebSocket(url, async (ws) => {
    if (phase === "forward") {
      const server = ws.connectToServer();
      open.set(ws, server);
      const forget = () => open.delete(ws);
      server.onClose(forget);
      ws.onClose((code, reason) => {
        forget();
        if (phase === "forward") {
          server.close({ code: code === 1005 || !code ? 1000 : code, reason });
        }
      });
      return;
    }
    if (phase === "candidate") {
      phase = "hold";
      ws.onMessage(() => undefined);
      ws.onClose(() => undefined);
      await activeServer!.close({ code: 1000, reason: "e2e-resume-severed" });
      await new Promise((resolve) => setTimeout(resolve, settleMs));
      let server: WebSocketRoute;
      try {
        server = ws.connectToServer();
      } catch {
        return;
      }
      let joined = false;
      server.onMessage(() => {
        if (joined) return;
        joined = true;
        candidateJoined({
          severedUrl,
          url: ws.url(),
          drop: async () => {
            await server.close({ code: 1000, reason: "e2e-resume-candidate-drops" });
            try {
              await ws.close({ code: 3001, reason: "e2e-resume-candidate-drops" });
            } catch {
              /* candidate already abandoned */
            }
          },
        });
      });
      return;
    }
    await released;
    try {
      ws.connectToServer();
    } catch {
      /* socket abandoned while held */
    }
  });

  return {
    async sever(timeoutMs = 30_000) {
      const [livePage, liveServer] = [...open].at(-1) ?? [];
      if (!livePage || !liveServer) {
        throw new Error(`no socket matching ${url} went through the route`);
      }
      activeServer = liveServer;
      severedUrl = livePage.url();
      phase = "candidate";
      try {
        await livePage.close({ code: 3001, reason: "e2e-resume-sever" });
      } catch {
        /* page side already closed */
      }
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        return await Promise.race([
          candidate,
          new Promise<never>((_, reject) => {
            timer = setTimeout(
              () => reject(new Error("no reconnect candidate reached the relay")),
              timeoutMs,
            );
          }),
        ]);
      } finally {
        clearTimeout(timer);
      }
    },
    release() {
      phase = "forward";
      release();
    },
  };
}
