import { mdsvex } from "mdsvex";
import adapter from "@sveltejs/adapter-node";

/** @type {import('@sveltejs/kit').Config} */
const config = {
	compilerOptions: {
		// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
		runes: ({ filename }) =>
			filename.split(/[/\\]/).includes("node_modules") ? undefined : true,
	},
	kit: {
		// adapter-auto only supports some environments, see https://svelte.dev/docs/kit/adapter-auto for a list.
		// If your environment is not supported, or you settled on a specific environment, switch out the adapter.
		// See https://svelte.dev/docs/kit/adapters for more information about adapters.
		adapter: adapter(),
		// SvelteKit adds a per-response nonce to its own inline hydration script and to this header.
		// A fixed `script-src 'self'` set in hooks blocked that script, so production pages never left
		// "Loading…". The theme bootstrap stays an external file, covered by 'self'. The remaining
		// headers (X-Frame-Options and friends) are still set in hooks.server.ts.
		csp: {
			mode: "nonce",
			directives: {
				"default-src": ["self"],
				"script-src": ["self"],
				"style-src": ["self", "unsafe-inline"],
				"img-src": ["self", "data:"],
				"connect-src": ["self"],
				"frame-ancestors": ["none"],
				"base-uri": ["self"],
				"object-src": ["none"],
				"form-action": ["self"],
			},
		},
	},
	preprocess: [mdsvex({ extensions: [".svx", ".md"] })],
	extensions: [".svelte", ".svx", ".md"],
};

export default config;
