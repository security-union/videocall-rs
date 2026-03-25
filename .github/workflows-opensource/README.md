# Opensource Workflows (Archived)

This directory contains GitHub Actions workflows that target **github.com** (opensource repository) runners.

## Why are these here?

These workflows were moved out of `.github/workflows/` to **declutter the GitHub Actions UI** on our GitHub Enterprise Server (github01.hclpnp.com).

Since these workflows use `runs-on: ubuntu-latest` (GitHub.com-hosted runners), they would fail to schedule on github01 and create notification spam. Moving them to this subdirectory:
- ✅ Removes them from the Actions UI on github01
- ✅ Preserves git history for merges from `main` → `hcl-main`
- ✅ Keeps them available for the opensource repository on github.com

## Active Workflows

For **HCL-specific workflows** (self-hosted runners, hcl-main branch), see:
- `.github/workflows/daily-build-images-hcl.yaml`
- `.github/workflows/daily-deploy-hcl.yaml`
- `.github/workflows/pr-*-hcl.yaml`

## Note on Merges

When merging from `main` → `hcl-main`:
- Git will continue tracking updates to these files in the subdirectory
- New workflows added to `main` will land in `.github/workflows/` and may need to be moved here
- This is intentional to keep the HCL Actions UI clean
