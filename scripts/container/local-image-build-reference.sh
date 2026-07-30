#!/bin/sh
# SPDX-License-Identifier: BUSL-1.1
#
# Source this file from a Docker build wrapper. It converts one locally
# inspected image into a temporary tag because Dockerfile FROM does not treat a
# local sha256:<image-id> as a local build reference.

XENOTEER_LOCAL_IMAGE_ID=
XENOTEER_LOCAL_IMAGE_ALIAS=
XENOTEER_LOCAL_IMAGE_RESERVATION=
XENOTEER_LOCAL_IMAGE_IIDFILE=
XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=
XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=
XENOTEER_LOCAL_IMAGE_RESERVATION_UID=
XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=
XENOTEER_LOCAL_DERIVED_IMAGE_ID=
XENOTEER_LOCAL_IMAGE_ALIAS_OWNED=0
XENOTEER_LOCAL_IMAGE_CHILD_PID=

xenoteer_validate_local_image_reference() {
    xenoteer_candidate_reference=$1
    case "$xenoteer_candidate_reference" in
        ''|-*) return 1 ;;
        *'
'*) return 1 ;;
    esac
    if printf '%s' "$xenoteer_candidate_reference" \
            | LC_ALL=C /usr/bin/grep -q '[[:cntrl:]]'; then
        return 1
    fi
}

xenoteer_validate_local_image_id() {
    xenoteer_candidate_id=$1
    case "$xenoteer_candidate_id" in
        sha256:*) xenoteer_candidate_digest=${xenoteer_candidate_id#sha256:} ;;
        *) return 1 ;;
    esac
    [ "${#xenoteer_candidate_digest}" -eq 64 ] || return 1
    case "$xenoteer_candidate_digest" in
        *[!0-9a-f]*) return 1 ;;
    esac
}

xenoteer_resolve_durable_local_image_reference() {
    xenoteer_durable_reference=$1
    xenoteer_excluded_alias=${2:-}
    if ! xenoteer_source_metadata=$(
        docker image inspect "$xenoteer_durable_reference" \
            --format '{"Id":{{json .Id}},"RepoTags":{{json .RepoTags}},"RepoDigests":{{json .RepoDigests}}}'
    ); then
        printf 'could not inspect durable local source image metadata: %s\n' \
            "$xenoteer_durable_reference" >&2
        return 1
    fi
    if ! xenoteer_durable_image_id=$(
        printf '%s\n' "$xenoteer_source_metadata" | python3 -c '
import json
import sys

excluded_alias = sys.argv[1]
try:
    metadata = json.load(sys.stdin)
except (TypeError, ValueError) as error:
    raise SystemExit("Docker returned malformed source image metadata") from error
if not isinstance(metadata, dict):
    raise SystemExit("Docker returned malformed source image metadata")
image = metadata
image_id = image.get("Id")
repo_tags = image.get("RepoTags")
repo_digests = image.get("RepoDigests")
if repo_tags is None:
    repo_tags = []
if repo_digests is None:
    repo_digests = []
if (
    not isinstance(image_id, str)
    or not isinstance(repo_tags, list)
    or not isinstance(repo_digests, list)
    or any(not isinstance(reference, str) for reference in repo_tags)
    or any(not isinstance(reference, str) for reference in repo_digests)
):
    raise SystemExit("Docker returned malformed source image metadata")
durable_references = [
    reference
    for reference in repo_tags
    if reference not in ("", "<none>:<none>", excluded_alias)
]
durable_references.extend(
    reference
    for reference in repo_digests
    if reference not in ("", "<none>@<none>", excluded_alias)
)
if not durable_references:
    raise SystemExit(
        "local source image has no durable pre-existing RepoTag or RepoDigest"
    )
print(image_id)
' "$xenoteer_excluded_alias"
    ); then
        printf 'could not prove a durable local source image reference: %s\n' \
            "$xenoteer_durable_reference" >&2
        return 1
    fi
    if ! xenoteer_validate_local_image_id "$xenoteer_durable_image_id"; then
        printf 'source image metadata did not contain an exact lowercase image ID: %s\n' \
            "$xenoteer_durable_reference" >&2
        return 1
    fi
    printf '%s\n' "$xenoteer_durable_image_id"
}

