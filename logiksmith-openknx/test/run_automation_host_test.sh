#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cxx=${CXX:-c++}
build_dir=${TMPDIR:-/tmp}/logiksmith-openknx-host-test
mkdir -p "$build_dir"

"$cxx" -std=c++11 -Wall -Wextra -Werror \
    -I"$root/include" \
    "$root/src/automation_store.cpp" \
    "$root/src/management_server.cpp" \
    "$root/test/automation_host_test.cpp" \
    -o "$build_dir/automation_host_test"
"$build_dir/automation_host_test"
