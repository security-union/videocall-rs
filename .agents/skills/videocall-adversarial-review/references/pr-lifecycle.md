# Pull Request Lifecycle

## Repository Identity

Use an explicit PR URL whenever possible. For bare PR numbers in `videocall-rs`, use `github01.hclpnp.com/labs-projects/videocall`. Do not derive an HCL target from `origin`, which points at the public OSS repository and is off-limits for direct pushes.

Use authenticated `gh` commands. Do not read token files into commands or logs. Before mutation, record the host, owner/repository, PR number, base branch, head branch, and head SHA.

## Multiple Or Discovered PRs

With no explicit PR, discover open non-draft PRs labeled `READY FOR REVIEW`, paginate all results, and order them oldest-first. Skip a candidate only when the authenticated reviewer's latest formal review has the same `commit_id` as the current head SHA. Report reviewed and skipped PRs before starting.

For two or more PRs, ask whether to review sequentially or in parallel. In sequential mode, preserve oldest-first order. In parallel mode, isolate worktrees and GitHub state per PR. Apply `REVIEW IN PROGRESS...` only when work on that PR starts.

## Start And Abort State

At the start of a live review:

```bash
scripts/reconcile-pr-labels.sh HOST/OWNER/REPO PR start
```

This applies `REVIEW IN PROGRESS...` and removes `READY FOR REVIEW`. Treat the transient label as a tracked side effect.

On abort before posting a verdict:

```bash
scripts/reconcile-pr-labels.sh HOST/OWNER/REPO PR abort
```

This removes the transient label and restores `READY FOR REVIEW`. Run abort cleanup after errors or interruption; never leave the in-progress label attached.

## Collect Complete Context

Run:

```bash
scripts/collect-pr-context.sh PR_URL > /tmp/pr-context.json
```

For a bare number, pass the repository explicitly:

```bash
scripts/collect-pr-context.sh PR HOST/OWNER/REPO > /tmp/pr-context.json
```

The collector paginates formal reviews, issue comments, inline comments, commits, changed files, check runs, and classic commit statuses. It also collects the base branch's legacy required-status configuration and active branch rulesets. `enabled: false` means GitHub reports that legacy required status checks are not configured; any `collection_error` is an evidence gap that must be resolved before approval. Read every non-trivial body in full. Treat each sub-point as an independent verification item.

Fetch the recorded head SHA into an isolated worktree. Compare the merge base to that exact head and inspect surrounding production code. If the head changes during review, restart context collection and re-evaluate affected conclusions.

## Mergeability And CI

Treat `mergeable_state == dirty`, or a reliable `mergeable == false`, as a conflict. Retry once when GitHub reports unknown/null because mergeability is asynchronous. A conflicted PR cannot be approved.

When advising the author to resolve conflicts: **force-push is blocked on this repository**. Instruct them to use `git merge github01/PR-staging` (not `git rebase`). Rebasing rewrites history and requires a force-push, which will be rejected and force creation of a new PR. A merge commit preserves the branch tip and can be pushed with a normal `git push`.

Every required check on the current head must succeed before approval. Investigate failing logs. `neutral` or `skipped` is acceptable only after verifying that a load-bearing workflow was not bypassed by path filters. Do not approve while required checks are queued or in progress.

For rollup/consolidation PRs, check out the integration head and compile the whole workspace because independently green sub-PRs can fail at their merge boundary.

## Formal Review

Post one formal review tied to the current head:

- **APPROVE** only with adequate required tests, green required CI, no conflicts, and no blockers.
- **REQUEST_CHANGES** for code blockers, missing required tests, conflicts, or merge-blocking CI.
- **COMMENT** when reviewing a self-authored PR that GitHub will not allow the author to approve/request changes. Labels must still reflect the substantive result.

Make the result scannable with `[x] Approved` or `[ ] Changes required`, then list findings first. Include a prior-finding audit with one line per sub-point when prior feedback exists. Never silently disagree with or replay prior feedback.

## Terminal Labels

After posting the formal review, reconcile labels:

```bash
# Approved
scripts/reconcile-pr-labels.sh HOST/OWNER/REPO PR finish approve

# Code changes required, optionally combined with other blockers
scripts/reconcile-pr-labels.sh HOST/OWNER/REPO PR finish changes --needs-tests --conflicts

# Only tests and/or conflicts block the PR
scripts/reconcile-pr-labels.sh HOST/OWNER/REPO PR finish blocked --needs-tests --conflicts
```

Terminal invariants:

- Always remove `REVIEW IN PROGRESS...` and `READY FOR REVIEW` after a completed review.
- `MERGE APPROVED` is incompatible with `NEEDS CHANGES`, `NEEDS TESTS`, and `RESOLVE CONFLICTS`.
- Missing required tests applies `NEEDS TESTS` and blocks approval.
- Conflicts apply `RESOLVE CONFLICTS` and block approval.
- Code blockers apply `NEEDS CHANGES`.
- Every completed review has at least one terminal verdict label.

## Responding To A REQUEST_CHANGES Verdict

When fixing a PR after a `CHANGES_REQUESTED` review:

1. Push the fix commit(s).
2. **Post a comment** on the PR summarizing exactly what was fixed and how each blocker was addressed. Include the commit SHA(s), a one-line description of each change, and (for test fixes) confirm mutation sensitivity was verified.
3. **Reconcile labels**: remove `NEEDS CHANGES`, `NEEDS TESTS`, and `RESOLVE CONFLICTS` as applicable; add `READY FOR REVIEW`.
4. **Re-request review** from the reviewer who requested changes:
   ```bash
   gh api --method POST repos/OWNER/REPO/pulls/PR/requested_reviewers -f "reviewers[]=LOGIN"
   ```

Do not leave the PR in `NEEDS CHANGES` state after pushing a fix without completing steps 2–4.

## Approved Follow-Ups

For non-blocking observations on an otherwise approved PR, choose one disposition:

1. When reviewer edits are explicitly authorized, fix it in the PR when it is small, directly related, low-risk, and does not invalidate review.
2. File or reuse an issue when it has concrete project impact, an actionable acceptance criterion, and is worth future engineering time.
3. Drop speculative, stylistic, already-handled, or generic observations.

Never defer required tests or blockers. Before filing, search open issues and reuse an existing issue when it covers the same work. Link the source PR/review, explain why it was deferred, state concrete acceptance criteria, apply `FOLLOW UP`, assign the PR author when possible, and add it to the repository project backlog with an evidence-based priority. Bundle only closely related items of the same subsystem and priority.

Use this priority rubric, adapting names to the project's available options:

- **P0/Highest**: security or privacy correctness that can fail silently under production configuration.
- **P1/High**: correctness bugs, handler panics, broken user flows, misleading operational signals, or missing authorization-sensitive coverage.
- **P2/Medium**: reconnection risks, scale-dependent performance, authorization-sensitive E2E gaps, or deferred invariant documentation.
- **P3/Low**: bounded cleanup, documentation, developer experience, or defensive handling of currently unreachable states.

Set project status to Backlog and set Priority. Position the item after existing items of equal or higher priority so older peers retain order and lower-priority work remains below it. If project-field or positioning APIs are unavailable, keep the issue and report the incomplete project metadata in the review.
