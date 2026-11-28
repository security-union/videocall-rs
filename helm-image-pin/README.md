helm-image-pin is a small Rust CLI that keeps your Helm charts pinned to an exact image, tag plus digest, so production runs the build you reviewed.

I wrote it after deploying by hand one night and finding out my cluster was still running a build from August. Funny thing is, every chart said `latest`, so nothing looked wrong.

So you tell it in one TOML file which chart uses which image and where each service reports its version, and then it does three things. First it finds the newest build on your main branch, checks it with the registry and rewrites the pin in `values.yaml`. It parses the YAML before and after, that way it can prove it only touched that one value. Then before a deploy it verifies that every pin is still the image the registry serves. And then it waits for each service to say it is running the new sha before you roll the next one.

If a tag got re-pushed or a pin goes backwards, it stops and tells you why.

I would love you to try it out and tell me what breaks.

## Build

```sh
cargo build --release -p helm-image-pin
```

In this repo CI builds it the same way, inside `nix-shell default.nix -A shells.backend-dev`.

## Config

`helm-image-pin.toml` sits at the root of your checkout. Here is a made-up shop with one API:

```toml
branch = "main"                  # builds are tagged main-<first 8 hex of the commit>
github_repo = "acme/shop"        # optional, for compare links in the PR body

[[release]]                      # one per Helm release, in deploy order
name = "api"
helm_release = "api-prod"
chart_dir = "charts/api"         # values file defaults to charts/api/values.yaml
image_key = "api.image"          # api.image.repository and api.image.tag
repository = "acme/api"          # Docker Hub; ghcr.io/acme/api works too

[release.version]                # optional: where the running build says its sha
url = "https://api.acme.example/version.json"
pointer = "/git_sha"
```

Other keys: `tag` (default `"{branch}-{sha}"`), `sha_length` (default 8), `pr_note`, `values_file`, and `version.select = { array = "/components", field = "service", equals = "api" }` for an endpoint that lists several services. This repo's [`helm-image-pin.toml`](../helm-image-pin.toml) uses all of them.

## Commands

```sh
helm-image-pin pin --check                   # newest build per image; --check only reports, exit 1 if stale
helm-image-pin verify --rendered-dir out/    # preflight; out/<name>.yaml is `helm template` output
helm-image-pin wait-version --release api    # poll until api reports its pinned sha (90 s)
helm-image-pin releases [--json]             # deploy order; --json adds each pin and version check
```

`pin --report-dir DIR` also writes the PR `title.txt` and `body.md`. Registry credentials, if you need them, come only from `HELM_IMAGE_PIN_REGISTRY_HOST`, `_USER` and `_PASSWORD`. They go only to that host, or to the https token endpoint it names in its own challenge.

## Exit codes

| code | meaning |
|---|---|
| 0 | done |
| 1 | not converged: `pin --check` found stale pins, or `wait-version` ran out of time |
| 2 | could not decide: network, registry, git, a shallow clone, bad arguments. Retry. |
| 3 | refused: the inputs are wrong or unsafe. A person has to look. |

## What it refuses

It stops on a tag whose digest changed since it was pinned, a pin newer than the newest build (a downgrade), a pin that is not on the first-parent history, a short sha that matches two commits, a repository other than the configured one, and a pinned tag the registry no longer has. In values files it refuses aliases or collections as keys, merge keys, anchors on the tag, and block or escaped tag strings, because then the file it reads might not be the file Helm reads. With `--rendered-dir`, every container image in the render must be exactly the pin.

If a publish job is re-run on the same commit and pushes a new digest, `pin` and `verify` both stop with exit 3. Check who pushed it, then put the new digest in the values file by hand and merge that.

## Limits

It finds builds by listing tags in the registry, so a build only counts once its tag is pushed. The version wait is a liveness signal, not an integrity check; the digest pin is the integrity check. It only edits Helm values files, and only an image block with `repository` and `tag` keys. `image_key` cannot name a key that has a dot in it. A service that reports a sha shorter than the pin, or in uppercase, never matches. This repo's workflows read `helm-image-pin.toml` and the values files with line tools, so they are stricter than the tool about formatting (one key per line, double quotes, no trailing comments on those lines) and stop with an error otherwise. If `helm upgrade` is killed and a release is left in pending-upgrade, later runs stop at that release until someone runs `helm rollback`.

## CI

This repo runs it in [`pin-images-pr.yaml`](../.github/workflows/pin-images-pr.yaml), which opens the pin PR, and in [`deploy-preflight.yaml`](../.github/workflows/deploy-preflight.yaml), the first job of [`deploy-production.yaml`](../.github/workflows/deploy-production.yaml). The deploy job itself does not run `wait-version`: it holds the cluster credentials, so it runs nothing built in the same workflow, and does the same check with curl and jq from `releases --json`.
