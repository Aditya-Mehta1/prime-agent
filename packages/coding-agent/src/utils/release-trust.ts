/**
 * Pinned trust anchors for compiled Prime Agent releases.
 *
 * Everything the self-updater is willing to trust is declared here, in one auditable place. The
 * release workflow signs `SHA256SUMS` with a cosign keyless signature; the updater then refuses any
 * artifact whose digest is not listed in a `SHA256SUMS` carrying a signature from EXACTLY this
 * repository, EXACTLY this workflow file and a ref matching EXACTLY this pattern.
 *
 * These values are deliberately NOT configurable at runtime. `PRIME_AGENT_DOWNLOAD_BASE_URL` may
 * move the download origin for development, but it cannot relax or replace anything below.
 */

/** The only repository whose releases this build will install. */
export const RELEASE_SIGNER_REPOSITORY = "PrimeIntellect-ai/prime-agent";

/** `SourceRepositoryURI` recorded in the signing certificate. */
export const RELEASE_SIGNER_REPOSITORY_URI = `https://github.com/${RELEASE_SIGNER_REPOSITORY}`;

/** The only workflow file allowed to produce a release signature. */
export const RELEASE_SIGNER_WORKFLOW_PATH = ".github/workflows/build-binaries.yml";

/** The OIDC issuer that must have minted the signing certificate. */
export const RELEASE_SIGNER_OIDC_ISSUER = "https://token.actions.githubusercontent.com";

/** Self-hosted runners are not part of the release path, so the certificate must say github-hosted. */
export const RELEASE_SIGNER_RUNNER_ENVIRONMENT = "github-hosted";

/**
 * Refs the release workflow is allowed to sign from: `main` (the normal merge-triggered release) and
 * `v<semver>` tags (the break-glass path). Any other ref - a feature branch, a fork, a pull request
 * ref - is rejected even though the certificate is otherwise valid.
 */
export const RELEASE_SIGNER_REF_PATTERN = /^refs\/(?:heads\/main|tags\/v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)$/;

/** Fulcio X.509 extension OIDs (https://github.com/sigstore/fulcio/blob/main/docs/oid-info.md). */
export const FULCIO_OID_SOURCE_REPOSITORY_URI = "1.3.6.1.4.1.57264.1.12";
export const FULCIO_OID_RUNNER_ENVIRONMENT = "1.3.6.1.4.1.57264.1.11";
export const FULCIO_OID_BUILD_SIGNER_URI = "1.3.6.1.4.1.57264.1.9";

/** Release assets the updater fetches, relative to `<base url>/releases/v<version>/`. */
export const RELEASE_CHECKSUMS_ASSET = "SHA256SUMS";
/** cosign `sign-blob --bundle` output for {@link RELEASE_CHECKSUMS_ASSET}. */
export const RELEASE_SIGNATURE_BUNDLE_ASSET = "SHA256SUMS.sigstore.json";

/** The identity policy the verifier enforces. Exported as data so tests can show what is pinned. */
export interface PinnedSignerIdentity {
	/** `SourceRepositoryURI` - the repository whose commit was built. */
	repositoryUri: string;
	/**
	 * The repository that owns the workflow file named in the certificate subject. Identical to
	 * {@link PinnedSignerIdentity.repositoryUri} for us, because we do not sign from a reusable
	 * workflow hosted in another repository. Kept separate so the distinction stays checkable.
	 */
	workflowRepositoryUri: string;
	workflowPath: string;
	oidcIssuer: string;
	runnerEnvironment: string;
	refPattern: RegExp;
}

export const PINNED_RELEASE_SIGNER: PinnedSignerIdentity = {
	repositoryUri: RELEASE_SIGNER_REPOSITORY_URI,
	workflowRepositoryUri: RELEASE_SIGNER_REPOSITORY_URI,
	workflowPath: RELEASE_SIGNER_WORKFLOW_PATH,
	oidcIssuer: RELEASE_SIGNER_OIDC_ISSUER,
	runnerEnvironment: RELEASE_SIGNER_RUNNER_ENVIRONMENT,
	refPattern: RELEASE_SIGNER_REF_PATTERN,
};

/** The certificate SAN a release signature must carry for `ref`. */
export function buildExpectedSignerIdentity(identity: PinnedSignerIdentity, ref: string): string {
	return `${identity.workflowRepositoryUri}/${identity.workflowPath}@${ref}`;
}

/** Splits a build-signer SAN back into its workflow URI and ref halves. */
export function parseSignerIdentity(subjectAlternativeName: string): { workflowUri: string; ref: string } | undefined {
	const separator = subjectAlternativeName.lastIndexOf("@");
	if (separator <= 0) return undefined;
	return {
		workflowUri: subjectAlternativeName.slice(0, separator),
		ref: subjectAlternativeName.slice(separator + 1),
	};
}
