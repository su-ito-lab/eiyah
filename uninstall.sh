#!/usr/bin/bash

# ==================================================
# @file uninstall.sh
# @brief Public Eiyah uninstallation bootstrap
# ==================================================


set -u
set -o pipefail

script_directory=${BASH_SOURCE[0]%/*}
if [[ $script_directory == "${BASH_SOURCE[0]}" ]]; then
    script_directory=.
fi

# shellcheck source=install.sh
source "$script_directory/install.sh"

readonly READLINK=/usr/bin/readlink
readonly STAT=/usr/bin/stat
readonly CLEANUP_PLAN_NAME=uninstall-cleanup-plan

cleanup_eiyah_binary=
cleanup_eiyah_entry=
cleanup_state_root=
cleanup_lock=
initial_eiyah_binary_identity=
initial_eiyah_entry_identity=
initial_state_root_identity=
initial_lock_identity=


# --------------------------------------------------
# Confirmation and Prerequisites
# --------------------------------------------------

# uninstall固有commandを含むbootstrap前提をnetwork access前に確認する
validate_uninstall_prerequisites() {
    validate_prerequisites || return 1
    validate_executable "$READLINK" || return 1
    validate_executable "$STAT"
}

# uninstall confirmationをaccepted inputまで繰り返す
confirm_uninstall() {
    local answer
    while true; do
        printf 'Uninstall Eiyah? [y/N] '
        if ! IFS= read -r answer; then
            error 'failed to read uninstallation confirmation'
            return 2
        fi
        case ${answer,,} in
            y | yes)
                return 0
                ;;
            '' | n | no)
                return 1
                ;;
            *)
                ;;
        esac
    done
}


# --------------------------------------------------
# Cleanup Plan
# --------------------------------------------------

# lowercase hexadecimalをUnix path bytesへdecodeする
decode_path() {
    local hexadecimal=$1
    local output_name=$2
    local escaped='' pair index decoded
    if [[ -z $hexadecimal || $hexadecimal =~ [^0-9a-f] || $((${#hexadecimal} % 2)) -ne 0 ]]; then
        error 'cleanup plan path must be non-empty lowercase hexadecimal'
        return 1
    fi
    for ((index = 0; index < ${#hexadecimal}; index += 2)); do
        pair=${hexadecimal:index:2}
        if [[ $pair == 00 ]]; then
            error 'cleanup plan path contains a NUL byte'
            return 1
        fi
        escaped+="\\x$pair"
    done
    printf -v decoded '%b' "$escaped"
    if [[ -z $decoded || $decoded != /* ]]; then
        error 'cleanup plan path must be a non-empty absolute path'
        return 1
    fi
    printf -v "$output_name" '%s' "$decoded"
}

# fixed-order cleanup planを全件検証してpath globalsへloadする
load_cleanup_plan() {
    local plan=$1
    local line extra field hexadecimal
    local -a fields=(eiyah-binary eiyah-entry state-root lock)
    local -a outputs=(cleanup_eiyah_binary cleanup_eiyah_entry cleanup_state_root cleanup_lock)
    local index

    if IFS= read -r -d '' _ <"$plan"; then
        error 'cleanup plan must not contain a NUL byte'
        return 1
    fi
    exec 3<"$plan" || {
        error "failed to open cleanup plan: $plan"
        return 1
    }
    for ((index = 0; index < ${#fields[@]}; index += 1)); do
        if ! IFS= read -r line <&3; then
            exec 3<&-
            error 'cleanup plan must contain exactly four newline-terminated lines'
            return 1
        fi
        field=${fields[index]}
        if [[ $line != "$field="* ]]; then
            exec 3<&-
            error "cleanup plan field is missing or out of order: $field"
            return 1
        fi
        hexadecimal=${line#*=}
        if ! decode_path "$hexadecimal" "${outputs[index]}"; then
            exec 3<&-
            return 1
        fi
    done
    if IFS= read -r extra <&3 || [[ -n ${extra-} ]]; then
        exec 3<&-
        error 'cleanup plan must contain exactly four lines'
        return 1
    fi
    exec 3<&-
}

# plan relationshipと全persistent target形状を削除開始前に検証する
validate_final_cleanup() {
    local expected_entry=$HOME/.local/bin/eiyah
    if [[ $cleanup_eiyah_entry != "$expected_entry" ]]; then
        error 'cleanup plan contains an unexpected Eiyah public entry'
        return 1
    fi
    if [[ $cleanup_lock != "$cleanup_state_root/lock" ]]; then
        error 'cleanup plan lock is not directly under the state root'
        return 1
    fi
    if [[ $cleanup_state_root == / || ${cleanup_state_root##*/} != eiyah ]]; then
        error 'cleanup plan state root is not an Eiyah namespace'
        return 1
    fi
    validate_eiyah_entry || return 1
    validate_eiyah_binary || return 1
    validate_state_root || return 1
    initial_eiyah_entry_identity=$(path_identity "$cleanup_eiyah_entry") || return 1
    initial_eiyah_binary_identity=$(path_identity "$cleanup_eiyah_binary") || return 1
    initial_state_root_identity=$(path_identity "$cleanup_state_root") || return 1
    initial_lock_identity=$(path_identity "$cleanup_lock") || return 1
}

