#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

inventory=${1:?inventory path required}
output=${2:?output path required}
manifest_hash=$(sha256sum "$inventory" | awk '{print $1}')
created=$(date --utc --date="@${SOURCE_DATE_EPOCH:-0}" '+%Y-%m-%dT%H:%M:%SZ')

jq -Rn \
  --arg created "$created" \
  --arg namespace "https://github.com/th317erd/xenoteer/sbom/source/$manifest_hash" '
  [inputs
    | split("\t")
    | select(.[0] != "path")
    | {
        SPDXID: ("SPDXRef-File-" + .[4]),
        fileName: .[0],
        checksums: [{algorithm: "SHA256", checksumValue: .[1]}],
        licenseConcluded: .[2],
        licenseInfoInFiles: [.[2]],
        copyrightText: "NOASSERTION",
        fileComment: ("License evidence: " + .[3])
      }
  ] as $files
  | {
      spdxVersion: "SPDX-2.3",
      dataLicense: "CC0-1.0",
      SPDXID: "SPDXRef-DOCUMENT",
      name: "xenoteer-source",
      documentNamespace: $namespace,
      creationInfo: {created: $created, creators: ["Tool: xenoteer-generate-source-sbom"]},
      files: $files,
      documentDescribes: [$files[].SPDXID]
    }
' <"$inventory" >"$output"

printf 'wrote deterministic SPDX source SBOM: %s\n' "$output"
