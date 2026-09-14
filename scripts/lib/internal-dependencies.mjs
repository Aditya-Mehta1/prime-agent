/**
 * Shared dependency rewriting for the two release packers.
 *
 * There are two distribution channels with different trust models:
 *
 *   R2 channel      scripts/pack-prime-agent-release.mjs
 *                   Internal workspace dependencies become absolute tarball URLs because those
 *                   artifacts only exist in the bucket. Unchanged behaviour - the installer path
 *                   depends on it.
 *
 *   Registry channel scripts/pack-npm-packages.mjs
 *                   Internal workspace dependencies become semver ranges against packages that are
 *                   actually published to registry.npmjs.org, so npm can verify registry signatures,
 *                   pin integrity hashes in the consumer lockfile, and cover the graph with
 *                   provenance attestations. A tarball URL in a published package would let anyone
 *                   holding the R2 key change what installs under an already published version.
 */

/**
 * Replace dependency specifiers whose key is present in `replacements`. Keys are always the source
 * workspace package names, because compiled output imports those specifiers literally.
 */
export function rewriteInternalDependencies(dependencies, replacements) {
	if (!dependencies) return undefined;
	const rewritten = {};
	for (const [name, range] of Object.entries(dependencies)) {
		const replacement = replacements.get(name);
		rewritten[name] = replacement === undefined ? range : replacement;
	}
	return rewritten;
}

/** R2 channel specifier: an absolute tarball URL inside the release prefix. */
export function tarballDependencySpec(baseUrl, version, tarballFile) {
	return `${baseUrl}/releases/v${version}/${tarballFile}`;
}

/**
 * Registry channel specifier. When the published name differs from the imported name (it does: the
 * compiled output still imports `@earendil-works/pi-*`), npm alias syntax keeps the import specifier
 * working while resolving to the package this project owns. `npm:` aliases are understood by npm,
 * pnpm, yarn and bun.
 */
export function registryDependencySpec(sourceName, registryName, version, options = {}) {
	const range = options.exact ? version : `^${version}`;
	return sourceName === registryName ? range : `npm:${registryName}@${range}`;
}

const FORBIDDEN_SPEC = /^(?:https?|git|git\+[a-z]+|file|link|portal|workspace|github):/i;

/**
 * Fail closed if a package that is about to be published to the registry declares a dependency that
 * npm cannot verify (tarball URL, git URL, file/workspace link).
 */
export function assertRegistryDependencies(packageJson) {
	for (const field of ["dependencies", "optionalDependencies", "peerDependencies"]) {
		for (const [name, spec] of Object.entries(packageJson[field] || {})) {
			if (typeof spec !== "string" || FORBIDDEN_SPEC.test(spec) || spec.endsWith(".tgz")) {
				throw new Error(
					`${packageJson.name}: ${field}["${name}"] must be a registry range for a published package, got "${spec}"`,
				);
			}
		}
	}
}
