import { Page } from "@playwright/test";
import { TONE_EPSILON } from "./hand-chime-tones";

// Mirrors `play_user_joined` in dioxus-ui/src/components/attendants.rs (C5 -> E5).
export const JOIN_TONE_LOW = 523.25;
export const JOIN_TONE_HIGH = 659.25;

export const JOIN_TONE_SPY = `(() => {
  window.__vcJoinTones = [];
  const proto = window.AudioParam && window.AudioParam.prototype;
  if (!proto || typeof proto.setValueAtTime !== "function") return;
  const original = proto.setValueAtTime;
  proto.setValueAtTime = function (value, when) {
    try { window.__vcJoinTones.push(value); } catch (_) {}
    return original.call(this, value, when);
  };
})();`;

export function joinChimeCount(page: Page): Promise<number> {
  return page.evaluate(
    ({ low, high, eps }) => {
      const tones = (window as Window & { __vcJoinTones?: number[] }).__vcJoinTones;
      const setter = window.AudioParam?.prototype?.setValueAtTime;
      const patched =
        typeof setter === "function" &&
        Function.prototype.toString.call(setter).includes("__vcJoinTones");
      if (!Array.isArray(tones) || !patched) {
        throw new Error("the join-chime spy is not installed on this page");
      }
      const isLow = (v: number) => Math.abs(v - low) < eps;
      const isHigh = (v: number) => Math.abs(v - high) < eps;
      const pitches = tones.filter((v) => isLow(v) || isHigh(v));
      let chimes = 0;
      for (let i = 0; i + 1 < pitches.length; i++) {
        if (isLow(pitches[i]) && isHigh(pitches[i + 1])) chimes++;
      }
      return chimes;
    },
    { low: JOIN_TONE_LOW, high: JOIN_TONE_HIGH, eps: TONE_EPSILON },
  );
}
