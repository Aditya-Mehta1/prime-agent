Self-updates now require a cosign signature. `prime-agent update` verifies a Sigstore bundle over
`SHA256SUMS` before it trusts any digest, with the signer pinned to the Prime Intellect repository,
the release workflow file and an allowed ref. A missing, broken or foreign signature refuses the
update and keeps the installed version.

`PRIME_AGENT_DOWNLOAD_BASE_URL` can still move the download origin for development, but it no longer
weakens anything: it must be an https URL, verification and the pinned identity are unchanged, and
the override is reported in the update plan instead of silently replacing the recorded install
source.

Homebrew and npm copies are detected correctly. A compiled binary installed in a Homebrew keg now
reports `homebrew` and is updated with `brew upgrade prime-agent` instead of overwriting the keg; a
compiled binary installed under `node_modules` reports `npm`.
