#!/bin/bash
# SPDX-License-Identifier: BUSL-1.1
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
base_image=${XENOTEER_IMAGE:-xenoteer:dev}
fixture_image=${XENOTEER_DESKTOP_APPS_IMAGE:-xenoteer:desktop-apps-test}
artifact_lock=$repo_root/container/fixtures/desktop-apps/artifacts.lock
. "$repo_root/scripts/container/local-image-build-reference.sh"

validate_fixture_build_options() {
  local argument index option value numeric_value memory_amount memory_suffix
  local memory_multiplier
  local -a build_options=("$@")
  local -A seen_options=()

  for ((index = 0; index < ${#build_options[@]}; index++)); do
    argument=${build_options[index]}
    case "$argument" in
      --provenance|--provenance=*|--sbom|--sbom=*|--attest|--attest=*)
        printf '%s\n' \
          'invalid fixture build option: attestation controls are not permitted; the wrapper owns --provenance=false and --sbom=false' \
          >&2
        return 2
        ;;
      --platform|--builder|--cpu-period|--cpu-quota|--memory)
        option=${argument#--}
        ((index += 1))
        if ((index >= ${#build_options[@]})); then
          if [[ $option == platform ]]; then
            printf '%s\n' \
              'invalid fixture build option: --platform requires one single-platform OCI value' \
              >&2
          else
            printf '%s\n' \
              "invalid fixture build option: --$option requires one value" \
              >&2
          fi
          return 2
        fi
        value=${build_options[index]}
        if [[ -z $value || $value == -* ]]; then
          if [[ $option == platform ]]; then
            printf '%s\n' \
              'invalid fixture build option: --platform requires one single-platform OCI value' \
              >&2
          else
            printf '%s\n' \
              "invalid fixture build option: --$option requires one non-option value" \
              >&2
          fi
          return 2
        fi
        ;;
      --platform=*|--builder=*|--cpu-period=*|--cpu-quota=*|--memory=*)
        option=${argument%%=*}
        option=${option#--}
        value=${argument#*=}
        ;;
      --no-cache)
        option=no-cache
        value=true
        ;;
      --no-cache=*)
        printf '%s\n' \
          'invalid fixture build option: --no-cache does not take a value' \
          >&2
        return 2
        ;;
      *)
        printf '%s\n' \
          'invalid fixture build option is not permitted; the wrapper owns context, tag, IID file, Dockerfile, output/export, and attestation policy' \
          >&2
        return 2
        ;;
    esac

    if [[ -n ${seen_options[$option]+present} ]]; then
      if [[ $option == platform ]]; then
        printf '%s\n' \
          'invalid fixture build option: at most one single-platform OCI value is permitted' \
          >&2
      else
        printf '%s\n' \
          "invalid fixture build option: --$option may be supplied at most once" \
          >&2
      fi
      return 2
    fi
    seen_options[$option]=1

    case "$option" in
      platform)
        if [[ $value != local ]] \
            && [[ ! $value =~ ^[a-z0-9][a-z0-9._-]{0,31}/[a-z0-9][a-z0-9._-]{0,31}(/[a-z0-9][a-z0-9._-]{0,31})?$ ]]; then
          printf '%s\n' \
            'invalid fixture build option: --platform requires one single-platform OCI value' \
            >&2
          return 2
        fi
        ;;
      builder)
        if ((${#value} > 128)) \
            || [[ ! $value =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$ ]]; then
          printf '%s\n' \
            'invalid fixture build option: --builder requires one bounded Docker builder name' \
            >&2
          return 2
        fi
        ;;
      cpu-period)
        if [[ ! $value =~ ^[0-9]{4,7}$ ]]; then
          printf '%s\n' \
            'invalid fixture build option: --cpu-period requires an integer from 1000 through 1000000' \
            >&2
          return 2
        fi
        numeric_value=$((10#$value))
        if ((numeric_value < 1000 || numeric_value > 1000000)); then
          printf '%s\n' \
            'invalid fixture build option: --cpu-period requires an integer from 1000 through 1000000' \
            >&2
          return 2
        fi
        ;;
      cpu-quota)
        if [[ ! $value =~ ^[1-9][0-9]{0,9}$ ]]; then
          printf '%s\n' \
            'invalid fixture build option: --cpu-quota requires a positive integer no greater than 1000000000' \
            >&2
          return 2
        fi
        numeric_value=$((10#$value))
        if ((numeric_value > 1000000000)); then
          printf '%s\n' \
            'invalid fixture build option: --cpu-quota requires a positive integer no greater than 1000000000' \
            >&2
          return 2
        fi
        ;;
      memory)
        if [[ ! $value =~ ^([1-9][0-9]{0,12})([bBkKmMgG]|[kKmMgG][bB])?$ ]]; then
          printf '%s\n' \
            'invalid fixture build option: --memory requires positive bytes or a b/k/m/g unit no greater than 1 TiB' \
            >&2
          return 2
        fi
        memory_amount=$((10#${BASH_REMATCH[1]}))
        memory_suffix=${BASH_REMATCH[2],,}
        case "$memory_suffix" in
          ''|b) memory_multiplier=1 ;;
          k|kb) memory_multiplier=1024 ;;
          m|mb) memory_multiplier=$((1024 * 1024)) ;;
          g|gb) memory_multiplier=$((1024 * 1024 * 1024)) ;;
          *) return 2 ;;
        esac
        if ((memory_amount > 1099511627776 / memory_multiplier)); then
          printf '%s\n' \
            'invalid fixture build option: --memory requires positive bytes or a b/k/m/g unit no greater than 1 TiB' \
            >&2
          return 2
        fi
        ;;
      no-cache)
        ;;
    esac
  done
}

validate_fixture_build_options "$@"

cleanup() {
  local original_status=$? alias_cleanup_status
  trap - EXIT HUP INT TERM
  set +e
  xenoteer_stop_guarded_local_image_command
  xenoteer_cleanup_local_image_alias
  alias_cleanup_status=$?
  if [[ $original_status -ne 0 ]]; then
    exit "$original_status"
  fi
  exit "$alias_cleanup_status"
}
signal_exit() {
  local signal_status=$1
  trap - HUP INT TERM
  set +e
  xenoteer_stop_guarded_local_image_command
  exit "$signal_status"
}
trap cleanup EXIT
trap 'signal_exit 129' HUP
trap 'signal_exit 130' INT
trap 'signal_exit 143' TERM

xenoteer_create_local_image_alias "$base_image" desktop-fixture
base_image_id=$XENOTEER_LOCAL_IMAGE_ID
base_build_reference=$XENOTEER_LOCAL_IMAGE_ALIAS
xenoteer_verify_local_image_alias

lock_value() {
  local key=$1 value
  value=$(sed -n "s/^${key}=//p" "$artifact_lock")
  if [[ -z $value ]] || [[ $(grep -c "^${key}=" "$artifact_lock") -ne 1 ]]; then
    printf 'artifact lock must contain exactly one non-empty %s entry\n' "$key" >&2
    exit 1
  fi
  printf '%s' "$value"
}

electron_version=$(lock_value ELECTRON_VERSION)
electron_url=$(lock_value ELECTRON_LINUX_X64_URL)
electron_sha256=$(lock_value ELECTRON_LINUX_X64_SHA256)

printf 'building desktop fixture from immutable base %s (resolved from %s)\n' \
  "$base_image_id" "$base_image"
xenoteer_prepare_local_image_iidfile
xenoteer_run_guarded_local_image_command docker build \
  "$@" \
  --provenance=false \
  --sbom=false \
  --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
  --file "$repo_root/container/fixtures/desktop-apps/Dockerfile" \
  --build-arg "XENOTEER_BASE_IMAGE=$base_build_reference" \
  --build-arg "XENOTEER_FIXTURE_BASE_IMAGE_ID=$base_image_id" \
  --build-arg "ELECTRON_VERSION=$electron_version" \
  --build-arg "ELECTRON_LINUX_X64_URL=$electron_url" \
  --build-arg "ELECTRON_LINUX_X64_SHA256=$electron_sha256" \
  --label com.aeor.xenoteer.distribution-scope=test-only-non-distributable \
  --tag "$fixture_image" \
  "$repo_root"

xenoteer_verify_local_image_alias

xenoteer_verify_local_image_derivation "$fixture_image"
fixture_image_id=$XENOTEER_LOCAL_DERIVED_IMAGE_ID

recorded_base_id=$(docker image inspect "$fixture_image_id" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.base-image-id"}}')
recorded_electron_sha=$(docker image inspect "$fixture_image_id" \
  --format '{{index .Config.Labels "com.aeor.xenoteer.fixture.electron-linux-x64-sha256"}}')
test "$recorded_base_id" = "$base_image_id"
test "$recorded_electron_sha" = "$electron_sha256"

printf 'desktop fixture image %s records exact base %s\n' \
  "$fixture_image_id" "$base_image_id"
