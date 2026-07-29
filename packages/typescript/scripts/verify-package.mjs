#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import { readFileSync, lstatSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { isDeepStrictEqual } from "node:util";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const allowlist = JSON.parse(
  readFileSync(join(packageRoot, "scripts/package-allowlist.json"), "utf8"),
);

function fail(message) {
  process.stderr.write(`package verification failed: ${message}\n`);
  process.exit(1);
}

function packProjection() {
  const result = spawnSync("npm", ["pack", "--json", "--dry-run"], {
    cwd: packageRoot,
    encoding: "utf8",
    maxBuffer: 8 * 1024 * 1024,
  });
  if (result.status !== 0) fail(`npm pack exited ${result.status}: ${result.stderr.trim()}`);
  let decoded;
  try {
    decoded = JSON.parse(result.stdout);
  } catch {
    fail("npm pack stdout is not JSON");
  }
  const entries = Array.isArray(decoded) ? decoded : Object.values(decoded);
  if (entries.length !== 1) fail("npm pack returned an unexpected package count");
  const entry = entries[0];
  if (!entry || !Array.isArray(entry.files)) fail("npm pack omitted its file inventory");
  return {
    entryCount: entry.entryCount,
    filename: entry.filename,
    files: entry.files.map(({ path, size, mode }) => ({ mode, path, size })),
    integrity: entry.integrity,
    shasum: entry.shasum,
    size: entry.size,
    unpackedSize: entry.unpackedSize,
  };
}

if (
  !Array.isArray(allowlist)
  || allowlist.some((path) => typeof path !== "string")
  || new Set(allowlist).size !== allowlist.length
) {
  fail("checked-in allowlist is malformed or contains duplicates");
}
const sortedAllowlist = [...allowlist].sort();
if (!isDeepStrictEqual(allowlist, sortedAllowlist)) fail("checked-in allowlist is not sorted");

const first = packProjection();
const second = packProjection();
if (!isDeepStrictEqual(first, second)) fail("two dry-run package inventories are not deterministic");

const paths = first.files.map(({ path }) => path).sort();
if (!isDeepStrictEqual(paths, sortedAllowlist)) {
  const actual = new Set(paths);
  const expected = new Set(sortedAllowlist);
  const missing = sortedAllowlist.filter((path) => !actual.has(path));
  const extra = paths.filter((path) => !expected.has(path));
  fail(`package inventory differs; missing=${JSON.stringify(missing)} extra=${JSON.stringify(extra)}`);
}

const rootReal = realpathSync(packageRoot);
const forbiddenLicense = /Business Source License 1\.1|SPDX-License-Identifier:\s*(?:BUSL|BSL)/iu;
for (const item of first.files) {
  const path = item.path;
  if (
    isAbsolute(path)
    || path.includes("\\")
    || path.split("/").some((part) => part === "" || part === "." || part === "..")
  ) {
    fail(`unsafe package path: ${path}`);
  }
  if (/(?:^|\/)(?:crates|server|xenoteer-server)(?:\/|$)|\.rs$/iu.test(path)) {
    fail(`server implementation path entered package: ${path}`);
  }
  const local = resolve(packageRoot, path);
  const relativePath = relative(rootReal, local);
  if (relativePath.startsWith(`..${sep}`) || relativePath === "..") {
    fail(`package path escaped package root: ${path}`);
  }
  let ancestor = packageRoot;
  for (const component of path.split("/").slice(0, -1)) {
    ancestor = join(ancestor, component);
    if (lstatSync(ancestor).isSymbolicLink()) {
      fail(`package entry has a symlink ancestor: ${path}`);
    }
  }
  const localReal = realpathSync(local);
  const realRelative = relative(rootReal, localReal);
  if (realRelative.startsWith(`..${sep}`) || realRelative === "..") {
    fail(`package entry resolves outside package root: ${path}`);
  }
  const metadata = lstatSync(local);
  if (!metadata.isFile() || metadata.isSymbolicLink()) {
    fail(`package entry is not a regular non-symlink file: ${path}`);
  }
  const content = readFileSync(local, "utf8");
  if (forbiddenLicense.test(content)) {
    fail(`BSL-licensed implementation content entered Apache package: ${path}`);
  }
}

const manifest = JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8"));
if (manifest.license !== "Apache-2.0") fail("package license is not Apache-2.0");
if (manifest.dependencies !== undefined && Object.keys(manifest.dependencies).length !== 0) {
  fail("unexpected runtime dependency entered the zero-runtime-dependency SDK");
}
if (
  first.filename !== "xenoteer-sdk-0.1.0.tgz"
  || first.entryCount !== sortedAllowlist.length
) {
  fail("package identity or entry count differs from the checked-in boundary");
}

process.stdout.write(
  `verified deterministic Apache package: ${first.entryCount} files, ${first.size} packed bytes\n`,
);
