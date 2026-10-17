#!/usr/bin/env bash
# Linux x86 nested restore qualification, also runnable outside GitHub Actions.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=${NESTED_TEST_DIR:?set NESTED_TEST_DIR to a dedicated output directory}
firmware=${NESTED_FIRMWARE_DIR:?set NESTED_FIRMWARE_DIR to a libkrunfw checkout}
mkdir -p "$work"
work=$(cd "$work" && pwd)
firmware=$(cd "$firmware" && pwd)
export CARGO_TARGET_DIR="$work/target"

case "${1:-}" in
  build)
    cargo fmt --manifest-path "$repo/tests/nested-restore/guest/Cargo.toml" -- --check
    cargo clippy --locked --target x86_64-unknown-linux-musl --manifest-path "$repo/tests/nested-restore/guest/Cargo.toml" -- -D warnings
    cargo build --locked --release --target x86_64-unknown-linux-musl --manifest-path "$repo/tests/nested-restore/guest/Cargo.toml"
    cargo build --locked --manifest-path "$repo/tests/nested-restore/Cargo.toml"
    cargo test --locked --manifest-path "$repo/tests/nested-restore/Cargo.toml"
    # Linux's firmware mmap path needs the fixture embedded in the kernel.
    # A gen_init_cpio list creates /dev/console without root or mknod privileges.
    cat > "$work/initramfs.list" <<LIST
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
file /init $CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/nested-counter-guest 0755 0 0
LIST
    kernel=$(sed -n 's/^KERNEL_VERSION = //p' "$firmware/Makefile")
    [[ $kernel =~ ^linux-[0-9]+\.[0-9]+\.[0-9]+$ ]]
    make -C "$firmware" "$kernel"
    "$firmware/$kernel/scripts/config" --file "$firmware/$kernel/.config" --set-str INITRAMFS_SOURCE "$work/initramfs.list"
    make -C "$firmware/$kernel" olddefconfig
    (cd "$firmware" && make -j "${NESTED_BUILD_JOBS:-4}")
    ;;
  run)
    test -r /dev/kvm && test -w /dev/kvm
    # No skip or admission bypass: the guest must actually create and run L2.
    version=$(sed -n 's/^FULL_VERSION = //p' "$firmware/Makefile")
    export KRUNFW_PATH="$firmware/libkrunfw.so.$version"
    export TEST_INITRAMFS="$work/unused.cpio"
    export TEST_PROGRESS_TIMEOUT_SECONDS=120
    touch "$TEST_INITRAMFS"
    harness="$CARGO_TARGET_DIR/debug/nested-restore-live"
    for TEST_SPLIT_IRQCHIP in false true; do
      export TEST_SPLIT_IRQCHIP
      profile="$work/$TEST_SPLIT_IRQCHIP"
      mkdir -p "$profile"
      export TEST_SNAPSHOT="$profile/snapshot"
      run_probe() {
        local name=$1 mode=$2
        export TEST_ROOT="$profile/$name"
        mkdir -p "$TEST_ROOT"
        timeout --kill-after=10s 180s "$harness" "$mode" 2>&1 | tee "$profile/$name.log"
        grep -qx PASS "$profile/$name.log"
      }
      run_probe capture capture
      run_probe restore restore
      run_probe fork restore
      TEST_OUTPUT_SNAPSHOT="$profile/second" run_probe recapture restore
      TEST_SNAPSHOT="$profile/second" run_probe second-restore restore
    done
    ;;
  *) echo 'usage: ci.sh build|run' >&2; exit 2 ;;
esac