xenoteer_run_guarded_local_image_command() {
    [ -z "$XENOTEER_LOCAL_IMAGE_CHILD_PID" ] || {
        printf 'a guarded local image command is already active\n' >&2
        return 1
    }
    "$@" &
    XENOTEER_LOCAL_IMAGE_CHILD_PID=$!
    xenoteer_guarded_status=0
    wait "$XENOTEER_LOCAL_IMAGE_CHILD_PID" || xenoteer_guarded_status=$?
    XENOTEER_LOCAL_IMAGE_CHILD_PID=
    return "$xenoteer_guarded_status"
}

xenoteer_stop_guarded_local_image_command() {
    xenoteer_guarded_pid=$XENOTEER_LOCAL_IMAGE_CHILD_PID
    [ -n "$xenoteer_guarded_pid" ] || return 0

    kill -TERM "$xenoteer_guarded_pid" 2>/dev/null || true
    xenoteer_guarded_attempt=0
    while kill -0 "$xenoteer_guarded_pid" 2>/dev/null; do
        xenoteer_guarded_attempt=$((xenoteer_guarded_attempt + 1))
        if [ "$xenoteer_guarded_attempt" -ge 5 ]; then
            kill -KILL "$xenoteer_guarded_pid" 2>/dev/null || true
            break
        fi
        /bin/sleep 0.05
    done
    wait "$xenoteer_guarded_pid" 2>/dev/null || true
    XENOTEER_LOCAL_IMAGE_CHILD_PID=
}

xenoteer_validate_local_image_reservation() {
    xenoteer_require_absent_iid=${1:-0}
    if [ -z "$XENOTEER_LOCAL_IMAGE_RESERVATION" ] \
            || [ -z "$XENOTEER_LOCAL_IMAGE_IIDFILE" ] \
            || [ "$XENOTEER_LOCAL_IMAGE_IIDFILE" != \
                "$XENOTEER_LOCAL_IMAGE_RESERVATION/derived-image-id" ]; then
        printf 'local image build reservation is not owned by this process\n' >&2
        return 1
    fi
    python3 -c '
import os
import stat
import sys

path, expected_device, expected_inode, expected_uid, expected_nlink, absent = (
    sys.argv[1:]
)
expected_identity = (
    int(expected_device),
    int(expected_inode),
    int(expected_uid),
    int(expected_nlink),
)
flags = os.O_RDONLY
flags |= getattr(os, "O_CLOEXEC", 0)
flags |= getattr(os, "O_DIRECTORY", 0)
flags |= getattr(os, "O_NOFOLLOW", 0)
try:
    directory = os.open(path, flags)
except (OSError, ValueError) as error:
    raise SystemExit("could not securely open local image reservation") from error
try:
    directory_stat = os.fstat(directory)
    observed_identity = (
        directory_stat.st_dev,
        directory_stat.st_ino,
        directory_stat.st_uid,
        directory_stat.st_nlink,
    )
    if (
        not stat.S_ISDIR(directory_stat.st_mode)
        or observed_identity != expected_identity
        or directory_stat.st_uid != os.geteuid()
        or stat.S_IMODE(directory_stat.st_mode) != 0o700
    ):
        raise SystemExit("local image reservation identity or metadata changed")
    if absent == "1":
        try:
            os.stat("derived-image-id", dir_fd=directory, follow_symlinks=False)
        except FileNotFoundError:
            pass
        else:
            raise SystemExit("Docker build IID path already exists")
finally:
    os.close(directory)
' "$XENOTEER_LOCAL_IMAGE_RESERVATION" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_INODE" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_UID" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK" \
        "$xenoteer_require_absent_iid"
}

xenoteer_prepare_local_image_iidfile() {
    if ! xenoteer_validate_local_image_reservation 1; then
        printf 'could not prove an absent Docker build IID path\n' >&2
        return 1
    fi
}

