# Resolve path to `fluvio` binary instead of expecting it in PATH
# Search order: $FLUVIO_BIN, in PATH, current directory, home directory
main() {
    check_load_bats_libraries;
    check_fluvio_bin_path;
    check_timeout_bin;
    check_fluvio_cluster;
}

function check_fluvio_bin_path() {

    if [[ -n $FLUVIO_BIN ]]; then
        if [[ -n $DEBUG ]]; then
            echo "# DEBUG: found: FLUVIO_BIN was defined" >&3
        fi
        set_fluvio_bin_path_then_exit "$FLUVIO_BIN";
    elif which fluvio; then
        if [[ -n $DEBUG ]]; then
            echo "# DEBUG: found: fluvio in PATH" >&3
        fi
        set_fluvio_bin_path_then_exit "$(which fluvio)";
    elif test -f "$(pwd)/fluvio"; then
        if [[ -n $DEBUG ]]; then
            echo "# DEBUG: found: fluvio in current directory" >&3
        fi
        set_fluvio_bin_path_then_exit "$(pwd)/fluvio";
    elif test -f "$HOME/.fluvio/bin/fluvio"; then
        if [[ -n $DEBUG ]]; then
            echo "# DEBUG: found: fluvio in home directory" >&3
        fi
        set_fluvio_bin_path_then_exit "$HOME/.fluvio/bin/fluvio";
    fi
}

function set_fluvio_bin_path_then_exit() {
    FLUVIO_BIN=$1
    export FLUVIO_BIN
    if [[ -n $DEBUG ]]; then
        echo "# DEBUG: Fluvio binary path: $FLUVIO_BIN" >&3
    fi

}

function check_fluvio_cluster() {
    if [[ -n $DEBUG ]]; then
        echo "# DEBUG: Attempting to start cluster with fluvio bin @ $FLUVIO_BIN" >&3
    fi
    run "$FLUVIO_BIN" cluster start
}

# Make sure Bats-core helper libraries are installed
function check_load_bats_libraries() {
    # Look for bats-support, bats-assert, bats-file
    # If not there, try to clone it into place

    if ! test -d "$BATS_TEST_DIRNAME/../test_helper/bats-support"; then
        echo "# Installing bats-support in $BATS_TEST_DIRNAME/../test_helper" >&3
        git clone https://github.com/bats-core/bats-support "$BATS_TEST_DIRNAME/../test_helper/bats-support"
    fi

    if ! test -d "$BATS_TEST_DIRNAME/../test_helper/bats-assert"; then
        echo "# Installing bats-assert in $BATS_TEST_DIRNAME/../test_helper" >&3
        git clone https://github.com/bats-core/bats-assert "$BATS_TEST_DIRNAME/../test_helper/bats-assert"
    fi
}

function check_timeout_bin() {
    if ! which timeout; then
        echo "# \`timeout\` not in PATH" >&3

        if [[ $(uname) == "Darwin" ]]; then
            echo "# run \`brew install coreutils\` to install" >&3
        fi

        false
    fi
}

main;