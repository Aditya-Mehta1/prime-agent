/**
 * Download-origin URL rules shared by every code path that fetches release metadata.
 *
 * This module is deliberately dependency-free: it is loaded by the lightweight npm bridge and the
 * version check, which must not pull the signature verifier (and its Sigstore dependencies) into
 * every process start.
 */

/**
 * Parse a download origin into a canonical `https://host[:port][/path]` string.
 *
 * The result is appended to (`/latest.json`, `/releases/v<version>/SHA256SUMS`), so anything that
 * would change the meaning of that suffix is refused rather than repaired: a query string or fragment
 * (the suffix would land inside them), embedded credentials (they would be sent to every download
 * host and shown in the update UI), and any scheme other than https. Trailing slashes are dropped so
 * paths are always joined with exactly one `/`. `label` names the setting in error messages.
 */
export function parseDownloadBaseUrl(raw: string, label = "The download base URL"): string {
	const trimmed = raw.trim();
	let parsed: URL;
	try {
		parsed = new URL(trimmed);
	} catch {
		throw new Error(`${label} is not a valid URL: ${raw}`);
	}
	if (parsed.protocol !== "https:") throw new Error(`${label} must use https, got ${parsed.protocol}//.`);
	if (parsed.username || parsed.password) throw new Error(`${label} must not contain credentials.`);
	// `new URL("https://x?").search` and `new URL("https://x#").hash` are both "", so check the raw text too.
	if (parsed.search || trimmed.includes("?")) throw new Error(`${label} must not contain a query string.`);
	if (parsed.hash || trimmed.includes("#")) throw new Error(`${label} must not contain a fragment.`);
	if (!parsed.hostname) throw new Error(`${label} must name a host.`);
	const pathname = parsed.pathname.replace(/\/+$/, "");
	return `${parsed.origin}${pathname}`;
}