xenoteer_remove_local_image_iidfile() {
    python3 -c '
import os
import stat
import sys

path, expected_device, expected_inode, expected_uid, expected_nlink = sys.argv[1:]
expected_identity = (
    int(expected_device),
    int(expected_inode),
    int(expected_uid),
    int(expected_nlink),
)
flags = os.O_RDONLY
flags |= getattr(os, "O_CLOEXEC", 0)
flags |= getattr(os, "O_DIRECTORY", 0)
flags |= getattr(os, "O_NOFOLLOW", 0)
try:
    directory = os.open(path, flags)
except (OSError, ValueError) as error:
    raise SystemExit("could not securely open local image reservation") from error
try:
    directory_stat = os.fstat(directory)
    observed_identity = (
        directory_stat.st_dev,
        directory_stat.st_ino,
        directory_stat.st_uid,
        directory_stat.st_nlink,
    )
    if (
        not stat.S_ISDIR(directory_stat.st_mode)
        or observed_identity != expected_identity
        or directory_stat.st_uid != os.geteuid()
        or stat.S_IMODE(directory_stat.st_mode) != 0o700
    ):
        raise SystemExit("local image reservation identity or metadata changed")
    try:
        os.unlink("derived-image-id", dir_fd=directory)
    except FileNotFoundError:
        pass
finally:
    os.close(directory)
' "$XENOTEER_LOCAL_IMAGE_RESERVATION" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_INODE" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_UID" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK"
}

xenoteer_verify_local_image_alias() {
    [ "$XENOTEER_LOCAL_IMAGE_ALIAS_OWNED" -eq 1 ] || {
        printf 'local image build alias is not owned by this process\n' >&2
        return 1
    }
    if ! xenoteer_observed_alias_id=$(
        docker image inspect "$XENOTEER_LOCAL_IMAGE_ALIAS" \
            --format '{{.Id}}'
    ); then
        printf 'local image build alias disappeared: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
        return 1
    fi
    if [ "$xenoteer_observed_alias_id" != "$XENOTEER_LOCAL_IMAGE_ID" ]; then
        printf 'local image build alias changed identity: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
        return 1
    fi
    if ! xenoteer_observed_source_id=$(
        docker image inspect "$XENOTEER_LOCAL_IMAGE_ID" \
            --format '{{.Id}}'
    ); then
        printf 'exact local source image disappeared: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ID" >&2
        return 1
    fi
    if [ "$xenoteer_observed_source_id" != "$XENOTEER_LOCAL_IMAGE_ID" ]; then
        printf 'exact local source image changed identity: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ID" >&2
        return 1
    fi
}

xenoteer_create_local_image_alias() {
    xenoteer_source_reference=$1
    xenoteer_alias_purpose=$2
    if ! xenoteer_validate_local_image_reference \
            "$xenoteer_source_reference"; then
        printf 'invalid local source image reference\n' >&2
        return 1
    fi
    case "$xenoteer_alias_purpose" in
        ''|*[!a-z0-9-]*)
            printf 'invalid local image alias purpose: %s\n' \
                "$xenoteer_alias_purpose" >&2
            return 1
            ;;
    esac
    [ "$XENOTEER_LOCAL_IMAGE_ALIAS_OWNED" -eq 0 ] || {
        printf 'a local image build alias is already active\n' >&2
        return 1
    }
    # This exported contract is consumed by the sourcing wrapper.
    # shellcheck disable=SC2034
    XENOTEER_LOCAL_DERIVED_IMAGE_ID=
    if ! XENOTEER_LOCAL_IMAGE_ID=$(
        xenoteer_resolve_durable_local_image_reference \
            "$xenoteer_source_reference"
    ); then
        printf 'could not admit local base image: %s\n' \
            "$xenoteer_source_reference" >&2
        return 1
    fi
    if ! xenoteer_validate_local_image_id "$XENOTEER_LOCAL_IMAGE_ID"; then
        printf 'base image did not resolve to an exact lowercase image ID: %s\n' \
            "$xenoteer_source_reference" >&2
        return 1
    fi

    xenoteer_reservation_attempt=0
    XENOTEER_LOCAL_IMAGE_RESERVATION=
    while [ "$xenoteer_reservation_attempt" -lt 8 ]; do
        xenoteer_reservation_attempt=$((xenoteer_reservation_attempt + 1))
        if ! xenoteer_alias_nonce=$(
            /usr/bin/od -An -N16 -tx1 /dev/urandom \
                | /usr/bin/tr -d ' \n'
        ); then
            printf 'could not generate a local image alias nonce\n' >&2
            return 1
        fi
        case "$xenoteer_alias_nonce" in
            *[!0-9a-f]*|'') continue ;;
        esac
        [ "${#xenoteer_alias_nonce}" -eq 32 ] || continue
        xenoteer_reservation_path="/tmp/xenoteer-local-image-$xenoteer_alias_nonce"
        if (umask 077 && /usr/bin/mkdir "$xenoteer_reservation_path") \
                2>/dev/null; then
            XENOTEER_LOCAL_IMAGE_RESERVATION=$xenoteer_reservation_path
            break
        fi
    done
    [ -n "$XENOTEER_LOCAL_IMAGE_RESERVATION" ] || {
        printf 'could not reserve a unique local image build alias\n' >&2
        return 1
    }
    XENOTEER_LOCAL_IMAGE_IIDFILE=\
