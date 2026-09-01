import eslint from "@eslint/js";
import tseslint from "typescript-eslint";
import noSkipInBvtTest from "./eslint-rules/no-skip-in-bvt-test.mjs";
import noDisableGpuWithFakeMedia from "./eslint-rules/no-disable-gpu-with-fake-media.mjs";

export default tseslint.config(
  eslint.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ["**/*.ts"],
    rules: {
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_" }],
    },
  },
  {
    files: [
      "tests/**/*.ts",
      "helpers/**/*.ts",
      "bots-app/src/**/*.ts",
      "playwright.config.ts",
      "global-setup.ts",
    ],
    plugins: {
      videocall: {
        rules: {
          "no-skip-in-bvt-test": noSkipInBvtTest,
          "no-disable-gpu-with-fake-media": noDisableGpuWithFakeMedia,
        },
      },
    },
    rules: {
      "videocall/no-disable-gpu-with-fake-media": "error",
    },
  },
  {
    files: ["tests/**/*.spec.ts"],
    rules: {
      "videocall/no-skip-in-bvt-test": "error",
    },
  },
  {
    ignores: [
      "node_modules/",
      "test-results/",
      "playwright-report/",
      // The dashboard subtree has its own self-contained tooling
      // (TypeScript / ESLint / Prettier / Vitest) — keep it out of
      // the top-level e2e linter to avoid double-coverage and a
      // dependency surface explosion in the parent package.json.
      "bots-app/dashboard/",
    ],
  },
);
