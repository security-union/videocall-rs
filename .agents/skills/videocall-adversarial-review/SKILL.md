---
name: videocall-adversarial-review
description: Perform the mandatory adversarial review of videocall-rs changes. Use before creating, updating, or marking a pull request ready; when reviewing or re-reviewing a pull request; when asked for a pre-submit, fresh-context, or code review; and before approving or requesting changes on a PR.
---

# Videocall Adversarial Review

Apply the same correctness standard to the agent's own code and to other authors' pull requests. Treat tests, current-head CI, mergeability, prior review findings, and real runtime execution as verdict inputs.

## Select The Mode

- Use **pre-submit mode** for local changes or a branch before creating, updating, or marking a PR ready.
- Use **pull-request mode** for a PR URL/number, a re-review, or a request to post a verdict.
- For two or more PRs, list them oldest-first and ask whether to review sequentially or in parallel before changing labels.

Read [review-rubric.md](references/review-rubric.md) for every review. In pull-request mode, also read [pr-lifecycle.md](references/pr-lifecycle.md) completely before mutating GitHub state.

Resolve all referenced scripts and files relative to this skill directory, not the repository root.

## Pre-Submit Mode

1. Identify the intended base branch. Inspect the complete base-to-head diff plus relevant uncommitted changes; do not review only the last commit.
2. Enumerate changed production files and tests. Classify every change using the test-obligation table in the rubric.
3. Trace each changed behavior through its real production entry point, lifecycle, transports, failure paths, and cleanup. Search the repository for the same root-cause pattern and intentionally similar implementations.
4. Verify tests import or invoke production code and would fail if the production fix were reverted. When the current task authorizes implementation, add missing required tests; for review-only tasks, report the gap without modifying files.
5. Run the narrowest relevant tests first, then the repository-required formatters, linters, and broader checks. For Rust test-bearing targets, compile tests with `cargo check --tests`, `cargo test --no-run`, or run them; plain `cargo check` is insufficient.
6. Review the diff again from a clean perspective. Use an independent subagent when available without leaking expected findings; otherwise set aside prior implementation reasoning and rebuild conclusions from the diff, tests, and call paths.
7. Report findings first with severity and file:line evidence. When implementation is authorized, fix every blocker, rerun affected checks, and repeat the adversarial pass. Otherwise stop after reporting the blockers.
8. Do not create, push as review-ready, or request review for a PR while required tests are absent, applicable validation is red, behavioral claims are unverified, or a blocker remains. A user may explicitly request a WIP push, but the unresolved status must be stated.

## Pull-Request Mode

1. Resolve the PR repository from its explicit URL. For a bare PR number in this repository, use `github01.hclpnp.com/labs-projects/videocall`; never infer the HCL target from `origin`.
2. Follow the start-state procedure in [pr-lifecycle.md](references/pr-lifecycle.md), including the transient `REVIEW IN PROGRESS...` label.
3. Run `scripts/collect-pr-context.sh` and read the complete output: PR metadata, formal reviews, issue comments, inline comments, commits, changed files, and current-head checks.
4. Audit every prior finding and every sub-point independently against the current head. Classify each as resolved/stale, live, or over-indexed, with evidence. Honor later corrections and author responses.
5. Fetch the exact head into an isolated worktree. Inspect the complete base-to-head diff and surrounding production code; never rely on a truncated API patch.
6. Apply the full rubric, including test obligations, mutation sensitivity, execution paths, lifecycle, both transports, network conditions, scale, security, and claim accuracy.
7. Verify mergeability and all required CI checks on the current head SHA. Investigate logs for failures; do not call a failure a flake without evidence.
8. Post findings and a formal verdict. Missing required tests, merge conflicts, unexplained red CI, or code blockers prohibit approval. Use `COMMENT` for self-authored PRs when GitHub prohibits a formal verdict, but apply the substantive labels.
9. Reconcile terminal labels using the lifecycle reference. Always remove `REVIEW IN PROGRESS...`, including on abort.
10. For an approved PR, either fix small directly-related issues when the user authorized reviewer edits, file concrete high-value follow-ups after deduplication, or drop irrelevant nits. Never defer required tests or blockers.

## Non-Negotiable Evidence

- Base and head SHAs reviewed.
- Complete changed-file list and full substantive diff inspected.
- Every prior comment body read without truncation.
- Every multi-part prior finding checked one sub-point at a time.
- Required tests identified by change class and inspected for production-path coverage.
- Applicable tests/checks run or current-head CI evidence recorded.
- Mergeability checked immediately before approval.
- Final verdict and labels agree.

If any item cannot be verified, state the gap and do not infer success.
