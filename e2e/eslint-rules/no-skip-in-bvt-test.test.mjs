import { RuleTester } from "eslint";
import { describe, it } from "vitest";
import rule from "./no-skip-in-bvt-test.mjs";

const ruleTester = new RuleTester({
  languageOptions: { ecmaVersion: 2022, sourceType: "module" },
});

describe("no-skip-in-bvt-test", () => {
  it("accepts the legitimate shapes and rejects every unconditional bailout under a @bvt tag", () => {
    ruleTester.run("no-skip-in-bvt-test", rule, {
      valid: [
        // Untagged bailouts: the deliberate developer-ergonomics case (#2378 scope note).
        'test("renders the panel", async () => { test.skip(true, "hook absent"); });',
        'test("renders the panel", async () => { test.skip(); });',
        'test("renders the panel", async () => { test.fixme(true, "hook absent"); });',
        'test.skip("permanently disabled, untagged", async () => { await go(); });',
        'test.describe.skip("untagged suite", () => { test("inner", async () => {}); });',
        // Real conditions gating the skip.
        'test("renders the panel @bvt1", async () => { test.skip(!hookPresent, "hook absent"); });',
        'test("renders the panel @bvt1", async () => { test.skip(process.platform === "darwin", "no"); });',
        'test("renders the panel @bvt1", async () => { test.skip(false, "never"); });',
        'test("renders the panel @bvt1", async () => { test.fixme(!hookPresent, "absent"); });',
        // A tagged test that actually runs, inside a plain suite.
        'test.describe("suite", () => { test("critical @bvt1", async () => { await go(); }); });',
        // Tag-boundary: only @bvt0 / @bvt1 opt into the per-PR project.
        'test("renders the panel @bvt2", async () => { test.skip(true, "hook absent"); });',
        'test("renders the panel @bvt10", async () => { test.skip(true, "hook absent"); });',
      ],
      invalid: [
        {
          code: 'test("renders the panel @bvt1", async () => { test.skip(true, "hook absent"); });',
          errors: [
            {
              messageId: "skipInBvt",
              data: { form: "test.skip(true, …)", title: "renders the panel @bvt1" },
            },
          ],
        },
        {
          code: 'test("opens the chat sidebar @bvt1", async () => { if (n > 0) { await go(); } else { test.skip(); } });',
          errors: [
            {
              messageId: "skipInBvt",
              data: { form: "test.skip()", title: "opens the chat sidebar @bvt1" },
            },
          ],
        },
        {
          code: 'test("renders the panel @bvt1", async () => { test.fixme(true, "bail"); });',
          errors: [
            {
              messageId: "skipInBvt",
              data: { form: "test.fixme(true, …)", title: "renders the panel @bvt1" },
            },
          ],
        },
        {
          code: 'test("renders the panel @bvt1", async () => { test.fixme(); });',
          errors: [
            {
              messageId: "skipInBvt",
              data: { form: "test.fixme()", title: "renders the panel @bvt1" },
            },
          ],
        },
        {
          code: 'test("@bvt0 renders the panel", async () => { test.skip(true); });',
          errors: [{ messageId: "skipInBvt" }],
        },
        {
          code: 'test("renders @bvt1", async () => { if (x) { for (;;) { test.skip(true, "m"); } } });',
          errors: [{ messageId: "skipInBvt" }],
        },
        {
          code: 'test("renders @bvt1", async () => { await test.step("inner", async () => { test.skip(); }); });',
          errors: [{ messageId: "skipInBvt" }],
        },
        {
          code: 'test.only("renders @bvt1", async () => { test.skip(true, "m"); });',
          errors: [{ messageId: "skipInBvt" }],
        },
        // The tag is inherited from the suite, so Playwright selects the leaf test.
        {
          code: 'test.describe("suite @bvt1", () => { test("inner", async () => { test.skip(true, "m"); }); });',
          errors: [
            {
              messageId: "skipInBvt",
              data: { form: "test.skip(true, …)", title: "suite @bvt1 inner" },
            },
          ],
        },
        {
          code: 'test.describe("outer @bvt1", () => { test.describe("mid", () => { test("leaf", async () => { test.skip(); }); }); });',
          errors: [
            {
              messageId: "skipInBvt",
              data: { form: "test.skip()", title: "outer @bvt1 mid leaf" },
            },
          ],
        },
        // Declaration family, tag on the declaration itself.
        {
          code: 'test.skip("renders the panel @bvt1", async () => { await go(); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "renders the panel @bvt1", callee: "test.skip" },
            },
          ],
        },
        {
          code: 'test.fixme("renders the panel @bvt1", async () => { await go(); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "renders the panel @bvt1", callee: "test.fixme" },
            },
          ],
        },
        // Tag on the leaf, `.skip` on the suite — selected by --grep, reports skipped.
        {
          code: 'test.describe.skip("disabled suite", () => { test("critical @bvt1", async () => { await go(); }); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "disabled suite critical @bvt1", callee: "test.describe.skip" },
            },
          ],
        },
        {
          code: 'test.describe.fixme("disabled suite", () => { test("critical @bvt1", async () => { await go(); }); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "disabled suite critical @bvt1", callee: "test.describe.fixme" },
            },
          ],
        },
        // Tag on the suite, `.skip` on the leaf — the other inheritance direction.
        {
          code: 'test.describe("suite @bvt1", () => { test.skip("critical", async () => { await go(); }); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "suite @bvt1 critical", callee: "test.skip" },
            },
          ],
        },
        {
          code: 'test.describe("suite @bvt1", () => { test.fixme("critical", async () => { await go(); }); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "suite @bvt1 critical", callee: "test.fixme" },
            },
          ],
        },
        {
          code: 'test.describe.skip("suite @bvt1", () => { test("inner", async () => {}); });',
          errors: [
            {
              messageId: "declaredSkipped",
              data: { title: "suite @bvt1 inner", callee: "test.describe.skip" },
            },
          ],
        },
      ],
    });
  });
});
