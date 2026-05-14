import eslint from "@eslint/js";
import tseslint from "typescript-eslint";

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
