import { afterEach, describe, expect, it, vi } from "vitest";

import {
  type CommittedFrame,
  type DocumentIdentityReads,
  isLoadFailure,
  joinedDocumentIdentity,
  PageReloadedError,
  STAMP_TIMEOUT_MS,
  watchPageReload,
} from "./page-reload";

describe("joinedDocumentIdentity", () => {
  afterEach(() => vi.unstubAllGlobals());

  it.each([
    [true, 1234.5],
    [false, null],
  ])("with the grid present=%s returns %s", (present, expected) => {
    const querySelector = vi.fn((sel: string) => (present && sel === "#grid" ? {} : null));
    vi.stubGlobal("document", { querySelector });
    vi.stubGlobal("performance", { timeOrigin: 1234.5 });
    expect(joinedDocumentIdentity("#grid")).toBe(expected);
    expect(querySelector).toHaveBeenCalledWith("#grid");
  });
});

describe("watchPageReload", () => {
  afterEach(() => vi.useRealTimers());

  const MEETING = "https://example.test/meeting/Room";
  const watching = (reads: Partial<DocumentIdentityReads> = {}) => {
    let listener: ((f: CommittedFrame) => void) | null = null;
    const pageUrl = vi.fn(() => MEETING);
    const page = {
      on: (_e: "framenavigated", l: (f: CommittedFrame) => void) => void (listener = l),
      off: () => void (listener = null),
      url: pageUrl,
    };
    const watch = watchPageReload(page, "/meeting/Room", {
      identity: async () => 7,
      joinedIdentity: async () => 7,
      ...reads,
    });
    const commit = (opts: { child?: boolean; url?: string } = {}): void =>
      listener?.({
        parentFrame: () => (opts.child ? {} : null),
        url: () => opts.url ?? MEETING,
      });
    return { watch, commit, pageUrl };
  };
  const stamped = async (identity: () => Promise<unknown>) => {
    const w = watching({ identity });
    expect(await w.watch.stamp()).toBe("stamped");
    return w;
  };
  const settled = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

  it("fails closed when the identity read rejects", async () => {
    const { watch, commit } = await stamped(() =>
      Promise.reject(new Error("Execution context was destroyed")),
    );
    commit();
    await expect(watch.detected).resolves.toMatchObject({ url: MEETING });
  });

  it("cancels a pending identity check's timer on disarm, without reading it as a reload", async () => {
    vi.useFakeTimers();
    const { watch, commit } = await stamped(() => new Promise(() => {}));
    commit();
    expect(vi.getTimerCount()).toBe(1);
    watch.disarm();
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(STAMP_TIMEOUT_MS + 1);
    expect(watch.seen()).toBeNull();
  });

  it("cancels the stamp's timer on disarm and reports the stamp as disarmed", async () => {
    vi.useFakeTimers();
    const { watch } = watching({ joinedIdentity: () => new Promise(() => {}) });
    const stamp = watch.stamp();
    expect(vi.getTimerCount()).toBe(1);
    watch.disarm();
    expect(vi.getTimerCount()).toBe(0);
    await expect(stamp).resolves.toBe("disarmed");
  });

  it("does not read a child-frame commit during the stamp as a reload", async () => {
    let commitDuring = (): void => {};
    const { watch, commit } = watching({
      joinedIdentity: async () => {
        commitDuring();
        return 7;
      },
    });
    commitDuring = () => commit({ child: true });
    await expect(watch.stamp()).resolves.toBe("stamped");
  });

  it("does not read a child-frame commit after the stamp as a reload", async () => {
    const { watch, commit } = await stamped(async () => 8);
    commit({ child: true });
    await settled();
    expect(watch.seen()).toBeNull();
  });

  it("files a reload by the committed URL, not the page's URL when the read answers", async () => {
    const { watch, commit, pageUrl } = await stamped(async () => {
      pageUrl.mockReturnValue("https://example.test/");
      return 8;
    });
    commit();
    await settled();
    expect(watch.seen()).toMatchObject({ url: MEETING });
  });

  it.each([
    ["after the stamp", true],
    ["before the stamp", false],
  ])("reads Chromium's load-failure page %s as a replacement", async (_when, stamp) => {
    const errorPage = "chrome-error://chromewebdata/";
    const { watch, commit } = watching();
    if (stamp) expect(await watch.stamp()).toBe("stamped");
    commit({ url: errorPage });
    expect(watch.seen()).toMatchObject({ url: errorPage });
    expect(new PageReloadedError({ atMs: 0, url: errorPage }).message).toContain(
      "the meeting page failed to load a new document",
    );
  });

  it.each([
    ["chrome-error://chromewebdata/", true],
    ["chrome://crash/", false],
    ["https://example.test/meeting/Room", false],
  ])("isLoadFailure(%s) is %s", (url, expected) => {
    expect(isLoadFailure(url)).toBe(expected);
  });
});
