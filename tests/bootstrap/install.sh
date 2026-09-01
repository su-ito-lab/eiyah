#!/usr/bin/bash

# ==================================================
# @file tests/bootstrap/install.sh
# @brief Installation bootstrap contract tests
# ==================================================

set -u
set -o pipefail

test_directory=${BASH_SOURCE[0]%/*}
repository_root=$(cd "$test_directory/../.." && pwd -P)

# shellcheck source=../../install.sh
source "$repository_root/install.sh"

test_root=$(/usr/bin/mktemp -d /tmp/eiyah-install-bootstrap-test.XXXXXXXXXX)
trap '/usr/bin/rm --recursive --force -- "$test_root"' EXIT

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

set -e


# --------------------------------------------------
# Prerequisites and Confirmation
# --------------------------------------------------

assert_status 1 validate_executable "$test_root/missing-command"

saved_home=${HOME-}
HOME=
assert_status 1 validate_home
HOME=relative
assert_status 1 validate_home
HOME=$saved_home

mkdir "$test_root/temporary-root"
TMPDIR=$test_root/temporary-root
[[ $(temporary_root) == "$TMPDIR" ]] || fail 'absolute temporary root was not accepted'
TMPDIR=relative
assert_status 1 temporary_root
TMPDIR=$test_root/temporary-root-link
ln -s "$test_root/temporary-root" "$TMPDIR"
assert_status 1 temporary_root
TMPDIR=$test_root/temporary-root

assert_status 0 confirm_install <<<''
assert_status 0 confirm_install <<<'y'
assert_status 0 confirm_install <<<'YES'
assert_status 1 confirm_install <<<'n'
assert_status 1 confirm_install <<<'No'
assert_status 0 confirm_install <<<$'invalid\nY'
assert_status 2 confirm_install </dev/null


# --------------------------------------------------
# Release and Checksum
# --------------------------------------------------

[[ $(release_tag_from_url 'https://github.com/su-ito-lab/eiyah/releases/tag/v1.2.3') == v1.2.3 ]] \
    || fail 'valid release tag was not parsed'
[[ $(release_tag_from_url 'https://github.com/su-ito-lab/eiyah/releases/tag/v1.2.3-rc.1+build.2') == v1.2.3-rc.1+build.2 ]] \
    || fail 'valid semantic version tag was not parsed'
assert_status 1 release_tag_from_url 'https://github.com/su-ito-lab/eiyah/releases/latest'
assert_status 1 release_tag_from_url 'https://github.com/su-ito-lab/eiyah/releases/tag/v01.2.3'

download_log=$test_root/downloads
download_file() {
    printf '%s\n' "$1" >>"$download_log"
    : >"$2"
}
mkdir "$test_root/assets"
download_release_assets v1.2.3 "$test_root/assets"
expected_urls=$(printf '%s\n%s\n' \
    "$RELEASE_DOWNLOAD_ROOT/v1.2.3/$BINARY_ASSET" \
    "$RELEASE_DOWNLOAD_ROOT/v1.2.3/$CHECKSUM_ASSET")
[[ $(<"$download_log") == "$expected_urls" ]] || fail 'assets did not use the same fixed tag'

binary=$test_root/assets/$BINARY_ASSET
checksum=$test_root/assets/$CHECKSUM_ASSET
printf 'verified binary\n' >"$binary"
digest=$(/usr/bin/sha256sum "$binary")
digest=${digest%% *}
printf '%s  %s\n' "$digest" "$BINARY_ASSET" >"$checksum"
verify_release_assets "$test_root/assets"

printf '%s  wrong-name\n' "$digest" >"$checksum"
assert_status 1 verify_release_assets "$test_root/assets"
printf '%064d  %s\n' 0 "$BINARY_ASSET" >"$checksum"
assert_status 1 verify_release_assets "$test_root/assets"
printf '%s  %s' "$digest" "$BINARY_ASSET" >"$checksum"
assert_status 1 verify_release_assets "$test_root/assets"


# --------------------------------------------------
# Execution and Cleanup
# --------------------------------------------------

invocation_log=$test_root/invocation
fake_eiyah=$test_root/fake-eiyah
printf '#!/usr/bin/bash\nprintf "%%s\\n" "$1" >"%s"\nexit 37\n' "$invocation_log" >"$fake_eiyah"
/usr/bin/chmod 0755 "$fake_eiyah"
assert_status 37 run_temporary_install "$fake_eiyah"
[[ $(<"$invocation_log") == __install ]] || fail 'temporary Eiyah invocation was not canonical'

confirm_install() {
    return 0
}
discover_release_tag() {
    printf 'v1.2.3\n'
}
download_release_assets() {
    local directory=$2
    printf '#!/usr/bin/bash\nprintf "%%s\\n" "$1" >"%s"\nexit "${FAKE_INSTALL_STATUS}"\n' \
        "$invocation_log" >"$directory/$BINARY_ASSET"
    local generated_digest
    generated_digest=$(/usr/bin/sha256sum "$directory/$BINARY_ASSET")
    generated_digest=${generated_digest%% *}
    if [[ ${FAKE_CHECKSUM_VALID:-1} == 1 ]]; then
        printf '%s  %s\n' "$generated_digest" "$BINARY_ASSET" >"$directory/$CHECKSUM_ASSET"
    else
        printf '%064d  %s\n' 0 "$BINARY_ASSET" >"$directory/$CHECKSUM_ASSET"
    fi
}

run_main_in_subshell() {
    (main)
}

FAKE_INSTALL_STATUS=0
FAKE_CHECKSUM_VALID=1
export FAKE_CHECKSUM_VALID FAKE_INSTALL_STATUS TMPDIR
: >"$invocation_log"
run_main_in_subshell
[[ $(<"$invocation_log") == __install ]] || fail 'main did not invoke temporary Eiyah'
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'successful bootstrap did not cleanup its temporary directory'
fi

FAKE_CHECKSUM_VALID=0
: >"$invocation_log"
assert_status 1 run_main_in_subshell
[[ ! -s $invocation_log ]] || fail 'checksum failure executed temporary Eiyah'
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'checksum failure did not cleanup its temporary directory'
fi
FAKE_CHECKSUM_VALID=1

FAKE_INSTALL_STATUS=23
assert_status 23 run_main_in_subshell
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'failed lifecycle did not cleanup its temporary directory'
fi

run_temporary_install() {
    kill -TERM "$BASHPID"
}
assert_status 143 run_main_in_subshell
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'signaled bootstrap did not cleanup its temporary directory'
fi

run_temporary_install() {
    /usr/bin/chmod 0555 "$TMPDIR"
    return 23
}
set +e
run_main_in_subshell 2>"$test_root/cleanup-failure-error"
cleanup_failure_status=$?
set -e
/usr/bin/chmod 0755 "$TMPDIR"
[[ $cleanup_failure_status -eq 23 ]] || fail 'cleanup failure replaced the lifecycle status'
cleanup_failure_error=$(<"$test_root/cleanup-failure-error")
[[ $cleanup_failure_error == *'Warning: failed to cleanup bootstrap temporary directory: '* ]] \
    || fail 'cleanup failure did not emit a warning'
/usr/bin/rm --recursive --force -- "$TMPDIR"/eiyah-bootstrap.*

download_release_assets() {
    return 9
}
: >"$invocation_log"
assert_status 1 run_main_in_subshell
[[ ! -s $invocation_log ]] || fail 'bootstrap failure executed temporary Eiyah'
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'bootstrap failure did not cleanup its temporary directory'
fi

confirm_install() {
    return 1
}
discover_release_tag() {
    : >"$test_root/network-started"
    printf 'v1.2.3\n'
}
assert_status 0 run_main_in_subshell
[[ ! -e $test_root/network-started ]] || fail 'cancelled bootstrap started network discovery'
if compgen -G "$TMPDIR/eiyah-bootstrap.*" >/dev/null; then
    fail 'cancelled bootstrap created a temporary directory'
fi

printf 'PASS: install bootstrap tests\n'
