import * as v from "valibot";
import type { Plugin } from "vite-plus";
import {
  checksumsName,
  installerName,
  portableName,
  repository,
  type Release,
  type ReleaseAsset,
} from "../src/release.ts";

const moduleId = "virtual:release";
const resolvedId = `\0${moduleId}`;

const GitHubRelease = v.object({
  tag_name: v.string(),
  html_url: v.pipe(v.string(), v.url()),
  published_at: v.pipe(v.string(), v.isoTimestamp()),
  assets: v.array(
    v.object({
      name: v.string(),
      size: v.pipe(v.number(), v.integer(), v.minValue(0)),
      browser_download_url: v.pipe(v.string(), v.url()),
    }),
  ),
});

type GitHubRelease = v.InferOutput<typeof GitHubRelease>;

const fetchOk = async (url: string, headers: Record<string, string> = {}): Promise<Response> => {
  const response = await fetch(url, { headers });
  if (!response.ok) {
    throw new Error(`${url} answered ${response.status} ${response.statusText}`);
  }
  return response;
};

const parseChecksums = (text: string): ReadonlyMap<string, string> =>
  new Map(
    text
      .split(/\r?\n/)
      .map((line) => /^([0-9a-f]{64})\s+\*?(\S+)$/i.exec(line.replace(/^﻿/, "").trim()))
      .filter((match) => match !== null)
      .map(([, hash = "", name = ""]) => [name, hash.toLowerCase()]),
  );

const findAsset = (release: GitHubRelease, name: string) => {
  const asset = release.assets.find((candidate) => candidate.name === name);
  if (asset === undefined) {
    throw new Error(`${release.tag_name} has no ${name}`);
  }
  return asset;
};

const loadRelease = async (): Promise<Release> => {
  const api = repository.replace("https://github.com/", "https://api.github.com/repos/");
  const token = process.env["GITHUB_TOKEN"];
  const response = await fetchOk(`${api}/releases/latest`, {
    Accept: "application/vnd.github+json",
    ...(token === undefined ? {} : { Authorization: `Bearer ${token}` }),
  });
  const release = v.parse(GitHubRelease, await response.json());

  const checksums = findAsset(release, checksumsName);
  const sums = parseChecksums(await (await fetchOk(checksums.browser_download_url)).text());

  const toAsset = (name: string): ReleaseAsset => {
    const asset = findAsset(release, name);
    return {
      name,
      url: asset.browser_download_url,
      bytes: asset.size,
      sha256: sums.get(name),
    };
  };

  return {
    version: release.tag_name.replace(/^v/, ""),
    publishedAt: release.published_at,
    notesUrl: release.html_url,
    installer: toAsset(installerName),
    portable: toAsset(portableName),
    checksumsUrl: checksums.browser_download_url,
  };
};

export const release = (): Plugin => {
  let isBuild = false;
  let pending: Promise<Release | null> | undefined;

  const load = async (): Promise<Release | null> => {
    try {
      return await loadRelease();
    } catch (error) {
      if (isBuild) {
        throw new Error("Could not read the latest hindsight release", { cause: error });
      }
      console.warn("Serving without release details:", error);
      return null;
    }
  };

  return {
    name: "hindsight:release",
    configResolved(config) {
      isBuild = config.command === "build";
    },
    resolveId(id) {
      return id === moduleId ? resolvedId : undefined;
    },
    async load(id) {
      if (id !== resolvedId) {
        return undefined;
      }
      pending ??= load();
      return `export default ${JSON.stringify(await pending)};`;
    },
  };
};
