import { build } from "esbuild";
import { createHash } from "node:crypto";
import { copyFile, readdir, readFile, writeFile } from "node:fs/promises";

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
await Promise.all((await readdir("sounds"))
  .filter((filename) => filename.endsWith(".mp3"))
  .map((filename) => copyFile(`sounds/${filename}`, `dist/assets/${filename}`)));
// A reload after cutover must fetch the current recovery client, even when
// the previous release used the same asset filenames.
const revision = createHash("sha256")
  .update(await readFile("dist/assets/app.js"))
  .update(await readFile("dist/assets/app.css"))
  .digest("hex");
await writeFile("dist/index.html", `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Tempo</title><link rel="stylesheet" href="/assets/app.css?v=${revision}"></head><body><div id="root"></div><script type="module" src="/assets/app.js?v=${revision}"></script></body></html>`);
