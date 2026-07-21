#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=${1:?repository root is required}
binary=${2:?linked binary is required}
manifest=${3:?component manifest output is required}
sbom=${4:?component SPDX output is required}
repo_root=$(cd "$repo_root" && pwd)
metadata=$(mktemp)
components=$(mktemp)
trap 'rm -f -- "$metadata" "$components"' EXIT

[[ -f $repo_root/Cargo.lock ]] || { printf 'Cargo.lock is missing\n' >&2; exit 1; }
[[ -x $binary ]] || { printf 'linked binary is missing or not executable: %s\n' "$binary" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { printf 'jq is required for Cargo manifest generation\n' >&2; exit 2; }

(cd "$repo_root" && cargo metadata \
  --locked \
  --filter-platform x86_64-unknown-linux-gnu \
  --format-version 1) >"$metadata"
lock_hash=$(sha256sum "$repo_root/Cargo.lock" | awk '{print $1}')
binary_hash=$(sha256sum "$binary" | awk '{print $1}')

jq -r '
  def closure($id; $edges): $id, ($edges[$id][]? | closure(.; $edges));
  . as $metadata
  | ($metadata.packages | map({key: .id, value: .}) | from_entries) as $packages
  | ($metadata.resolve.nodes
      | map({
          key: .id,
          value: [
            .deps[]
            | select(any(.dep_kinds[]; .kind != "dev"))
            | .pkg
          ]
        })
      | from_entries) as $edges
  | ([
      $metadata.packages[]
      | select(.name == "xenoteerd" and .source == null)
      | .id
    ] | if length == 1 then .[0] else error("expected exactly one workspace xenoteerd package") end) as $root
  | [closure($root; $edges)] | unique[] as $id
  | $packages[$id]
  | if (.license == null or .license == "") then error("package has no declared license: " + .id) else . end
  | [
      .name,
      .version,
      (.source // "workspace"),
      .license,
      (.license_file // "-"),
      (.repository // "-"),
      .manifest_path
    ]
  | @tsv
' "$metadata" | LC_ALL=C sort -t $'\t' -k1,1 -k2,2V >"$components"

mkdir -p "$(dirname "$manifest")" "$(dirname "$sbom")"
{
  printf 'name\tversion\tsource\tlicense_expression\tlicense_file\trepository\tmanifest_path\tcargo_lock_sha256\txenoteerd_sha256\n'
  while IFS= read -r line; do
    printf '%s\t%s\t%s\n' "$line" "$lock_hash" "$binary_hash"
  done <"$components"
} >"$manifest"

manifest_hash=$(sha256sum "$manifest" | awk '{print $1}')
jq -Rn \
  --arg namespace "https://github.com/th317erd/xenoteer/sbom/cargo/$manifest_hash" \
  --arg lock_hash "$lock_hash" \
  --arg binary_hash "$binary_hash" '
  [inputs
    | split("\t")
    | select(.[0] != "name")
    | {
        SPDXID: ("SPDXRef-Package-" + (.[0] | gsub("[^A-Za-z0-9.-]"; "-")) + "-" + (.[1] | gsub("[^A-Za-z0-9.-]"; "-"))),
        name: .[0],
        versionInfo: .[1],
        downloadLocation: (if .[2] == "workspace" then "NOASSERTION" else .[2] end),
        filesAnalyzed: false,
        licenseConcluded: .[3],
        licenseDeclared: .[3],
        copyrightText: "NOASSERTION",
        supplier: "NOASSERTION"
      }
  ] as $packages
  | {
      spdxVersion: "SPDX-2.3",
      dataLicense: "CC0-1.0",
      SPDXID: "SPDXRef-DOCUMENT",
      name: "xenoteerd-linked-cargo-components",
      documentNamespace: $namespace,
      creationInfo: {created: "1970-01-01T00:00:00Z", creators: ["Tool: xenoteer-generate-cargo-manifest"]},
      comment: ("Cargo.lock SHA256: " + $lock_hash + "; xenoteerd SHA256: " + $binary_hash),
      packages: $packages,
      documentDescribes: [$packages[].SPDXID]
    }
' <"$manifest" >"$sbom"

printf 'wrote Cargo component manifest and SPDX SBOM for %s\n' "$binary"
