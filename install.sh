#!/usr/bin/bash

# ==================================================
# @file install.sh
# @brief Public Eiyah installation bootstrap
# ==================================================

set -u
set -o pipefail

readonly CURL=/usr/bin/curl
readonly MKTEMP=/usr/bin/mktemp
readonly SHA256SUM=/usr/bin/sha256sum
readonly CHMOD=/usr/bin/chmod
readonly RM=/usr/bin/rm
readonly LATEST_RELEASE_URL=https://github.com/su-ito-lab/eiyah/releases/latest
readonly RELEASE_DOWNLOAD_ROOT=https://github.com/su-ito-lab/eiyah/releases/download
readonly BINARY_ASSET=eiyah-x86_64-unknown-linux-gnu
readonly CHECKSUM_ASSET=eiyah-x86_64-unknown-linux-gnu.sha256

attempt_directory=


# --------------------------------------------------
# Diagnostics
# --------------------------------------------------

# bootstrap errorをstderrへ表示する
error() {
    printf 'Error: %s\n' "$1" >&2
}

# primary resultを変えずtemporary cleanup failureを表示する
warning() {
    printf 'Warning: %s\n' "$1" >&2
}


# --------------------------------------------------
# Prerequisites
# --------------------------------------------------

# required commandが固定pathのexecutableであることを確認する
validate_executable() {
    local path=$1
    if [[ ! -x $path ]]; then
        error "required command is unavailable: $path"
        return 1
    fi
}

# HOMEがabsolute non-empty pathであることを確認する
validate_home() {
    if [[ -z ${HOME-} || $HOME != /* ]]; then
        error 'HOME must be an absolute non-empty path'
        return 1
    fi
}

# bootstrap temporary rootの形状を確認する
temporary_root() {
    local root=${TMPDIR:-/tmp}
    if [[ $root != /* ]]; then
        error 'TMPDIR must be an absolute path'
        return 1
    fi
    if [[ ! -d $root || -L $root ]]; then
        error "temporary root must be an existing non-symlink directory: $root"
        return 1
    fi
    printf '%s\n' "$root"
}

# network access前にbootstrapの全前提を確認する
validate_prerequisites() {
    local command
    for command in "$CURL" "$MKTEMP" "$SHA256SUM" "$CHMOD" "$RM"; do
        validate_executable "$command" || return 1
    done
    validate_home || return 1
    temporary_root >/dev/null || return 1
}


# --------------------------------------------------
# Confirmation
# --------------------------------------------------

# install confirmationをaccepted inputまで繰り返す
confirm_install() {
    local answer
    while true; do
        printf 'Install Eiyah? [Y/n] '
        if ! IFS= read -r answer; then
            error 'failed to read installation confirmation'
            return 2
        fi
        case ${answer,,} in
            '' | y | yes)
                return 0
                ;;
            n | no)
                return 1
                ;;
            *)
                ;;
        esac
    done
}


# --------------------------------------------------
# Public Release
# --------------------------------------------------

# latest Release redirectのfinal effective URLを取得する
latest_release_effective_url() {
    "$CURL" -q --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' \
        --output /dev/null --write-out '%{url_effective}' \
        -- "$LATEST_RELEASE_URL"
}

# final Release URLからstable vSEMVER tagを取得する
release_tag_from_url() {
    local effective_url=$1
    local prefix=https://github.com/su-ito-lab/eiyah/releases/tag/
    local tag=${effective_url#"$prefix"}
    local identifier='(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
    local semver="^v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)(-${identifier}(\\.${identifier})*)?(\\+[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?$"
    if [[ $tag == "$effective_url" || ! $tag =~ $semver ]]; then
        error "latest Public Release URL has an invalid tag: $effective_url"
        return 1
    fi
    printf '%s\n' "$tag"
}

# latest stable Public Release tagを1回だけ確定する
discover_release_tag() {
    local effective_url
    if ! effective_url=$(latest_release_effective_url); then
        error 'failed to discover latest Public Release'
        return 1
    fi
    release_tag_from_url "$effective_url"
}

# HTTPS assetをattempt directory内のtargetへ取得する
download_file() {
    local url=$1
    local target=$2
    "$CURL" -q --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' \
        --output "$target" -- "$url"
}

# 同じRelease tagからbinaryとchecksumを取得する
download_release_assets() {
    local tag=$1
    local directory=$2
    local release_url=$RELEASE_DOWNLOAD_ROOT/$tag
    download_file "$release_url/$BINARY_ASSET" "$directory/$BINARY_ASSET" || return 1
    download_file "$release_url/$CHECKSUM_ASSET" "$directory/$CHECKSUM_ASSET" || return 1
}


# --------------------------------------------------
# Verification and Execution
# --------------------------------------------------

# checksum fileがexact one-line contractに従うことを確認する
validate_checksum_format() {
    local checksum=$1
    local line extra
    exec 3<"$checksum" || return 1
    if ! IFS= read -r line <&3; then
        exec 3<&-
        error 'checksum file must contain exactly one newline-terminated line'
        return 1
    fi
    if IFS= read -r extra <&3 || [[ -n ${extra-} ]]; then
        exec 3<&-
        error 'checksum file must contain exactly one line'
        return 1
    fi
    exec 3<&-
    if [[ ! $line =~ ^[0-9a-f]{64}[[:space:]][[:space:]]${BINARY_ASSET}$ || $line != *"  $BINARY_ASSET" ]]; then
        error 'checksum file has an invalid format or filename'
        return 1
    fi
}

# checksum formatとdownload済みbinary digestを検証する
verify_release_assets() {
    local directory=$1
    validate_checksum_format "$directory/$CHECKSUM_ASSET" || return 1
    if ! (cd "$directory" && "$SHA256SUM" --check --status "$CHECKSUM_ASSET"); then
        error 'downloaded Eiyah checksum does not match'
        return 1
    fi
}

# checksum検証済みtemporary Eiyahでinitial installを実行する
run_temporary_install() {
    local binary=$1
    "$binary" __install
}


# --------------------------------------------------
# Bootstrap Lifecycle
# --------------------------------------------------

# bootstrap attempt directoryだけをbest-effort cleanupする
cleanup_temporary() {
    if [[ -n $attempt_directory && ( -e $attempt_directory || -L $attempt_directory ) ]]; then
        if ! "$RM" --recursive --force -- "$attempt_directory"; then
            warning "failed to cleanup bootstrap temporary directory: $attempt_directory"
        fi
    fi
    return 0
}

# Checkpoint Aのinstallation bootstrapを順序通り実行する
main() {
    local confirmation_status root tag binary
    validate_prerequisites || return 1

    confirm_install
    confirmation_status=$?
    case $confirmation_status in
        0) ;;
        1) return 0 ;;
        *) return "$confirmation_status" ;;
    esac

    root=$(temporary_root) || return 1
    if ! attempt_directory=$("$MKTEMP" -d "$root/eiyah-bootstrap.XXXXXXXXXX"); then
        error 'failed to create bootstrap temporary directory'
        return 1
    fi
    trap cleanup_temporary EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    tag=$(discover_release_tag) || return 1
    download_release_assets "$tag" "$attempt_directory" || {
        error 'failed to download Public Release assets'
        return 1
    }
    verify_release_assets "$attempt_directory" || return 1

    binary=$attempt_directory/$BINARY_ASSET
    if ! "$CHMOD" 0755 "$binary"; then
        error 'failed to make temporary Eiyah executable'
        return 1
    fi
    run_temporary_install "$binary"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main
    exit $?
fi
