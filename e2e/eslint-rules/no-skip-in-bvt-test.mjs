const BVT_TAG = /@bvt[01]\b/;

const TEST_CALLEES = new Set(["test", "test.only", "test.skip", "test.fixme"]);

const DESCRIBE_CALLEES = new Set([
  "test.describe",
  "test.describe.only",
  "test.describe.serial",
  "test.describe.parallel",
  "test.describe.skip",
  "test.describe.fixme",
]);

// Playwright 1.58 routes skip and fixme through one branch (worker/testInfo.js):
// both set expectedStatus "skipped" and throw TestSkipError.
const BAILOUT_CALLEES = new Set(["test.skip", "test.fixme"]);

const SKIPPED_DECLARATIONS = new Set([
  "test.skip",
  "test.fixme",
  "test.describe.skip",
  "test.describe.fixme",
]);

const SKIPPED_DESCRIBES = new Set(["test.describe.skip", "test.describe.fixme"]);

const CASTS = new Set(["TSAsExpression", "TSTypeAssertion", "TSNonNullExpression"]);

/** Dotted source text of a callee, e.g. `test.describe.serial`, or null if not a plain member chain. */
function calleePath(node) {
  if (node.type === "Identifier") return node.name;
  if (node.type === "MemberExpression" && !node.computed) {
    const object = calleePath(node.object);
    if (object === null || node.property.type !== "Identifier") return null;
    return `${object}.${node.property.name}`;
  }
  return null;
}

function staticString(node) {
  if (node === undefined) return null;
  if (node.type === "Literal" && typeof node.value === "string") return node.value;
  if (node.type === "TemplateLiteral" && node.expressions.length === 0) {
    return node.quasis.map((q) => q.value.cooked ?? "").join("");
  }
  return null;
}

/** Zero-arg `test.skip()` in a test body bails unconditionally, same as `test.skip(true)`. */
function unconditionalForm(node, callee) {
  let first = node.arguments[0];
  if (first === undefined) return `${callee}()`;
  while (CASTS.has(first.type)) first = first.expression;
  if (first.type === "Literal" && first.value === true) return `${callee}(true, …)`;
  return null;
}

/**
 * Static titles of every enclosing test/describe call, outermost first. Playwright
 * selects on the combined title across suite ancestry, so a tag anywhere along
 * this path opts the leaf test into the @bvt project.
 */
function ancestorTitles(context, node) {
  const parts = [];
  for (const a of context.sourceCode.getAncestors(node)) {
    if (a.type !== "CallExpression") continue;
    const cp = calleePath(a.callee);
    if (cp === null || (!TEST_CALLEES.has(cp) && !DESCRIBE_CALLEES.has(cp))) continue;
    const title = staticString(a.arguments[0]);
    if (title !== null) parts.push(title);
  }
  return parts;
}

function skippedAncestor(context, node) {
  for (const a of context.sourceCode.getAncestors(node)) {
    if (a.type !== "CallExpression") continue;
    const cp = calleePath(a.callee);
    if (cp !== null && SKIPPED_DESCRIBES.has(cp)) return cp;
  }
  return null;
}

export default {
  meta: {
    type: "problem",
    docs: {
      description:
        "A @bvt0/@bvt1-tagged test must actually run: no unconditional `test.skip`/`test.fixme` bailout in its body, and no skipped/fixme declaration on it or a suite containing it. Catches the common syntactic forms only — NOT a complete guarantee. Escapes: computed titles, bailouts behind helper indirection, `beforeEach`/`beforeAll` hooks, coerced truthies (`!!1`), aliased callees; and it wrongly reports a skip inside a never-called closure. Only a runtime assertion that no selected test reported `skipped` can guarantee the property.",
    },
    schema: [],
    messages: {
      skipInBvt:
        'Unconditional `{{form}}` inside the @bvt-tagged test "{{title}}". A skipped test reports SUCCESS, so this makes the per-PR E2E gate green without observing anything. Assert the precondition instead of skipping it — see `assertInjectHook` in e2e/tests/freshness-skip.spec.ts or in e2e/tests/connection-quality-rtt-transitions.spec.ts.',
      declaredSkipped:
        'The @bvt-tagged test "{{title}}" is declared with `{{callee}}`, so it never runs yet still reports as part of the per-PR E2E gate. Drop the @bvt tag, or make it run.',
    },
  },

  create(context) {
    return {
      CallExpression(node) {
        const callee = calleePath(node.callee);
        if (callee === null) return;

        const ownTitle = staticString(node.arguments[0]);

        if (TEST_CALLEES.has(callee) && ownTitle !== null) {
          const title = [...ancestorTitles(context, node), ownTitle].join(" ");
          if (BVT_TAG.test(title)) {
            const via = SKIPPED_DECLARATIONS.has(callee) ? callee : skippedAncestor(context, node);
            if (via !== null) {
              context.report({ node, messageId: "declaredSkipped", data: { title, callee: via } });
              return;
            }
          }
        }

        if (!BAILOUT_CALLEES.has(callee)) return;
        const form = unconditionalForm(node, callee);
        if (form === null) return;

        const title = ancestorTitles(context, node).join(" ");
        if (title !== "" && BVT_TAG.test(title)) {
          context.report({ node, messageId: "skipInBvt", data: { form, title } });
        }
      },
    };
  },
};