"$XENOTEER_LOCAL_IMAGE_RESERVATION/derived-image-id"
    if ! xenoteer_reservation_identity=$(
        python3 -c '
import os
import stat
import sys

path = sys.argv[1]
try:
    directory_stat = os.lstat(path)
except OSError as error:
    raise SystemExit("could not inspect local image reservation") from error
if (
    not stat.S_ISDIR(directory_stat.st_mode)
    or directory_stat.st_uid != os.geteuid()
    or stat.S_IMODE(directory_stat.st_mode) != 0o700
    or directory_stat.st_nlink < 1
):
    raise SystemExit("local image reservation is not private and owned")
print(
    f"{directory_stat.st_dev}:{directory_stat.st_ino}:"
    f"{directory_stat.st_uid}:{directory_stat.st_nlink}"
)
' "$XENOTEER_LOCAL_IMAGE_RESERVATION"
    ); then
        printf 'could not prove ownership of the local image reservation\n' >&2
        /usr/bin/rmdir "$XENOTEER_LOCAL_IMAGE_RESERVATION"
        XENOTEER_LOCAL_IMAGE_RESERVATION=
        XENOTEER_LOCAL_IMAGE_IIDFILE=
        return 1
    fi
    XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=\
${xenoteer_reservation_identity%%:*}
    xenoteer_reservation_identity_rest=${xenoteer_reservation_identity#*:}
    XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=\
${xenoteer_reservation_identity_rest%%:*}
    xenoteer_reservation_identity_rest=${xenoteer_reservation_identity_rest#*:}
    XENOTEER_LOCAL_IMAGE_RESERVATION_UID=\
${xenoteer_reservation_identity_rest%%:*}
    XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=\
${xenoteer_reservation_identity_rest#*:}
    for xenoteer_reservation_number in \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_INODE" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_UID" \
        "$XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK"; do
        case "$xenoteer_reservation_number" in
            ''|*[!0-9]*)
                printf 'local image reservation identity was malformed\n' >&2
                /usr/bin/rmdir "$XENOTEER_LOCAL_IMAGE_RESERVATION"
                XENOTEER_LOCAL_IMAGE_RESERVATION=
                XENOTEER_LOCAL_IMAGE_IIDFILE=
                XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=
                XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=
                XENOTEER_LOCAL_IMAGE_RESERVATION_UID=
                XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=
                return 1
                ;;
        esac
    done
    XENOTEER_LOCAL_IMAGE_ALIAS=\
"xenoteer-local-build/$xenoteer_alias_purpose:$xenoteer_alias_nonce"
    case "$XENOTEER_LOCAL_IMAGE_ALIAS" in
        *[!a-z0-9/:.-]*)
            printf 'generated an unsafe local image build alias\n' >&2
            /usr/bin/rmdir "$XENOTEER_LOCAL_IMAGE_RESERVATION"
            XENOTEER_LOCAL_IMAGE_RESERVATION=
            XENOTEER_LOCAL_IMAGE_IIDFILE=
            XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=
            XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=
            XENOTEER_LOCAL_IMAGE_RESERVATION_UID=
            XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=
            XENOTEER_LOCAL_IMAGE_ALIAS=
            return 1
            ;;
    esac
    if ! xenoteer_existing_alias_ids=$(
        docker image ls --quiet --no-trunc \
            --filter "reference=$XENOTEER_LOCAL_IMAGE_ALIAS"
    ); then
        printf 'could not prove local image alias absence: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
        /usr/bin/rmdir "$XENOTEER_LOCAL_IMAGE_RESERVATION"
        XENOTEER_LOCAL_IMAGE_RESERVATION=
        XENOTEER_LOCAL_IMAGE_IIDFILE=
        XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=
        XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=
        XENOTEER_LOCAL_IMAGE_RESERVATION_UID=
        XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=
        XENOTEER_LOCAL_IMAGE_ALIAS=
        return 1
    fi
    if [ -n "$xenoteer_existing_alias_ids" ]; then
        printf 'refusing to replace a pre-existing local image alias: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
        /usr/bin/rmdir "$XENOTEER_LOCAL_IMAGE_RESERVATION"
        XENOTEER_LOCAL_IMAGE_RESERVATION=
        XENOTEER_LOCAL_IMAGE_IIDFILE=
        XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=
        XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=
        XENOTEER_LOCAL_IMAGE_RESERVATION_UID=
        XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=
        XENOTEER_LOCAL_IMAGE_ALIAS=
        return 1
    fi
    XENOTEER_LOCAL_IMAGE_ALIAS_OWNED=1
    if ! xenoteer_run_guarded_local_image_command docker image tag \
            "$XENOTEER_LOCAL_IMAGE_ID" "$XENOTEER_LOCAL_IMAGE_ALIAS"; then
        printf 'could not create local image build alias: %s\n' \
            "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
        return 1
    fi
    xenoteer_verify_local_image_alias
}

