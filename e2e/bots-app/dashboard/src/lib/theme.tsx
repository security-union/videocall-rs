import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";

export type ThemeMode = "light" | "dark" | "system";
export type EffectiveTheme = "light" | "dark";

export const THEME_STORAGE_KEY = "bots-app-dashboard:theme";

const DARK_QUERY = "(prefers-color-scheme: dark)";

interface ThemeContextValue {
  mode: ThemeMode;
  effective: EffectiveTheme;
  setMode: (mode: ThemeMode) => void;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

function readStoredMode(): ThemeMode {
  if (typeof window === "undefined") return "system";
  try {
    const raw = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (raw === "light" || raw === "dark" || raw === "system") return raw;
  } catch {
    // ignore — Safari private mode / disabled storage
  }
  return "system";
}

function safeMatchesDark(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  try {
    return window.matchMedia(DARK_QUERY).matches;
  } catch {
    return false;
  }
}

/**
 * Resolve a `ThemeMode` into a concrete `EffectiveTheme`. `system`
 * consults `prefers-color-scheme: dark`.
 */
export function resolveEffective(mode: ThemeMode): EffectiveTheme {
  if (mode === "light" || mode === "dark") return mode;
  return safeMatchesDark() ? "dark" : "light";
}

function applyDocumentClass(effective: EffectiveTheme): void {
  if (typeof document === "undefined") return;
  const root = document.documentElement;
  if (effective === "dark") {
    root.classList.add("dark");
  } else {
    root.classList.remove("dark");
  }
}

interface ThemeProviderProps {
  children: ReactNode;
  /** Optional override for unit tests; defaults to `readStoredMode()`. */
  initialMode?: ThemeMode;
}

/**
 * Provides the current theme mode to the app and keeps the `dark`
 * class on `<html>` in sync. A small inline `<script>` in
 * `index.html` performs the same class-setting before React mounts to
 * avoid a flash of the wrong palette.
 */
export function ThemeProvider({ children, initialMode }: ThemeProviderProps) {
  const [mode, setModeState] = useState<ThemeMode>(() => initialMode ?? readStoredMode());
  const [effective, setEffective] = useState<EffectiveTheme>(() => resolveEffective(mode));

  // Re-derive `effective` whenever `mode` changes.
  useEffect(() => {
    setEffective(resolveEffective(mode));
  }, [mode]);

  // Apply the document class whenever `effective` changes.
  useEffect(() => {
    applyDocumentClass(effective);
  }, [effective]);

  // While in `system` mode, listen for OS color-scheme changes and
  // re-evaluate `effective`. The listener is torn down when mode
  // switches away from `system`.
  useEffect(() => {
    if (mode !== "system") return;
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const mq = window.matchMedia(DARK_QUERY);
    const handler = (e: MediaQueryListEvent) => {
      setEffective(e.matches ? "dark" : "light");
    };
    // Old Safari uses `addListener`; modern browsers use addEventListener.
    if (typeof mq.addEventListener === "function") {
      mq.addEventListener("change", handler);
      return () => mq.removeEventListener("change", handler);
    } else if (typeof mq.addListener === "function") {
      mq.addListener(handler);
      return () => mq.removeListener(handler);
    }
    return undefined;
  }, [mode]);

  const setMode = useCallback((next: ThemeMode) => {
    setModeState(next);
    if (typeof window !== "undefined") {
      try {
        window.localStorage.setItem(THEME_STORAGE_KEY, next);
      } catch {
        // ignore
      }
    }
  }, []);

  const value = useMemo<ThemeContextValue>(
    () => ({ mode, effective, setMode }),
    [mode, effective, setMode],
  );

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (!ctx) {
    throw new Error("useTheme must be called inside <ThemeProvider>");
  }
  return ctx;
}
