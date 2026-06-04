import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";

import {
  THEME_STORAGE_KEY,
  ThemeProvider,
  resolveEffective,
  useTheme,
  type ThemeMode,
} from "../lib/theme";

interface FakeMql {
  matches: boolean;
  media: string;
  onchange: null;
  addEventListener: (type: "change", h: (e: MediaQueryListEvent) => void) => void;
  removeEventListener: (type: "change", h: (e: MediaQueryListEvent) => void) => void;
  addListener: (h: (e: MediaQueryListEvent) => void) => void;
  removeListener: (h: (e: MediaQueryListEvent) => void) => void;
  dispatchEvent: (e: Event) => boolean;
  _listeners: ((e: MediaQueryListEvent) => void)[];
}

function installMatchMedia(initial: boolean): {
  mql: FakeMql;
  fire: (matches: boolean) => void;
} {
  const mql: FakeMql = {
    matches: initial,
    media: "(prefers-color-scheme: dark)",
    onchange: null,
    _listeners: [],
    addEventListener(_type, h) {
      this._listeners.push(h);
    },
    removeEventListener(_type, h) {
      this._listeners = this._listeners.filter((x) => x !== h);
    },
    addListener(h) {
      this._listeners.push(h);
    },
    removeListener(h) {
      this._listeners = this._listeners.filter((x) => x !== h);
    },
    dispatchEvent: () => false,
  };
  window.matchMedia = vi.fn().mockReturnValue(mql) as unknown as typeof window.matchMedia;
  return {
    mql,
    fire(matches) {
      mql.matches = matches;
      mql._listeners.forEach((h) =>
        h({ matches, media: mql.media } as MediaQueryListEvent),
      );
    },
  };
}

function Probe() {
  const { mode, effective, setMode } = useTheme();
  return (
    <div>
      <span data-testid="mode">{mode}</span>
      <span data-testid="effective">{effective}</span>
      <button onClick={() => setMode("light")} data-testid="set-light">
        light
      </button>
      <button onClick={() => setMode("dark")} data-testid="set-dark">
        dark
      </button>
      <button onClick={() => setMode("system")} data-testid="set-system">
        system
      </button>
    </div>
  );
}

describe("resolveEffective()", () => {
  it("returns the mode itself for explicit light/dark", () => {
    expect(resolveEffective("light")).toBe("light");
    expect(resolveEffective("dark")).toBe("dark");
  });

  it.each<[boolean, "light" | "dark"]>([
    [true, "dark"],
    [false, "light"],
  ])("resolves system against matchMedia=%s → %s", (matches, expected) => {
    installMatchMedia(matches);
    expect(resolveEffective("system")).toBe(expected);
  });
});

describe("<ThemeProvider />", () => {
  beforeEach(() => {
    window.localStorage.clear();
    document.documentElement.classList.remove("dark");
  });
  afterEach(() => {
    window.localStorage.clear();
    document.documentElement.classList.remove("dark");
    vi.restoreAllMocks();
  });

  it("applies the dark class when mode=dark", () => {
    installMatchMedia(false);
    render(
      <ThemeProvider initialMode="dark">
        <Probe />
      </ThemeProvider>,
    );
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(screen.getByTestId("effective").textContent).toBe("dark");
  });

  it("removes the dark class when mode=light", () => {
    installMatchMedia(true);
    document.documentElement.classList.add("dark");
    render(
      <ThemeProvider initialMode="light">
        <Probe />
      </ThemeProvider>,
    );
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    expect(screen.getByTestId("effective").textContent).toBe("light");
  });

  it("resolves system mode using prefers-color-scheme", () => {
    installMatchMedia(true);
    render(
      <ThemeProvider initialMode="system">
        <Probe />
      </ThemeProvider>,
    );
    expect(screen.getByTestId("effective").textContent).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("reacts to OS color-scheme changes when in system mode", () => {
    const { fire } = installMatchMedia(false);
    render(
      <ThemeProvider initialMode="system">
        <Probe />
      </ThemeProvider>,
    );
    expect(screen.getByTestId("effective").textContent).toBe("light");
    act(() => fire(true));
    expect(screen.getByTestId("effective").textContent).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    act(() => fire(false));
    expect(screen.getByTestId("effective").textContent).toBe("light");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  it("persists mode changes to localStorage", () => {
    installMatchMedia(false);
    render(
      <ThemeProvider initialMode="system">
        <Probe />
      </ThemeProvider>,
    );
    act(() => {
      screen.getByTestId("set-dark").click();
    });
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
    expect(screen.getByTestId("mode").textContent).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("hydrates from localStorage when initialMode is not provided", () => {
    window.localStorage.setItem(THEME_STORAGE_KEY, "dark" satisfies ThemeMode);
    installMatchMedia(false);
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(screen.getByTestId("mode").textContent).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });
});
