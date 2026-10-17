#!/bin/bash
# Build on Linux matching the guest architecture. Requires cc, QEMU, cpio, and e2fsprogs.
set -euo pipefail
source_dir=$(cd -- "$(dirname -- "$0")" && pwd)
output=$(realpath -m -- "${1:?output directory required}")
kernel=$(realpath -- "${2:?inner Linux kernel image required}")
mkdir -p "$output"
# Refuse reuse so a prior disk or stale initramfs cannot contaminate a run.
mkdir "$output/inner" "$output/parent"
inner="$output/inner"
parent="$output/parent"
mkdir -p "$inner/dev" "$inner/proc" "$inner/data" "$parent/dev" "$parent/bin"
cc -static -Wall -Wextra -Werror "$source_dir/inner.c" -o "$inner/init"
sudo mknod "$inner/dev/console" c 5 1
sudo mknod "$parent/dev/console" c 5 1
(cd "$inner" && find . -print0 | cpio --null -o --format=newc --owner=0:0) > "$parent/inner-initramfs"
cp "$kernel" "$parent/inner-kernel"
cp "$source_dir/parent-init.sh" "$parent/init"
chmod +x "$parent/init"
if [ "${NESTED_BUSY:-0}" = 1 ]; then touch "$parent/nested-busy"; fi
cp /bin/busybox "$parent/bin/busybox"
case "$(uname -m)" in
    x86_64) qemu=$(command -v qemu-system-x86_64) ;;
    aarch64) qemu=$(command -v qemu-system-aarch64) ;;
    *) echo 'unsupported build architecture' >&2; exit 1 ;;
esac
cp "$qemu" "$parent/qemu-system"
# ldd prints transitive dependencies as well as the ELF interpreter.
while IFS= read -r library; do
    cp --parents -L "$library" "$parent"
done < <(ldd "$qemu" | awk '{for (i=1;i<=NF;i++) if ($i ~ /^\//) print $i}')
for firmware in /usr/share/qemu /usr/share/seabios; do
    if [ -d "$firmware" ]; then cp --parents -a "$firmware" "$parent"; fi
done
truncate -s 32M "$parent/state.img"
mkfs.ext4 -q -F "$parent/state.img"
(cd "$parent" && find . -print0 | cpio --null -o --format=newc --owner=0:0) > "$output/parent.cpio"
printf 'outer initramfs: %s/parent.cpio\n' "$output"
