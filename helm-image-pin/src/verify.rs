// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deploy preflight: is every pin safe to roll out as written?
//!
//! Reading values.yaml is not enough on its own: the image string that ships
//! is built by the chart's templates, so a template change (a hardcoded
//! image, a sidecar, a Job, a different field) would slip past a values
//! check. When `--rendered-dir` is given, every document of the release's
//! `helm template` output is walked, and every `containers`,
//! `initContainers` and `ephemeralContainers` list found anywhere in it
//! (Deployments, StatefulSets, DaemonSets, Jobs, CronJobs, bare Pods, and
//! custom resources that embed a pod spec) contributes its images. That set
//! must be exactly `{ <repository>:<pin> }`.

use crate::config::Release;
use crate::error::{Error, Result};
use crate::git::Line;
use crate::pin::{Pin, TagScheme};
use crate::registry::Registry;
use crate::values;
use std::collections::BTreeSet;
use yaml_rust2::{Yaml, YamlLoader};

const CONTAINER_LISTS: [&str; 3] = ["containers", "initContainers", "ephemeralContainers"];

/// Check one release's pin. `rendered` is its `helm template` output, if any.
pub fn check(
    release: &Release,
    values_src: &str,
    rendered: Option<&str>,
    line: &Line,
    registry: &dyn Registry,
    scheme: &TagScheme,
) -> Result<Pin> {
    let block = values::read_image(&release.values_file, values_src, &release.image_key)?;
    if block.repository != release.image.written {
        return Err(Error::Refused(format!(
            "{}: {}.repository is {:?}, only {:?} may deploy here",
            release.name,
            release.image_key_display(),
            block.repository,
            release.image.written
        )));
    }
    let pin = scheme.pin(&block.tag).ok_or_else(|| {
        Error::Refused(format!(
            "{}: tag {:?} is not {}@sha256:<64 hex>",
            release.name,
            block.tag,
            scheme.placeholder()
        ))
    })?;
    if line.find(&pin.sha)?.is_none() {
        return Err(Error::Refused(format!(
            "{}: {} is not a commit on the first-parent history",
            release.name, pin.tag
        )));
    }
    if let Some(rendered) = rendered {
        let expected = format!("{}:{}", release.image.written, pin);
        let images = rendered_images(&release.name, rendered)?;
        if images.len() != 1 || !images.contains(&expected) {
            return Err(Error::Refused(format!(
                "{}: helm template renders {images:?}, expected exactly {expected:?}",
                release.name
            )));
        }
    }
    let served = registry
        .digest(&release.image, &pin.tag)
        .map_err(|e| match e {
            Error::NotFound(what) => Error::NotFound(format!(
                "{what}; the pinned tag is gone from the registry, so there is nothing to deploy"
            )),
            other => other,
        })?;
    if served != pin.digest {
        return Err(Error::Refused(format!(
            "{}: {}:{} now serves {served}, pin says {}; the tag was re-pushed",
            release.name, release.image, pin.tag, pin.digest
        )));
    }
    Ok(pin)
}

