import { build } from "esbuild";
import { readFile, writeFile } from "node:fs/promises";

const license = await readFile("node_modules/chess.js/LICENSE", "utf8");
await build({
  entryPoints: ["src/main.jsx"],
  bundle: true,
  format: "esm",
  outdir: "dist/assets",
  entryNames: "app",
  assetNames: "[name]-[hash]",
  loader: { ".svg": "file" },
  banner: { js: `/* chess.js\n${license}*/` },
});
await writeFile("dist/index.html", '<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Tempo</title><link rel="stylesheet" href="/assets/app.css"></head><body><div id="root"></div><script type="module" src="/assets/app.js"></script></body></html>');
