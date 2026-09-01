import { RuleTester } from "eslint";
import { describe, it } from "vitest";
import rule from "./no-disable-gpu-with-fake-media.mjs";

const ruleTester = new RuleTester({
  languageOptions: { ecmaVersion: 2022, sourceType: "module" },
});

describe("no-disable-gpu-with-fake-media", () => {
  it("flags --disable-gpu only when the same array opens a camera", () => {
    ruleTester.run("no-disable-gpu-with-fake-media", rule, {
      valid: [
        // The global-setup.ts shape: no media flags, so the browser never opens a
        // camera and the flag is irrelevant to it. Exempt without an allowlist.
        'const a = ["--ignore-certificate-errors", "--disable-gpu", "--disable-dev-shm-usage"];',
        // A camera-capable list that does not carry the flag — the shape this PR leaves behind.
        'const a = ["--use-fake-device-for-media-stream", "--disable-dev-shm-usage"];',
        // Neither flag.
        'const a = ["--ignore-certificate-errors"];',
        // Empty and non-string members must not throw.
        "const a = [];",
        'const a = [someVar, "--use-fake-device-for-media-stream"];',
        // The two flags in DIFFERENT arrays is not the hazard.
        'const a = ["--disable-gpu"]; const b = ["--use-fake-device-for-media-stream"];',
      ],
      invalid: [
        {
          code: 'const a = ["--use-fake-device-for-media-stream", "--disable-gpu"];',
          errors: [{ messageId: "disableGpuWithFakeMedia" }],
        },
        {
          code: 'const a = ["--use-fake-device-for-media-stream=device-count=3", "--disable-gpu"];',
          errors: [{ messageId: "disableGpuWithFakeMedia" }],
        },
        // Order must not matter.
        {
          code: 'const a = ["--disable-gpu", "--use-fake-device-for-media-stream"];',
          errors: [{ messageId: "disableGpuWithFakeMedia" }],
        },
        // The real CHROME_ARGS shape this PR fixed.
        {
          code: `const CHROME_ARGS = [
            "--ignore-certificate-errors",
            "--use-fake-device-for-media-stream",
            "--use-fake-ui-for-media-stream",
            "--disable-gpu",
            "--disable-dev-shm-usage",
          ];`,
          errors: [{ messageId: "disableGpuWithFakeMedia" }],
        },
        // Inline at a launch site, not only in a named const.
        {
          code: 'chromium.launch({ args: ["--use-fake-device-for-media-stream", "--disable-gpu"] });',
          errors: [{ messageId: "disableGpuWithFakeMedia" }],
        },
        // Template literals with no expressions are static strings too.
        {
          code: "const a = [`--use-fake-device-for-media-stream`, `--disable-gpu`];",
          errors: [{ messageId: "disableGpuWithFakeMedia" }],
        },
      ],
    });
  });
});
