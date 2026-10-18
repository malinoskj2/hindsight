const date = new Intl.DateTimeFormat("en-GB", { dateStyle: "long", timeZone: "UTC" });

export const formatDate = (iso: string): string => date.format(new Date(iso));

export const formatBytes = (bytes: number): string => `${(bytes / 1024 ** 2).toFixed(1)} MB`;