xenoteer_verify_local_image_derivation() {
    XENOTEER_LOCAL_DERIVED_IMAGE_ID=
    if [ -z "$XENOTEER_LOCAL_IMAGE_IIDFILE" ] \
            || [ -z "$XENOTEER_LOCAL_IMAGE_RESERVATION" ] \
            || [ "$XENOTEER_LOCAL_IMAGE_IIDFILE" != \
                "$XENOTEER_LOCAL_IMAGE_RESERVATION/derived-image-id" ]; then
        printf 'local image build IID file is not reserved by this process\n' >&2
        return 1
    fi
    if ! xenoteer_derived_image_id=$(
        python3 -c '
import os
import re
import stat
import sys

path, expected_device, expected_inode, expected_uid, expected_nlink = sys.argv[1:]
expected_identity = (
    int(expected_device),
    int(expected_inode),
    int(expected_uid),
    int(expected_nlink),
)
directory_flags = os.O_RDONLY
directory_flags |= getattr(os, "O_CLOEXEC", 0)
directory_flags |= getattr(os, "O_DIRECTORY", 0)
directory_flags |= getattr(os, "O_NOFOLLOW", 0)
try:
    directory = os.open(path, directory_flags)
except (OSError, ValueError) as error:
    raise SystemExit("could not securely open local image reservation") from error
try:
    directory_stat = os.fstat(directory)
    observed_directory_identity = (
        directory_stat.st_dev,
        directory_stat.st_ino,
        directory_stat.st_uid,
        directory_stat.st_nlink,
    )
    if (
        not stat.S_ISDIR(directory_stat.st_mode)
        or observed_directory_identity != expected_identity
        or directory_stat.st_uid != os.geteuid()
        or stat.S_IMODE(directory_stat.st_mode) != 0o700
    ):
        raise SystemExit("local image reservation identity or metadata changed")
    file_flags = os.O_RDONLY | os.O_NONBLOCK
    file_flags |= getattr(os, "O_CLOEXEC", 0)
    file_flags |= getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(
            "derived-image-id",
            file_flags,
            dir_fd=directory,
        )
    except (OSError, ValueError) as error:
        raise SystemExit("could not securely open Docker build IID file") from error
    try:
        file_stat = os.fstat(descriptor)
        original_file_identity = (
            file_stat.st_dev,
            file_stat.st_ino,
            file_stat.st_uid,
        )
        file_mode = stat.S_IMODE(file_stat.st_mode)
        if (
            not stat.S_ISREG(file_stat.st_mode)
            or file_stat.st_uid != os.geteuid()
            or file_stat.st_nlink != 1
            or file_mode | 0o644 != 0o644
            or file_mode & 0o400 == 0
            or file_stat.st_size not in (71, 72)
        ):
            raise SystemExit("Docker build IID file identity or metadata is unsafe")
        if file_mode != 0o600:
            os.fchmod(descriptor, 0o600)
            private_stat = os.fstat(descriptor)
            if (
                (
                    private_stat.st_dev,
                    private_stat.st_ino,
                    private_stat.st_uid,
                )
                != original_file_identity
                or not stat.S_ISREG(private_stat.st_mode)
                or stat.S_IMODE(private_stat.st_mode) != 0o600
                or private_stat.st_nlink != 1
            ):
                raise SystemExit("could not reduce Docker build IID permissions")
        raw_id = os.read(descriptor, 73)
        if len(raw_id) > 72 or os.read(descriptor, 1) != b"":
            raise SystemExit("Docker build IID file exceeded its exact size bound")
    finally:
        os.close(descriptor)
finally:
    os.close(directory)
if raw_id.endswith(b"\n"):
    raw_id = raw_id[:-1]
try:
    image_id = raw_id.decode("ascii")
except UnicodeDecodeError as error:
    raise SystemExit("Docker build IID is not ASCII") from error
if re.fullmatch(r"sha256:[0-9a-f]{64}", image_id) is None:
    raise SystemExit("Docker build IID is not an exact lowercase image ID")
print(image_id)
' "$XENOTEER_LOCAL_IMAGE_RESERVATION" \
            "$XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE" \
            "$XENOTEER_LOCAL_IMAGE_RESERVATION_INODE" \
            "$XENOTEER_LOCAL_IMAGE_RESERVATION_UID" \
            "$XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK"
    ); then
        printf 'could not read an exact derived image ID from Docker build IID file\n' \
            >&2
        return 1
    fi
    if ! xenoteer_validate_local_image_id "$xenoteer_derived_image_id"; then
        printf 'derived image did not resolve to an exact lowercase image ID\n' >&2
        return 1
    fi
    if [ "$xenoteer_derived_image_id" = "$XENOTEER_LOCAL_IMAGE_ID" ]; then
        printf 'derived image unexpectedly resolves to the exact base image\n' >&2
        return 1
    fi
    if ! xenoteer_derivation_metadata=$(
        docker image inspect \
            "$XENOTEER_LOCAL_IMAGE_ID" "$xenoteer_derived_image_id"
    ); then
        printf 'could not inspect exact local image derivation\n' >&2
        return 1
    fi
    if ! printf '%s\n' "$xenoteer_derivation_metadata" | python3 -c '
import json
import sys

base_id, derived_id = sys.argv[1:]
try:
    base, derived = json.load(sys.stdin)
    base_layers = base["RootFS"]["Layers"]
    derived_layers = derived["RootFS"]["Layers"]
except (KeyError, TypeError, ValueError) as error:
    raise SystemExit("Docker returned malformed derivation metadata") from error
if base["Id"] != base_id or derived["Id"] != derived_id:
    raise SystemExit("local image identity changed during derivation proof")
if derived_layers[: len(base_layers)] != base_layers:
    raise SystemExit("derived image does not retain the exact base layer prefix")
' "$XENOTEER_LOCAL_IMAGE_ID" "$xenoteer_derived_image_id"
    then
        return 1
    fi
    XENOTEER_LOCAL_DERIVED_IMAGE_ID=$xenoteer_derived_image_id
}