fn collect(node: &Yaml, label: &str, images: &mut BTreeSet<String>) -> Result<()> {
    match node {
        Yaml::Hash(map) => {
            for (key, value) in map {
                if let (Some(name), Yaml::Array(list)) = (key.as_str(), value)
                    && CONTAINER_LISTS.contains(&name)
                {
                    for container in list {
                        let image = container["image"].as_str().ok_or_else(|| {
                            Error::values(label, format!("a {name} entry has no string image"))
                        })?;
                        images.insert(image.to_string());
                    }
                }
                collect(value, label, images)?;
            }
        }
        Yaml::Array(items) => {
            for item in items {
                collect(item, label, images)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Every container image anywhere in a rendered release.
pub fn rendered_images(name: &str, rendered: &str) -> Result<BTreeSet<String>> {
    let label = format!("rendered {name}");
    let docs = YamlLoader::load_from_str(rendered)
        .map_err(|e| Error::values(&label, format!("not valid YAML: {e}")))?;
    let mut images = BTreeSet::new();
    for doc in &docs {
        collect(doc, &label, &mut images)?;
    }
    if images.is_empty() {
        return Err(Error::values(&label, "renders no containers at all"));
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{FakeRegistry, digest, line, release, scheme, sha, values_doc};

    const REPO: &str = "acme/relay";

    fn rel() -> Release {
        release("ws", REPO)
    }

    fn values(tag: &str) -> String {
        values_doc("ws", REPO, tag)
    }

    fn deployment(image: &str) -> String {
        format!(
            "---\nkind: Service\nmetadata:\n  name: ws-prod\n---\nkind: Deployment\nmetadata:\n  name: ws-prod\nspec:\n  template:\n    spec:\n      containers:\n        - name: ws\n          image: \"{image}\"\n"
        )
    }

    fn good() -> String {
        format!("main-{}@{}", sha(6), digest(6))
    }

    fn good_image() -> String {
        format!("{REPO}:{}", good())
    }

    fn registry() -> FakeRegistry {
        FakeRegistry::default().with(REPO, &[(6, 6)])
    }

    fn run(values_src: &str, rendered: Option<&str>) -> Result<Pin> {
        check(
            &rel(),
            values_src,
            rendered,
            &line(10),
            &registry(),
            &scheme(),
        )
    }

    #[test]
    fn accepts_a_good_pin_with_a_matching_render() {
        let got = run(&values(&good()), Some(&deployment(&good_image())));
        assert_eq!(got.unwrap().sha, sha(6));
    }

    #[test]
    fn rejects_wrong_repo_bare_tag_non_build_and_off_history() {
        let wrong_repo = values(&good()).replace(REPO, "acme/api");
        for (doc, needle) in [
            (wrong_repo, "only \"acme/relay\""),
            (
                values(&format!("main-{}", sha(6))),
                "is not main-<sha8>@sha256",
            ),
            (
                values(&format!("pr-123@{}", digest(6))),
                "is not main-<sha8>@sha256",
            ),
            (values("latest"), "is not main-<sha8>@sha256"),
            (
                values(&format!("main-deadbeef@{}", digest(6))),
                "not a commit on the first-parent history",
            ),
        ] {
            let err = run(&doc, None).unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
            assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
        }
    }

    #[test]
    fn rejects_a_re_pushed_tag() {
        let pinned = format!("main-{}@{}", sha(6), digest(99));
        let err = run(&values(&pinned), None).unwrap_err();
        assert!(err.to_string().contains("re-pushed"), "{err}");
    }

    #[test]
    fn a_pinned_tag_missing_from_the_registry_is_refused_not_retried() {
        let pinned = format!("main-{}@{}", sha(4), digest(4));
        let err = run(&values(&pinned), None).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
    }

    #[test]
    fn walks_every_kind_of_pod_spec() {
        let extra = |doc: &str| format!("{}{doc}", deployment(&good_image()));
        let cases = [
            "---\nkind: Job\nspec:\n  template:\n    spec:\n      containers:\n        - image: busybox\n",
            "---\nkind: CronJob\nspec:\n  jobTemplate:\n    spec:\n      template:\n        spec:\n          containers:\n            - image: busybox\n",
            "---\nkind: StatefulSet\nspec:\n  template:\n    spec:\n      initContainers:\n        - image: busybox\n",
            "---\nkind: DaemonSet\nspec:\n  template:\n    spec:\n      containers:\n        - image: busybox\n",
            "---\nkind: Pod\nspec:\n  ephemeralContainers:\n    - image: busybox\n",
            "---\nkind: Deployment\nmetadata:\n  name: another-name\nspec:\n  template:\n    spec:\n      containers:\n        - image: busybox\n",
            "---\nkind: Rollout\napiVersion: argoproj.io/v1alpha1\nspec:\n  template:\n    spec:\n      containers:\n        - image: busybox\n",
            "---\nkind: List\nitems:\n  - kind: Pod\n    spec:\n      containers:\n        - image: busybox\n",
        ];
        for case in cases {
            let err = run(&values(&good()), Some(&extra(case))).unwrap_err();
            assert!(err.to_string().contains("busybox"), "{case}: {err}");
            assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
        }
        // A second workload with the same, correct image is fine.
        let same = format!(
            "---\nkind: Job\nspec:\n  template:\n    spec:\n      containers:\n        - image: \"{}\"\n",
            good_image()
        );
        assert!(run(&values(&good()), Some(&extra(&same))).is_ok());
    }

    #[test]
    fn rejects_a_render_that_disagrees_or_is_empty() {
        let other = deployment(&format!("{REPO}:latest"));
        assert!(run(&values(&good()), Some(&other)).is_err());
        let sidecar = format!(
            "{}        - name: extra\n          image: busybox\n",
            deployment(&good_image())
        );
        assert!(run(&values(&good()), Some(&sidecar)).is_err());
        let no_pods = "kind: Service\nmetadata:\n  name: ws-prod\n";
        let err = run(&values(&good()), Some(no_pods)).unwrap_err();
        assert!(err.to_string().contains("no containers"), "{err}");
        let no_image = "kind: Pod\nspec:\n  containers:\n    - name: x\n";
        assert!(run(&values(&good()), Some(no_image)).is_err());
    }
}
