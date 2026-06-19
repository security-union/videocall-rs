import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for `migrate_legacy_storage()` in `dioxus-ui/src/context.rs`.
 *
 * The function runs once on first startup (guarded by a `vc_storage_migrated`
 * sentinel) and migrates old localStorage formats to the current plain-text
 * `vc_display_name` key:
 *
 *   1. If `vc_display_name` looks like a legacy CBOR+zlib hex blob (>= 16
 *      hex chars, even length, all hex digits, valid zlib magic header), it
 *      is removed.
 *   2. If `vc_display_name_raw` exists, its value is promoted to
 *      `vc_display_name` and the raw key is removed.
 *   3. Otherwise, if `vc_username` exists, its value is promoted to
 *      `vc_display_name` and the legacy key is removed.
 *   4. If `vc_display_name` is already plain text, it is left untouched.
 *
 * After running, the sentinel `vc_storage_migrated=1` is written so the
 * migration never fires again — preventing perpetual deletion of legitimate
 * all-hex display names.
 */

// Real CBOR+zlib hex blob: zlib.compress(CBOR("Alice")).  Header 0x789c
// satisfies the zlib magic check (0x789c = 30876, 30876 % 31 == 0), the
// body is a valid deflate stream, and the trailer is an Adler-32 checksum.
const REAL_CBOR_HEX_BLOB = "789c4b75ccc94c4e050007bf0244";

test.describe("Legacy storage migration (migrate_legacy_storage)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test.afterEach(async ({ page }) => {
    await page.evaluate(() => {
      localStorage.removeItem("vc_display_name");
      localStorage.removeItem("vc_display_name_raw");
      localStorage.removeItem("vc_username");
      localStorage.removeItem("vc_storage_migrated");
    });
  });

  // 1. A real CBOR+zlib hex blob is detected and removed; no fallback keys
  //    exist so vc_display_name ends up absent and the #username input is empty.
  test("removes real CBOR+zlib hex blob from vc_display_name when no fallback keys exist", async ({
    page,
  }) => {
    await page.goto("/");
    await page.evaluate(
      (blob) => localStorage.setItem("vc_display_name", blob),
      REAL_CBOR_HEX_BLOB,
    );
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBeNull();

    const input = page.locator("#username");
    await expect(input).toHaveValue("");

    // Sentinel must be written so migration doesn't re-run.
    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_storage_migrated")), {
        timeout: 10_000,
      })
      .toBe("1");
  });

  // 2. vc_display_name_raw is promoted to vc_display_name when no current
  //    key exists.
  test("promotes vc_display_name_raw to vc_display_name", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_display_name_raw", "Alice"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("Alice");

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name_raw")), {
        timeout: 10_000,
      })
      .toBeNull();
  });

  // 3. vc_username is promoted to vc_display_name when no other keys exist.
  test("promotes vc_username to vc_display_name", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_username", "Bob"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("Bob");

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_username")), {
        timeout: 10_000,
      })
      .toBeNull();
  });

  // 4. When a CBOR hex blob exists alongside vc_display_name_raw, the blob
  //    is dropped and the raw fallback wins.
  test("drops CBOR hex blob and falls back to vc_display_name_raw", async ({ page }) => {
    await page.goto("/");
    await page.evaluate((blob) => {
      localStorage.setItem("vc_display_name", blob);
      localStorage.setItem("vc_display_name_raw", "Alice");
    }, REAL_CBOR_HEX_BLOB);
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("Alice");

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name_raw")), {
        timeout: 10_000,
      })
      .toBeNull();
  });

  // 5. A genuine plain-text value in vc_display_name is left untouched.
  test("preserves plain-text vc_display_name without modification", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_display_name", "RealName"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("RealName");
  });

  // 6. A legitimate all-hex display name (e.g. "1234") is preserved, not
  //    mistaken for a CBOR blob.  This is the false-positive regression test:
  //    short all-hex strings fail the >= 16 length floor, and even long ones
  //    fail the zlib magic check (first byte != 0x78).
  test("preserves legitimate all-hex display name", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_display_name", "1234"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("1234");

    // Verify the name is actually shown in the UI input.
    const input = page.locator("#username");
    await expect(input).toHaveValue("1234");
  });

  // 7. Migration sentinel prevents re-running: after migration completes,
  //    a subsequent reload does not re-evaluate the primary key.
  test("sentinel prevents migration from re-running on subsequent reloads", async ({ page }) => {
    await page.goto("/");
    // First load: seed a display name and let migration run (writes sentinel).
    await page.evaluate(() => localStorage.setItem("vc_display_name", "TestUser"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_storage_migrated")), {
        timeout: 10_000,
      })
      .toBe("1");

    // Now replace the display name with a value that would look like a hex
    // blob to the discriminator (but is actually a legitimate name).  Because
    // the sentinel exists, migration must NOT fire and the value must survive.
    await page.evaluate(() => localStorage.setItem("vc_display_name", "78deadbeefcafe1234567890"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("78deadbeefcafe1234567890");
  });
});
