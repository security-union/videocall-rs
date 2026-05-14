import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";

import { addEntry, removeEntry, useFieldHistory } from "../lib/fieldHistory";

describe("addEntry()", () => {
  it("inserts a new value at the head with the given timestamp", () => {
    const next = addEntry([], "alice", 1000);
    expect(next).toEqual([{ value: "alice", lastUsed: 1000 }]);
  });

  it("de-duplicates by bumping the timestamp of an existing value", () => {
    const start = [
      { value: "alice", lastUsed: 1000 },
      { value: "bob", lastUsed: 2000 },
    ];
    const next = addEntry(start, "alice", 3000);
    expect(next).toEqual([
      { value: "alice", lastUsed: 3000 },
      { value: "bob", lastUsed: 2000 },
    ]);
  });

  it("keeps entries sorted by lastUsed DESC", () => {
    const start = [
      { value: "old", lastUsed: 100 },
      { value: "recent", lastUsed: 500 },
    ];
    const next = addEntry(start, "newest", 1000);
    expect(next.map((e) => e.value)).toEqual(["newest", "recent", "old"]);
  });

  it("ignores whitespace-only values", () => {
    expect(addEntry([], "   ", 1000)).toEqual([]);
    expect(addEntry([], "", 1000)).toEqual([]);
  });

  it("trims values before storing them", () => {
    const next = addEntry([], "  5m  ", 1000);
    expect(next).toEqual([{ value: "5m", lastUsed: 1000 }]);
  });

  it("caps the list at maxEntries (default 10)", () => {
    let list: { value: string; lastUsed: number }[] = [];
    for (let i = 0; i < 15; i++) {
      list = addEntry(list, `v${i}`, i);
    }
    expect(list).toHaveLength(10);
    // Most-recent at head
    expect(list[0].value).toBe("v14");
    expect(list[9].value).toBe("v5");
  });

  it("honors a custom maxEntries cap", () => {
    let list: { value: string; lastUsed: number }[] = [];
    for (let i = 0; i < 5; i++) {
      list = addEntry(list, `v${i}`, i, 3);
    }
    expect(list).toHaveLength(3);
    expect(list.map((e) => e.value)).toEqual(["v4", "v3", "v2"]);
  });
});

describe("removeEntry()", () => {
  it("filters out the requested value", () => {
    const start = [
      { value: "alice", lastUsed: 1000 },
      { value: "bob", lastUsed: 2000 },
    ];
    expect(removeEntry(start, "alice")).toEqual([{ value: "bob", lastUsed: 2000 }]);
  });

  it("is a no-op when the value is absent", () => {
    const start = [{ value: "alice", lastUsed: 1000 }];
    expect(removeEntry(start, "ghost")).toEqual(start);
  });
});

describe("useFieldHistory()", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });
  afterEach(() => {
    window.localStorage.clear();
    vi.restoreAllMocks();
  });

  it("hydrates from localStorage on mount", () => {
    window.localStorage.setItem(
      "bots-app-dashboard:history:meetingURL",
      JSON.stringify([{ value: "https://example.com/m/A", lastUsed: 100 }]),
    );
    const { result } = renderHook(() => useFieldHistory("meetingURL"));
    expect(result.current.entries).toHaveLength(1);
    expect(result.current.entries[0].value).toBe("https://example.com/m/A");
  });

  it("returns empty when localStorage is empty", () => {
    const { result } = renderHook(() => useFieldHistory("ttl"));
    expect(result.current.entries).toEqual([]);
  });

  it("ignores malformed JSON in localStorage", () => {
    window.localStorage.setItem("bots-app-dashboard:history:ttl", "{not-json");
    const { result } = renderHook(() => useFieldHistory("ttl"));
    expect(result.current.entries).toEqual([]);
  });

  it("writes back to localStorage on push()", () => {
    const { result } = renderHook(() => useFieldHistory("participant"));
    act(() => result.current.push("alice"));
    expect(result.current.entries.map((e) => e.value)).toEqual(["alice"]);
    const stored = window.localStorage.getItem("bots-app-dashboard:history:participant");
    expect(stored).not.toBeNull();
    const parsed = JSON.parse(stored!);
    expect(parsed).toHaveLength(1);
    expect(parsed[0].value).toBe("alice");
  });

  it("re-pushing an existing value de-dupes and keeps it at the head", () => {
    const { result } = renderHook(() => useFieldHistory("participant"));
    act(() => result.current.push("alice"));
    act(() => result.current.push("bob"));
    act(() => result.current.push("alice"));
    expect(result.current.entries.map((e) => e.value)).toEqual(["alice", "bob"]);
  });

  it("caps the history at 10 entries", () => {
    const { result } = renderHook(() => useFieldHistory("meetingURL"));
    act(() => {
      for (let i = 0; i < 12; i++) {
        result.current.push(`v${i}`);
      }
    });
    expect(result.current.entries).toHaveLength(10);
    // Most-recent first; v11 inserted last.
    expect(result.current.entries[0].value).toBe("v11");
  });

  it("remove() deletes the specified entry and persists", () => {
    const { result } = renderHook(() => useFieldHistory("participant"));
    act(() => result.current.push("alice"));
    act(() => result.current.push("bob"));
    act(() => result.current.remove("alice"));
    expect(result.current.entries.map((e) => e.value)).toEqual(["bob"]);
    const stored = JSON.parse(
      window.localStorage.getItem("bots-app-dashboard:history:participant")!,
    );
    expect(stored.map((e: { value: string }) => e.value)).toEqual(["bob"]);
  });

  it("ignores whitespace-only push()", () => {
    const { result } = renderHook(() => useFieldHistory("participant"));
    act(() => result.current.push("   "));
    expect(result.current.entries).toEqual([]);
  });
});
