#!/bin/busybox sh
set -eu
/bin/busybox mount -t devtmpfs devtmpfs /dev
case "$(/bin/busybox uname -m)" in
    x86_64)
        machine=q35,kernel_irqchip=on
        console=ttyS0
        block=virtio-blk-pci
        ;;
    aarch64)
        machine=virt,gic-version=3
        console=ttyAMA0
        block=virtio-blk-device
        ;;
    *) echo 'FAIL: unsupported inner OS architecture'; exit 1 ;;
esac
extra=""
if [ -f /nested-busy ]; then extra="NESTED_BUSY=1"; fi
# -accel kvm is mandatory: software emulation cannot qualify nested restore.
exec /qemu-system -accel kvm -machine "$machine" -cpu host -smp 2 -m 256 \
    -nodefaults -no-user-config -display none -monitor none -serial stdio \
    -kernel /inner-kernel -initrd /inner-initramfs \
    -append "console=$console rdinit=/init panic=-1 quiet $extra" \
    -drive file=/state.img,format=raw,if=none,id=state,cache=writeback \
    -device "$block",drive=state
