export type ScreenReading = "static" | "no-frames" | "live";

export const SETTLED_WINDOW_SAMPLES = 6;

export function classifyScreenText(screen: string): ScreenReading {
  if (/\(no frames\)/.test(screen)) {
    return "no-frames";
  }
  return /fps \(static\).*kbps \(static\)/.test(screen) ? "static" : "live";
}

export function settledStaticWindow(screens: readonly string[]): string[] | null {
  const kinds = screens.map(classifyScreenText);
  for (let start = 0; start + SETTLED_WINDOW_SAMPLES <= kinds.length; start++) {
    const window = kinds.slice(start, start + SETTLED_WINDOW_SAMPLES);
    if (
      window[0] === "static" &&
      window[window.length - 1] === "static" &&
      !window.includes("live") &&
      window.filter((k) => k === "no-frames").length <= 1
    ) {
      return screens.slice(start, start + SETTLED_WINDOW_SAMPLES);
    }
  }
  return null;
}
