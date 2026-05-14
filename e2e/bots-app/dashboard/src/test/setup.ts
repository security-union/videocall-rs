import "@testing-library/jest-dom/vitest";
import { afterEach, vi } from "vitest";
import { cleanup } from "@testing-library/react";

// jsdom does not implement ResizeObserver or matchMedia — Radix's
// Select / Popover primitives consult both. Stub them with no-op
// observers so component tests can render without crashing.
class ResizeObserverStub {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}
type GlobalWithObservers = typeof globalThis & {
  ResizeObserver?: typeof ResizeObserver;
  IntersectionObserver?: typeof IntersectionObserver;
};
const g = globalThis as GlobalWithObservers;
if (!g.ResizeObserver) {
  g.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;
}
if (!g.IntersectionObserver) {
  g.IntersectionObserver = ResizeObserverStub as unknown as typeof IntersectionObserver;
}
if (typeof window !== "undefined" && !window.matchMedia) {
  window.matchMedia = vi.fn().mockImplementation((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })) as unknown as typeof window.matchMedia;
}
if (typeof Element !== "undefined" && !Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = function () {};
}
if (typeof Element !== "undefined" && !Element.prototype.hasPointerCapture) {
  Element.prototype.hasPointerCapture = function () {
    return false;
  };
  Element.prototype.setPointerCapture = function () {};
  Element.prototype.releasePointerCapture = function () {};
}

// Unmount everything after each test to keep DOM state isolated.
afterEach(() => {
  cleanup();
});