# symlinkをfollowせずUnix device / inode identityを取得する
path_identity() {
    local path=$1
    local identity
    identity=$("$STAT" --format='%d:%i' -- "$path") || {
        error "failed to read filesystem identity: $path"
        return 1
    }
    if [[ ! $identity =~ ^[0-9]+:[0-9]+$ ]]; then
        error "invalid filesystem identity: $path"
        return 1
    fi
    printf '%s\n' "$identity"
}

# targetのcurrent identityがinitial identityと一致することを確認する
validate_identity() {
    local path=$1
    local expected=$2
    local current
    current=$(path_identity "$path") || return 1
    if [[ $current != "$expected" ]]; then
        error "filesystem identity changed: $path"
        return 1
    fi
}

# public entryがexpected absolute symlinkであることを検証する
validate_eiyah_entry() {
    local target
    if [[ ! -L $cleanup_eiyah_entry ]]; then
        error 'Eiyah public entry must be a symlink'
        return 1
    fi
    target=$($READLINK -- "$cleanup_eiyah_entry") || return 1
    if [[ $target != /* || $target != "$cleanup_eiyah_binary" ]]; then
        error 'Eiyah public entry has an unexpected target'
        return 1
    fi
}

# installed Eiyahがexecutable regular non-symlink fileであることを検証する
validate_eiyah_binary() {
    if [[ ! -f $cleanup_eiyah_binary || -L $cleanup_eiyah_binary || ! -x $cleanup_eiyah_binary ]]; then
        error 'installed Eiyah must be an executable regular non-symlink file'
        return 1
    fi
}

# state rootと直下lockの削除可能な形状を検証する
validate_state_root() {
    if [[ ! -d $cleanup_state_root || -L $cleanup_state_root ]]; then
        error 'Eiyah state root must be a non-symlink directory'
        return 1
    fi
    if [[ ! -f $cleanup_lock || -L $cleanup_lock ]]; then
        error 'Eiyah lock must be a regular non-symlink file'
        return 1
    fi
}


# --------------------------------------------------
# Final Cleanup
# --------------------------------------------------

# expected targetを直前に再検証してfixed orderでfinal cleanupする
run_final_cleanup() {
    validate_eiyah_entry || return 1
    validate_identity "$cleanup_eiyah_entry" "$initial_eiyah_entry_identity" || return 1
    "$RM" --force -- "$cleanup_eiyah_entry" || {
        error 'failed to remove Eiyah public entry'
        return 1
    }
    validate_eiyah_binary || return 1
    validate_identity "$cleanup_eiyah_binary" "$initial_eiyah_binary_identity" || return 1
    "$RM" --force -- "$cleanup_eiyah_binary" || {
        error 'failed to remove installed Eiyah binary'
        return 1
    }
    validate_state_root || return 1
    validate_identity "$cleanup_state_root" "$initial_state_root_identity" || return 1
    validate_identity "$cleanup_lock" "$initial_lock_identity" || return 1
    "$RM" --recursive --force -- "$cleanup_state_root" || {
        error 'failed to remove Eiyah state root'
        return 1
    }
}

# temporary Eiyahへcanonical uninstall protocolを渡す
run_temporary_uninstall() {
    local binary=$1
    local plan=$2
    "$binary" __uninstall --cleanup-plan "$plan"
}


# --------------------------------------------------
# Bootstrap Lifecycle
# --------------------------------------------------

# uninstall bootstrapを順序通り実行する
main() {
    local confirmation_status root tag binary cleanup_plan
    validate_uninstall_prerequisites || return 1

    confirm_uninstall
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
    cleanup_plan=$attempt_directory/$CLEANUP_PLAN_NAME
    if [[ -e $cleanup_plan || -L $cleanup_plan ]]; then
        error 'cleanup plan target already exists'
        return 1
    fi
    run_temporary_uninstall "$binary" "$cleanup_plan" || return $?
    load_cleanup_plan "$cleanup_plan" || return 1
    validate_final_cleanup || return 1
    run_final_cleanup
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main
    exit $?
fi
