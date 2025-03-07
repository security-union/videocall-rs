#!/usr/bin/env bash
#
# Usage:
#   ./git-squash-all.sh [feature_branch] [base_branch] [squash_branch_name]
#
# Example:
#   ./git-squash-all.sh branch_a master branch_a_squashed
#
# Description:
#   This script creates or resets a "squash branch" off the base branch
#   and merges changes from the feature branch into a single commit.

set -e

FEATURE_BRANCH=${1:-branch_a}
BASE_BRANCH=${2:-master}
SQUASH_BRANCH=${3:-branch_a_squashed}

echo "===== Squashing ALL commits from '$FEATURE_BRANCH' into ONE commit on top of '$BASE_BRANCH' ====="

# 1. Checkout base branch and pull latest
git checkout "$BASE_BRANCH"
git pull --ff-only origin "$BASE_BRANCH"

# 2. Create/Reset the squash branch
git checkout -B "$SQUASH_BRANCH" "$BASE_BRANCH"

# 3. Merge --squash the feature branch
git merge --squash "$FEATURE_BRANCH"

# 4. Commit the squashed changes
git commit -m "Squash all commits from $FEATURE_BRANCH into one."

echo "===== Done squashing into branch '$SQUASH_BRANCH' ====="
echo "If needed, push with 'git push --force origin $SQUASH_BRANCH' (use caution!)."
