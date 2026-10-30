import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { defineConfig, type Plugin } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// `npm run dev` proxies the API to a local `pipette-mgmt serve`.
const API = process.env.PIPETTE_MGMT_URL ?? "http://127.0.0.1:3000";

/** The operator API, every route the app calls (`router.rs`). */
const PREFIXES = ["/api/v1"];

/**
 * Lists every npm package the bundle carries, as `name version` lines, for CI to
 * check against the credits in `about.hbs`. Outside `dist/`, which the server
 * embeds whole.
 */
function bundledPackages(): Plugin {
  return {
    name: "bundled-packages",
    generateBundle(_, bundle) {
      const names = new Set<string>();
      for (const chunk of Object.values(bundle)) {
        if (chunk.type !== "chunk") continue;
        for (const id of Object.keys(chunk.modules)) {
          const name = /node_modules\/((?:@[^/]+\/)?[^/]+)\//.exec(id)?.[1];
          if (name) names.add(name);
        }
      }
      const lines = [...names].sort().map((name) => {
        const { version } = JSON.parse(readFileSync(`node_modules/${name}/package.json`, "utf8"));
        return `${name} ${version}\n`;
      });
      mkdirSync("node_modules/.cache", { recursive: true });
      writeFileSync("node_modules/.cache/bundled-packages.txt", lines.join(""));
    },
  };
}

export default defineConfig({
  base: "/app/",
  plugins: [svelte(), bundledPackages()],
  // One chunk, so nothing to preload, and Vite's own code stays out of the bundle.
  build: { modulePreload: { polyfill: false } },
  server: {
    // The object form, as a bare URL turns on changeOrigin: the signed `@authority` is the page's Host.
    proxy: Object.fromEntries(PREFIXES.map((prefix) => [prefix, { target: API }])),
  },
});
