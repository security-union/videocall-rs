import { Locator } from "@playwright/test";

export async function waitForVisibleState<const T extends string>(
  states: readonly { name: T; locator: Locator }[],
  timeout = 30_000,
): Promise<T> {
  const deadline = Date.now() + timeout;

  while (Date.now() < deadline) {
    for (const state of states) {
      if (await state.locator.isVisible().catch(() => false)) {
        return state.name;
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }

  const names = states.map((state) => state.name).join(", ");
  throw new Error(`Timed out after ${timeout}ms waiting for one visible state: ${names}`);
}
