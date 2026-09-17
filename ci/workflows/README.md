# CI workflows (staged)

The GitHub Actions workflows for this repo live here, byte-identical to their
final form, because the dev box's GitHub token has no `workflow` scope and
cannot push anything under `.github/workflows/` (see
docs/installer-ci-design.md §3 and PR #76 for the same constraint).

Local gates mirror every workflow step today:

    make check            # fmt + clippy + test + release build (ci.yml fmt/clippy-test jobs)
    make deny             # cargo-deny advisories + licenses (ci.yml deny job)
    make actionlint       # validates the staged workflow files
    make release-dry-run  # local mirror of the release build-job gates (release.yml)

## Promotion (once the token gains `workflow` scope)

    git mv ci/workflows/ci.yml ci/workflows/release.yml .github/workflows/
    git rm ci/workflows/README.md

Nothing else changes — the files are inert until Actions is enabled anyway.
