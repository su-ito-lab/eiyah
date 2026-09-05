#!/usr/bin/bash

# ==================================================
# @file tests/bootstrap/uninstall.sh
# @brief Uninstallation bootstrap contract tests
# ==================================================


set -u
set -o pipefail

test_directory=${BASH_SOURCE[0]%/*}
repository_root=$(cd "$test_directory/../.." && pwd -P)

# shellcheck source=../../uninstall.sh
source "$repository_root/uninstall.sh"

test_root=$(/usr/bin/mktemp -d /tmp/eiyah-uninstall-bootstrap-test.XXXXXXXXXX)
trap '/usr/bin/chmod 0755 -- "$test_root" 2>/dev/null || true; /usr/bin/rm --recursive --force -- "$test_root"' EXIT

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    exit 1
}

assert_status() {
    local expected=$1
    shift
    set +e
    "$@"
    local actual=$?
    set -e
    [[ $actual -eq $expected ]] || fail "expected status $expected, got $actual: $*"
}

hex_path() {
    local path=$1
    local encoded='' character value index
    LC_ALL=C
    for ((index = 0; index < ${#path}; index += 1)); do
        character=${path:index:1}
        printf -v value '%d' "'$character"
        printf -v value '%02x' "$value"
        encoded+=$value
    done
    printf '%s' "$encoded"
}

write_plan() {
    local plan=$1
    printf 'eiyah-binary=%s\neiyah-entry=%s\nstate-root=%s\nlock=%s\n' \
        "$(hex_path "$fixture_binary")" \
        "$(hex_path "$fixture_entry")" \
        "$(hex_path "$fixture_state_root")" \
        "$(hex_path "$fixture_lock")" >"$plan"
}

create_cleanup_fixture() {
    fixture_home=$test_root/home
    fixture_binary=$test_root/prefix/bin/eiyah
    fixture_entry=$fixture_home/.local/bin/eiyah
    fixture_state_root=$test_root/state/eiyah
    fixture_lock=$fixture_state_root/lock
    /usr/bin/rm --recursive --force -- \
        "$fixture_home" "$test_root/prefix" "$test_root/state"
    /usr/bin/mkdir -p "${fixture_binary%/*}" "${fixture_entry%/*}" "$fixture_state_root"
    printf '#!/usr/bin/bash\n' >"$fixture_binary"
    /usr/bin/chmod 0755 "$fixture_binary"
    /usr/bin/ln -s "$fixture_binary" "$fixture_entry"
    printf 'lock\n' >"$fixture_lock"
    HOME=$fixture_home
    export HOME
}

set -e


# --------------------------------------------------
# Confirmation and Plan Validation
# --------------------------------------------------

assert_status 1 confirm_uninstall <<<''
assert_status 0 confirm_uninstall <<<'y'
assert_status 0 confirm_uninstall <<<'YES'
assert_status 1 confirm_uninstall <<<'n'
assert_status 1 confirm_uninstall <<<'No'
assert_status 0 confirm_uninstall <<<$'invalid\nY'
assert_status 2 confirm_uninstall </dev/null

overview=$(uninstall_overview)
expected_overview=$(printf '%s\n' '==> Eiyah will remove:' 'Eiyah' 'show-cad-status' \
    'Pixi environment' 'Eiyah configuration' 'managed dotfiles' '' \
    'Previously backed up configuration will be restored when present.' \
    'SSH keys and authorized_keys changes will be kept.')
[[ $overview == "$expected_overview" ]] || fail 'uninstall overview did not match the UI contract'

create_cleanup_fixture
valid_plan=$test_root/valid-plan
write_plan "$valid_plan"
cleanup_eiyah_binary=
cleanup_eiyah_entry=
cleanup_state_root=
cleanup_lock=
load_cleanup_plan "$valid_plan"
validate_final_cleanup
[[ $cleanup_eiyah_binary == "$fixture_binary" ]] || fail 'binary path was decoded incorrectly'
[[ $cleanup_eiyah_entry == "$fixture_entry" ]] || fail 'entry path was decoded incorrectly'
[[ $cleanup_state_root == "$fixture_state_root" ]] || fail 'state root was decoded incorrectly'
[[ $cleanup_lock == "$fixture_lock" ]] || fail 'lock path was decoded incorrectly'
non_utf8_path=
decode_path 2fff non_utf8_path
[[ $(hex_path "$non_utf8_path") == 2fff ]] || fail 'non-UTF-8 path bytes were not preserved'

printf 'eiyah-entry=2f746d70\n' >"$test_root/missing-fields"
assert_status 1 load_cleanup_plan "$test_root/missing-fields"
printf '\0' >"$test_root/nul-plan"
assert_status 1 load_cleanup_plan "$test_root/nul-plan"
sed '1s/eiyah-binary/unknown/' "$valid_plan" >"$test_root/unknown-field"
assert_status 1 load_cleanup_plan "$test_root/unknown-field"
sed '1s/=.*/=2F/' "$valid_plan" >"$test_root/uppercase-hex"
assert_status 1 load_cleanup_plan "$test_root/uppercase-hex"
sed '1s/=.*/=2f0/' "$valid_plan" >"$test_root/odd-hex"
assert_status 1 load_cleanup_plan "$test_root/odd-hex"
sed '1s/=.*/=00/' "$valid_plan" >"$test_root/nul-path"
assert_status 1 load_cleanup_plan "$test_root/nul-path"
sed '1s/=.*/=72656c6174697665/' "$valid_plan" >"$test_root/relative-path"
assert_status 1 load_cleanup_plan "$test_root/relative-path"
printf 'extra=2f\n' >>"$valid_plan"
assert_status 1 load_cleanup_plan "$valid_plan"


# --------------------------------------------------
# Final Cleanup Safety
# --------------------------------------------------

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
/usr/bin/rm --force -- "$fixture_entry"
/usr/bin/ln -s "$test_root/wrong" "$fixture_entry"
assert_status 1 run_final_cleanup
[[ -L $fixture_entry ]] || fail 'wrong public entry was removed'
[[ -f $fixture_binary ]] || fail 'binary was removed after entry validation failure'
[[ -d $fixture_state_root ]] || fail 'state root was removed after entry validation failure'

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
/usr/bin/mv -- "$fixture_entry" "$test_root/original-entry"
/usr/bin/ln -s "$fixture_binary" "$fixture_entry"
assert_status 1 run_final_cleanup
[[ -L $fixture_entry && -f $fixture_binary && -d $fixture_state_root ]] \
    || fail 'same-shape public entry replacement was removed'

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
/usr/bin/mv -- "$fixture_binary" "$test_root/original-binary"
printf '#!/usr/bin/bash\n' >"$fixture_binary"
/usr/bin/chmod 0755 "$fixture_binary"
assert_status 1 run_final_cleanup
[[ -f $fixture_binary && -d $fixture_state_root ]] \
    || fail 'same-shape binary replacement or later target was removed'

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
/usr/bin/mv -- "$fixture_state_root" "$test_root/original-state-root"
/usr/bin/mkdir "$fixture_state_root"
printf 'lock\n' >"$fixture_lock"
assert_status 1 run_final_cleanup
[[ -d $fixture_state_root && -f $fixture_lock ]] \
    || fail 'same-shape state root replacement was removed'

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
/usr/bin/mv -- "$fixture_lock" "$test_root/original-lock"
printf 'lock\n' >"$fixture_lock"
assert_status 1 run_final_cleanup
[[ -d $fixture_state_root && -f $fixture_lock ]] \
    || fail 'same-shape lock replacement allowed state root removal'

/usr/bin/rm --recursive --force -- \
    "$test_root/original-entry" "$test_root/original-binary" \
    "$test_root/original-state-root" "$test_root/original-lock"

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
run_final_cleanup
[[ ! -e $fixture_entry && ! -L $fixture_entry ]] || fail 'public entry was not removed'
[[ ! -e $fixture_binary ]] || fail 'installed binary was not removed'
[[ ! -e $fixture_state_root ]] || fail 'state root was not removed'
[[ -d ${fixture_binary%/bin/eiyah} ]] || fail 'Eiyah prefix parent was removed'
[[ -d ${fixture_entry%/eiyah} ]] || fail 'public entry parent was removed'
[[ -d ${fixture_state_root%/eiyah} ]] || fail 'state root parent was removed'

create_cleanup_fixture
write_plan "$valid_plan"
load_cleanup_plan "$valid_plan"
validate_final_cleanup
/usr/bin/chmod 0555 "${fixture_entry%/eiyah}"
assert_status 1 run_final_cleanup
/usr/bin/chmod 0755 "${fixture_entry%/eiyah}"
[[ -L $fixture_entry && -f $fixture_binary && -d $fixture_state_root ]] \
    || fail 'final cleanup continued after a target removal failure'


# --------------------------------------------------
# Bootstrap Integration
# --------------------------------------------------

confirm_uninstall() {
    printf 'Uninstall Eiyah? [y/N] y\n'
    return 0
}
discover_release_tag() {
    printf 'v1.2.3\n'
}
download_release_assets() {
    local directory=$2
    printf '#!/usr/bin/bash\nprintf "%%s\\n" "$*" >"%s"\nprintf "%%s\\n" "${FAKE_UNINSTALL_OUTPUT-}"\nprintf "eiyah-binary=%%s\\neiyah-entry=%%s\\nstate-root=%%s\\nlock=%%s\\n" "%s" "%s" "%s" "%s" >"$3"\nexit %s\n' \
        "$test_root/invocation" \
        "$(hex_path "$fixture_binary")" \
        "$(hex_path "$fixture_entry")" \
        "$(hex_path "$fixture_state_root")" \
        "$(hex_path "$fixture_lock")" \
        "$FAKE_UNINSTALL_STATUS" >"$directory/$BINARY_ASSET"
    local digest
    digest=$(/usr/bin/sha256sum "$directory/$BINARY_ASSET")
    printf '%s  %s\n' "${digest%% *}" "$BINARY_ASSET" >"$directory/$CHECKSUM_ASSET"
}

run_main_in_subshell() {
    (main)
}

create_cleanup_fixture
export fixture_binary fixture_entry fixture_state_root fixture_lock
FAKE_UNINSTALL_STATUS=0
FAKE_UNINSTALL_OUTPUT=$'\n'$(printf '%s\n' \
    '==> Unlinking configuration files' '' \
    '==> Removing show-cad-status' "$fixture_home/.local/bin/show-cad-status" '' \
    '==> Removing Eiyah configuration' "$test_root/config/eiyah/config.toml" \
    "$fixture_home/.dotfiles" '' \
    '==> Removing Pixi environment' "$test_root/data/eiyah/pixi" '' \
    '==> Restoring previous configuration' "$fixture_home/.cshrc")
export FAKE_UNINSTALL_OUTPUT FAKE_UNINSTALL_STATUS
TMPDIR=$test_root/temporary
/usr/bin/mkdir "$TMPDIR"
export TMPDIR
main_output=$(run_main_in_subshell)
[[ $(<"$test_root/invocation") == "__uninstall --cleanup-plan $TMPDIR/"eiyah-bootstrap.* ]] \
    || fail 'temporary Eiyah invocation was not canonical'
expected_main_output=$(printf '%s\n' '==> Eiyah will remove:' 'Eiyah' 'show-cad-status' \
    'Pixi environment' 'Eiyah configuration' 'managed dotfiles' '' \
    'Previously backed up configuration will be restored when present.' \
    'SSH keys and authorized_keys changes will be kept.' '' 'Uninstall Eiyah? [y/N] y' '' \
    '==> Downloading Eiyah 1.2.3' \
    "$RELEASE_DOWNLOAD_ROOT/v1.2.3/$BINARY_ASSET" '' \
    '==> Verifying Eiyah download' 'SHA-256: verified' \
    "$FAKE_UNINSTALL_OUTPUT" '' '==> Removing Eiyah' "$fixture_entry" '' \
    '==> Eiyah uninstall complete')
[[ $main_output == "$expected_main_output" ]] || fail 'uninstall output did not match the UI contract'
[[ ! -e $fixture_entry && ! -L $fixture_entry ]] || fail 'bootstrap did not remove public entry'
[[ ! -e $fixture_binary ]] || fail 'bootstrap did not remove installed binary'
[[ ! -e $fixture_state_root ]] || fail 'bootstrap did not remove state root'
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'successful uninstall bootstrap did not cleanup temporary content'
fi

create_cleanup_fixture
FAKE_UNINSTALL_STATUS=19
export FAKE_UNINSTALL_STATUS
assert_status 19 run_main_in_subshell
[[ -L $fixture_entry && -f $fixture_binary && -d $fixture_state_root ]] \
    || fail 'failed __uninstall started persistent final cleanup'
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'failed uninstall bootstrap did not cleanup temporary content'
fi

create_cleanup_fixture
FAKE_UNINSTALL_STATUS=0
/usr/bin/chmod 0555 "${fixture_entry%/eiyah}"
set +e
run_main_in_subshell >"$test_root/final-cleanup-output" 2>"$test_root/final-cleanup-error"
final_cleanup_status=$?
set -e
/usr/bin/chmod 0755 "${fixture_entry%/eiyah}"
[[ $final_cleanup_status -eq 1 ]] || fail 'final cleanup failure did not fail uninstall'
mapfile -t final_cleanup_diagnostics <"$test_root/final-cleanup-error"
[[ ${#final_cleanup_diagnostics[@]} -eq 3 ]] \
    || fail 'final cleanup failure emitted an unexpected diagnostic count'
[[ ${final_cleanup_diagnostics[0]} == "Error: failed to remove $fixture_entry: "?* ]] \
    || fail 'final cleanup failure did not report the path and error detail'
[[ ${final_cleanup_diagnostics[1]} == 'Warning: Eiyah configuration has already been removed.' ]] \
    || fail 'final cleanup failure did not report removed configuration'
[[ ${final_cleanup_diagnostics[2]} == 'Hint: Uninstallation is incomplete.' ]] \
    || fail 'final cleanup failure did not report incomplete uninstall'

confirm_uninstall() {
    return 1
}
discover_release_tag() {
    : >"$test_root/network-started"
    printf 'v1.2.3\n'
}
assert_status 0 run_main_in_subshell
[[ ! -e $test_root/network-started ]] || fail 'cancelled uninstall started network discovery'

printf 'PASS: uninstall bootstrap tests\n'
