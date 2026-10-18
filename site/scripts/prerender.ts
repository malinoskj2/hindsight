import { readFile, rm, writeFile } from "node:fs/promises";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const page = `${root}dist/index.html`;
const server = `${root}dist-ssr`;
const outlet = '<div id="app"></div>';

const entry: unknown = await import(pathToFileURL(`${server}/entry-server.js`).href);
if (
  typeof entry !== "object" ||
  entry === null ||
  !("render" in entry) ||
  typeof entry.render !== "function"
) {
  throw new Error(`${server}/entry-server.js does not export render`);
}
const markup: unknown = await entry.render();
if (typeof markup !== "string") {
  throw new Error("render did not return a string");
}

const html = await readFile(page, "utf8");
if (!html.includes(outlet)) {
  throw new Error(`${page} has no ${outlet}`);
}
await writeFile(page, html.replace(outlet, `<div id="app">${markup}</div>`));
await rm(server, { recursive: true });
