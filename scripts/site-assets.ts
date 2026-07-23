import { mkdir, copyFile } from "node:fs/promises";
import { join } from "node:path";

const destination = join(import.meta.dir, "..", "docs", "static", "fonts");

const fonts = [
  [
    "@fontsource-variable/source-sans-3/files/source-sans-3-latin-wght-normal.woff2",
    "source-sans-3-latin-wght-normal.woff2",
  ],
  [
    "@fontsource-variable/source-sans-3/files/source-sans-3-latin-wght-italic.woff2",
    "source-sans-3-latin-wght-italic.woff2",
  ],
  [
    "@fontsource-variable/source-sans-3/files/source-sans-3-latin-ext-wght-normal.woff2",
    "source-sans-3-latin-ext-wght-normal.woff2",
  ],
  [
    "@fontsource-variable/source-sans-3/files/source-sans-3-latin-ext-wght-italic.woff2",
    "source-sans-3-latin-ext-wght-italic.woff2",
  ],
  [
    "@fontsource-variable/jetbrains-mono/files/jetbrains-mono-latin-wght-normal.woff2",
    "jetbrains-mono-latin-wght-normal.woff2",
  ],
  [
    "@fontsource-variable/jetbrains-mono/files/jetbrains-mono-latin-ext-wght-normal.woff2",
    "jetbrains-mono-latin-ext-wght-normal.woff2",
  ],
] as const;

await mkdir(destination, { recursive: true });

await Promise.all(
  fonts.map(async ([specifier, filename]) => {
    const source = Bun.resolveSync(specifier, import.meta.dir);
    await copyFile(source, join(destination, filename));
  }),
);
