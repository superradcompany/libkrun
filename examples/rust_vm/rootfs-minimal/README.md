# rootfs-minimal

This is the minimal root filesystem used by the `rust_vm` example by default.

It intentionally contains only the files needed to run `/bin/echo` through `init.krun`: BusyBox copied to the applet names the example executes, plus the matching musl dynamic loader for each supported guest architecture.

The applet files are regular files instead of symlinks so the example works from ordinary Windows Git checkouts, where Unix symlinks can otherwise materialize as small text files containing the link target.

For fuller manual testing, set `KRUN_ROOTFS_PATH` to another Linux rootfs directory.
