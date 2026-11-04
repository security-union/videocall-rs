/** Participant-record outcome and registry `finishReason` of a bot whose meeting page loaded a new document after it joined. */
export const PAGE_RELOADED = "page-reloaded";

/** How long the stamp waits for the joined document to answer before the launch fails. */
export const STAMP_TIMEOUT_MS = 10_000;

/** Evaluated in the page: the document's identity if it shows the in-meeting grid, else `null`. */
export function joinedDocumentIdentity(gridSelector: string): number | null {
  return document.querySelector(gridSelector) ? performance.timeOrigin : null;
}

export type StampResult = "stamped" | "reloaded" | "unreadable" | "timed-out" | "disarmed";

export interface PageReload {
  atMs: number;
  url: string;
}

/** True for the error page Chromium commits when a main-frame load fails. */
export function isLoadFailure(url: string): boolean {
  return url.startsWith("chrome-error://");
}

export function reloadCause(reload: PageReload): string {
  return isLoadFailure(reload.url)
    ? "the meeting page failed to load a new document"
    : "the meeting page loaded a new document";
}

/** The meeting document was replaced before `launchBot` handed the bot back, so it never counted as joined. */
export class PageReloadedError extends Error {
  constructor(readonly reload: PageReload) {
    super(`${reloadCause(reload)} during the launch (${reload.url})`);
    this.name = "PageReloadedError";
  }
}

export class StampTimeoutError extends Error {
  constructor(timeoutMs: number) {
    super(
      `the joined meeting document did not answer its identity read within ${timeoutMs}ms: failing the launch, since a reload could not be detected`,
    );
    this.name = "StampTimeoutError";
  }
}

export interface CommittedFrame {
  parentFrame(): unknown;
  url(): string;
}

export interface MainFrameNavigations {
  on(event: "framenavigated", listener: (frame: CommittedFrame) => void): unknown;
  off(event: "framenavigated", listener: (frame: CommittedFrame) => void): unknown;
  url(): string;
}

export interface DocumentIdentityReads {
  identity: () => Promise<unknown>;
  /** The document's identity, read in one evaluation with the in-meeting check: `null` when it is not in the meeting. */
  joinedIdentity: () => Promise<unknown>;
}

export interface PageReloadWatch {
  detected: Promise<PageReload>;
  /** What `detected` resolved with, readable synchronously; `null` before it. */
  seen: () => PageReload | null;
  /** Takes the current document as the joined one, once. */
  stamp: () => Promise<StampResult>;
  disarm: () => void;
}

const UNANSWERED = Symbol("unanswered");
const FAILED = Symbol("failed");
const CANCELLED = Symbol("cancelled");

/**
 * The stamp fails closed: a read that throws, a document not in the meeting, or a main-frame commit during it is a reload.
 * After it, a commit whose identity differs, whose read throws, or goes unanswered for the stamp timeout, is a replacement; off the meeting path it is a hang-up.
 * While armed, a commit of Chromium's load-failure page is a replacement, before the stamp too.
 * `disarm` ends every pending read, the stamp's included.
 */
export function watchPageReload(
  page: MainFrameNavigations,
  meetingPathPrefix: string,
  reads: DocumentIdentityReads,
  now: () => number = Date.now,
  stampTimeoutMs: number = STAMP_TIMEOUT_MS,
): PageReloadWatch {
  let seen: PageReload | null = null;
  let settle!: (reload: PageReload) => void;
  const detected = new Promise<PageReload>((r) => {
    settle = r;
  });
  const resolve = (reload: PageReload): void => {
    seen = reload;
    settle(reload);
  };
  let armed = true;
  let commits = 0;
  let joined: number | null = null;
  let stamping: Promise<StampResult> | null = null;
  const pendingReads = new Set<() => void>();
  const disarm = (): void => {
    armed = false;
    page.off("framenavigated", onFrame);
    for (const cancel of pendingReads) cancel();
  };
  const reloaded = (url: string = page.url()): void => {
    if (!armed) return;
    let pathname: string;
    try {
      pathname = new URL(url).pathname;
    } catch {
      return;
    }
    if (!isLoadFailure(url) && !pathname.startsWith(meetingPathPrefix)) return;
    disarm();
    resolve({ atMs: now(), url });
  };
  const answer = (read: () => Promise<unknown>): Promise<unknown> =>
    new Promise((settle) => {
      const done = (value: unknown): void => {
        clearTimeout(timer);
        pendingReads.delete(cancel);
        settle(value);
      };
      const cancel = (): void => done(CANCELLED);
      const timer = setTimeout(() => done(UNANSWERED), stampTimeoutMs);
      pendingReads.add(cancel);
      try {
        read().then(done, () => done(FAILED));
      } catch {
        done(FAILED);
      }
    });
  const check = async (url: string): Promise<void> => {
    const id = await answer(() => reads.identity());
    if (id === CANCELLED) return;
    if (id === UNANSWERED || id === FAILED || (typeof id === "number" && id !== joined)) {
      reloaded(url);
    }
  };
  const onFrame = (frame: CommittedFrame): void => {
    if (frame.parentFrame() !== null) return;
    commits += 1;
    if (isLoadFailure(frame.url())) reloaded(frame.url());
    else if (joined !== null) void check(frame.url());
  };
  page.on("framenavigated", onFrame);
  const takeStamp = async (): Promise<StampResult> => {
    const before = commits;
    const id = await answer(() => reads.joinedIdentity());
    if (id === CANCELLED) return "disarmed";
    if (id === UNANSWERED) return "timed-out";
    if (id === FAILED || commits !== before || id === null) {
      reloaded();
      return "reloaded";
    }
    if (typeof id !== "number") return "unreadable";
    joined = id;
    return "stamped";
  };
  const stamp = (): Promise<StampResult> => (stamping ??= takeStamp());
  return { detected, seen: () => seen, stamp, disarm };
}
