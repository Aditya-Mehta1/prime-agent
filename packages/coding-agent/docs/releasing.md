# Releasing Prime Agent

This page describes how a Prime Agent release happens: what you do, what CI does, and what to check.
For the trust model behind it, see [Release security](release-security.md).

## In one sentence

A release is a merged pull request that bumps the version; everything after the merge is automated,
and anything that is *not* an approved version-bump pull request stops and waits for a reviewer.

## The normal release

1. **Prepare the release.** Run `npm run release:patch` or `npm run release:minor` (no major
   releases by policy: patch for fixes and features, minor for breaking changes). The script bumps
   the version in lockstep across the published packages, folds the `.changes/*.md` fragments into
   the changelogs, commits on a `release/vX.Y.Z` branch and pushes that branch. It does not push
   `main`, does not create the tag, and does not publish anything.
2. **Open a pull request from that branch, get it reviewed, and merge it.** This is the approval
   gate. The release workflow later confirms that the merge commit belongs to a merged pull request
   with at least one approving review from a human; bot approvals do not count.
3. **CI takes over.** The `Release Prime Agent` workflow runs on the merge:

   | Job | What it does | Secrets |
   |---|---|---|
   | `context` | Resolves the version and decides whether the release may publish unattended | none |
   | `standalone` | Compiles the four platform binaries with Bun and signs macOS binaries ad hoc | none |
   | `build` | Packs the release archives and npm-shaped tarballs | none |
   | `validate-macos` | Re-verifies macOS signatures and matches every `executableSha256` receipt | none |
   | `assemble` | Renders the installer, verifies receipts, produces the final artifact set | none |
   | `sign` | cosign keyless signature over `SHA256SUMS`, SBOM per archive, build provenance | OIDC only |
   | `github-release` | Creates a **draft** release, uploads assets, records GitHub's digests | `GITHUB_TOKEN` |
   | `publish-r2` | Verifies artifacts against those digests, uploads immutably, moves the channel | R2, step-scoped |
   | `finalize-release` | Publishes the release and creates the `vX.Y.Z` tag **last** | `GITHUB_TOKEN` |
   | `verify` | Re-downloads from the public URL, verifies the signature, runs the binary | none |
   | `publish-npm` | Publishes the registry packages over OIDC with provenance | OIDC only |
   | `tap-bump` | Opens the Homebrew formula bump | tap token |

4. **Watch `verify`.** It installs the way a user does and fails the run if anything does not match.

Nothing is public before the artifacts pass verification: the GitHub release is drafted first, and
the git tag is created only after R2 accepted the upload.

## Nightly (beta)

Every merge to `main` publishes a beta build. It runs in its own job with its own credential and can
only write the beta prefix and the beta pointers. It cannot write `stable`, `latest.json`, or
`install.sh`, and it creates no version tag. Nightly never waits for a reviewer.

Install or test a nightly build with the beta channel:

```sh
curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh | PRIME_AGENT_RELEASE_CHANNEL=beta sh
```

## Break-glass releases

Some publications cannot point at an approved release pull request:

- `workflow_dispatch` with a `release_tag` input
- a version bump that reached `main` without a pull request
- a retry of a failed release that rides on an unrelated merge

These still run, but `context` routes them to the `release-manual` environment, which requires a
reviewer who did not trigger the run. Pushing a `v*` tag does **not** start a release at all; the tag
is an output of the release, never an input.

## If a release fails

- **Before `publish-r2`:** nothing was published. Fix the problem and re-run.
- **During `publish-r2`:** uploads refuse to overwrite an existing object in `releases/vX.Y.Z/`, so a
  retry is safe. The channel pointer moves only after every object is verified and read back.
- **After publication:** the release is immutable. Ship a new patch version; do not rewrite a
  published version.
- A version whose tag already points at a different commit is refused outright.

## Checking a published release by hand

```sh
BASE=https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev
VERSION=0.9.5
curl -fsSLO "$BASE/releases/v$VERSION/SHA256SUMS"
curl -fsSLO "$BASE/releases/v$VERSION/SHA256SUMS.sigstore.json"
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/PrimeIntellect-ai/prime-agent/.github/workflows/build-binaries.yml@refs/heads/main" \
  SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

`prime-agent update` performs the equivalent check automatically and refuses to install anything that
fails it.

## Operator setup

The workflow expects these GitHub environments. Each holds one credential and is limited to `main`:

| Environment | Holds | Notes |
|---|---|---|
| `release-r2` | `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `R2_BUCKET`, `R2_ENDPOINT_URL` | unattended production publishing |
| `release-manual` | the same R2 secrets | required reviewers, self-review disabled |
| `nightly-r2` | `NIGHTLY_R2_*` | a token scoped to the beta prefix only |
| `release-npm` | nothing | npm trusted publishing over OIDC |
| `release-homebrew` | `HOMEBREW_TAP_TOKEN` | formula bump pull requests |

Two repository variables act as switches: `NPM_PUBLISH_ENABLED` enables npm publishing, and
`HOMEBREW_TAP_REPO` enables the tap bump. Both jobs do nothing until they are set.
