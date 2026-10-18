export const repository = "https://github.com/malinoskj2/hindsight";

export interface ReleaseAsset {
  readonly name: string;
  readonly url: string;
  readonly bytes: number;
  readonly sha256: string | undefined;
}

export interface Release {
  readonly version: string;
  readonly publishedAt: string;
  readonly notesUrl: string;
  readonly installer: ReleaseAsset;
  readonly portable: ReleaseAsset;
  readonly checksumsUrl: string;
}

export const installerName = "hindsight-setup-x64.exe";
export const portableName = "hindsight-windows-x64.zip";
export const checksumsName = "SHA256SUMS.txt";

export const latestDownload = (name: string): string =>
  `${repository}/releases/latest/download/${name}`;