xenoteer_cleanup_local_image_alias() {
    xenoteer_alias_cleanup_status=0
    xenoteer_alias_removed=0
    xenoteer_stop_guarded_local_image_command
    if [ "$XENOTEER_LOCAL_IMAGE_ALIAS_OWNED" -eq 1 ]; then
        if ! xenoteer_cleanup_alias_id=$(
            docker image inspect "$XENOTEER_LOCAL_IMAGE_ALIAS" \
                --format '{{.Id}}'
        ); then
            printf 'owned local image alias disappeared before cleanup: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
            xenoteer_alias_cleanup_status=1
        elif [ "$xenoteer_cleanup_alias_id" != "$XENOTEER_LOCAL_IMAGE_ID" ]; then
            printf 'refusing to remove a foreign-retagged local image alias: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
            xenoteer_alias_cleanup_status=1
        elif ! xenoteer_cleanup_durable_id=$(
            xenoteer_resolve_durable_local_image_reference \
                "$XENOTEER_LOCAL_IMAGE_ID" \
                "$XENOTEER_LOCAL_IMAGE_ALIAS"
        ); then
            printf 'refusing to remove the owned alias without a durable source reference: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
            xenoteer_alias_cleanup_status=1
        elif [ "$xenoteer_cleanup_durable_id" != \
                "$XENOTEER_LOCAL_IMAGE_ID" ]; then
            printf 'durable local source identity changed before alias cleanup\n' \
                >&2
            xenoteer_alias_cleanup_status=1
        elif ! docker image rm "$XENOTEER_LOCAL_IMAGE_ALIAS" >/dev/null; then
            printf 'could not remove owned local image alias: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_ALIAS" >&2
            xenoteer_alias_cleanup_status=1
        else
            xenoteer_alias_removed=1
        fi
        if [ "$xenoteer_alias_removed" -eq 1 ]; then
            if ! xenoteer_cleanup_source_id=$(
                docker image inspect "$XENOTEER_LOCAL_IMAGE_ID" \
                    --format '{{.Id}}'
            ); then
                printf 'exact local source image did not survive alias cleanup: %s\n' \
                    "$XENOTEER_LOCAL_IMAGE_ID" >&2
                xenoteer_alias_cleanup_status=1
            elif [ "$xenoteer_cleanup_source_id" != \
                    "$XENOTEER_LOCAL_IMAGE_ID" ]; then
                printf 'exact local source identity changed during alias cleanup\n' \
                    >&2
                xenoteer_alias_cleanup_status=1
            fi
        fi
    fi
    if [ -n "$XENOTEER_LOCAL_IMAGE_IIDFILE" ]; then
        if [ -z "$XENOTEER_LOCAL_IMAGE_RESERVATION" ] \
                || [ "$XENOTEER_LOCAL_IMAGE_IIDFILE" != \
                    "$XENOTEER_LOCAL_IMAGE_RESERVATION/derived-image-id" ]; then
            printf 'refusing to remove an unowned Docker build IID file: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_IIDFILE" >&2
            xenoteer_alias_cleanup_status=1
        elif ! xenoteer_remove_local_image_iidfile; then
            printf 'could not securely remove Docker build IID file: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_IIDFILE" >&2
            xenoteer_alias_cleanup_status=1
        fi
    fi
    if [ -n "$XENOTEER_LOCAL_IMAGE_RESERVATION" ]; then
        if ! xenoteer_validate_local_image_reservation 1; then
            printf 'could not revalidate local image alias reservation for cleanup: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_RESERVATION" >&2
            xenoteer_alias_cleanup_status=1
        elif ! /usr/bin/rmdir "$XENOTEER_LOCAL_IMAGE_RESERVATION"; then
            printf 'could not remove local image alias reservation: %s\n' \
                "$XENOTEER_LOCAL_IMAGE_RESERVATION" >&2
            xenoteer_alias_cleanup_status=1
        else
            XENOTEER_LOCAL_IMAGE_RESERVATION=
            XENOTEER_LOCAL_IMAGE_IIDFILE=
            XENOTEER_LOCAL_IMAGE_RESERVATION_DEVICE=
            XENOTEER_LOCAL_IMAGE_RESERVATION_INODE=
            XENOTEER_LOCAL_IMAGE_RESERVATION_UID=
            XENOTEER_LOCAL_IMAGE_RESERVATION_NLINK=
        fi
    fi
    if [ "$XENOTEER_LOCAL_IMAGE_ALIAS_OWNED" -eq 0 ] \
            || [ "$xenoteer_alias_removed" -eq 1 ]; then
        XENOTEER_LOCAL_IMAGE_ALIAS_OWNED=0
        XENOTEER_LOCAL_IMAGE_ALIAS=
        XENOTEER_LOCAL_IMAGE_ID=
    fi
    # This exported contract is consumed by the sourcing wrapper.
    # shellcheck disable=SC2034
    XENOTEER_LOCAL_DERIVED_IMAGE_ID=
    return "$xenoteer_alias_cleanup_status"
}
