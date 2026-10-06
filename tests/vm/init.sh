#!/bin/busybox sh
# The init of the kernel-verification VM (#12), run as PID 1 from the
# initramfs tests/vm/build-image.sh makes. For each geometry, in tmpfs:
#
#   mkfs.xfs (xfsprogs)                 an empty filesystem
#   vm_verify write                     this crate writes a tree into it
#   xfs_repair -n                       clean
#   the kernel mounts it                every file, mode, owner, link count,
#                                       symlink, device and directory in the
#                                       manifest checks out
#   the kernel changes it               names added to and removed from every
#                                       directory form, a file copied in
#   xfs_repair -n                       clean
#   vm_verify check                     this crate reads the kernel's changes
#   vm_verify write2                    and writes again, over the kernel's
#                                       B+trees and directories
#   xfs_repair -n, the kernel mounts    clean, and the second manifest checks
#
# Prints `VERIFY PASS` or `VERIFY FAIL <why>` on the serial console, then
# powers off. stormcentral's testhost boot watches for those lines.
/bin/busybox mount -t proc proc /proc
/bin/busybox --install -s /bin
export PATH=/bin:/sbin
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mkdir -p /mnt /work
mount -t tmpfs -o size=90% tmpfs /work
echo 1 > /proc/sys/kernel/printk 2>/dev/null

say() { echo "FIO-XFS-VERIFY: $*"; }
fail() {
    say "FAIL: $*"
    dmesg | grep -iE 'xfs|loop' | tail -20 | sed 's/^/FIO-XFS-VERIFY dmesg: /'
    [ -s /work/repair.log ] && grep -vE '^ *- |^Phase|host filesystem|sector size|the image and|^$' /work/repair.log \
        | head -30 | sed 's/^/FIO-XFS-VERIFY repair: /'
    echo "VERIFY FAIL $*"
    sync; poweroff -f; sleep 30
}

say "kernel $(uname -r), $(cat /build-info 2>/dev/null)"
for m in $(cat /modules.order 2>/dev/null); do
    insmod "/modules/$m" || fail "insmod $m"
done
grep -qw xfs /proc/filesystems || fail "the kernel has no xfs"

repair() { # image what
    : > /work/repair.log
    xfs_repair -n -f "$1" >/work/repair.log 2>&1 || fail "xfs_repair -n $2"
}

TAB=$(printf '\t')

# Check a manifest (examples/vm_verify.rs) against what the kernel sees.
check_manifest() { # manifest what
    local n=0
    while IFS="$TAB" read -r kind path a b c d e f; do
        p=/mnt/$path
        case $kind in
        F)
            [ -f "$p" ] && [ ! -L "$p" ] || fail "$2: $path is not a regular file"
            set -- "$1" "$2" $(stat -c '%a %u %g %s %h' "$p")
            [ "$3 $4 $5 $6 $7" = "$a $b $c $d $f" ] || fail "$2: $path is '$3 $4 $5 $6 $7', want '$a $b $c $d $f' (mode uid gid size links)"
            sum=$(md5sum "$p" | cut -d' ' -f1)
            [ "$sum" = "$e" ] || fail "$2: $path contents differ (md5 $sum, want $e)"
            ;;
        D)
            [ -d "$p" ] || fail "$2: $path is not a directory"
            set -- "$1" "$2" $(stat -c '%a %u %g' "$p")
            [ "$3 $4 $5" = "$a $b $c" ] || fail "$2: $path is '$3 $4 $5', want '$a $b $c'"
            cnt=$(ls -A "$p" | wc -l)
            [ "$cnt" -eq "$d" ] || fail "$2: $path lists $cnt names, want $d"
            ;;
        L)
            [ -L "$p" ] || fail "$2: $path is not a symlink"
            [ "$(readlink "$p")" = "$a" ] || fail "$2: $path points elsewhere"
            ;;
        C|B)
            if [ "$kind" = C ]; then [ -c "$p" ]; else [ -b "$p" ]; fi || fail "$2: $path is not a $kind device"
            [ "$(stat -c '%t %T' "$p")" = "$a $b" ] || fail "$2: $path is $(stat -c '%t:%T' "$p"), want $a:$b"
            ;;
        P) [ -p "$p" ] || fail "$2: $path is not a FIFO" ;;
        G) [ -e "$p" ] || [ -L "$p" ] && fail "$2: $path should not exist" ;;
        *) fail "$2: manifest line '$kind'" ;;
        esac
        n=$((n + 1))
    done < "$1"
    say "$2: $n names checked through the kernel"
}

# What the kernel does to the tree: names added to and removed from every
# directory form, and a file copied in (vm_verify check reads them back).
kernel_changes() { # what
    for d in sf block leaf node; do
        i=0
        while [ $i -lt 30 ]; do
            printf 'kernel %s %d\n' $d $i > "/mnt/t/$d/$(printf 'kernel-%03d' $i)" || fail "$1: kernel write in $d"
            i=$((i + 1))
        done
        rm -f /mnt/t/$d/name-*0 || fail "$1: kernel removals in $d"
    done
    cp /bin/busybox /mnt/t/kernel-busybox || fail "$1: kernel copy"
    mkdir -p /mnt/t/kernel-dir/sub && echo x > /mnt/t/kernel-dir/sub/f || fail "$1: kernel mkdir"
}

# name : size : mkfs.xfs options
for case in \
    "512m-default:512M:" \
    "1g-b1024-n8192:1G:-b size=1024 -n size=8192" \
    "1g-i2048:1G:-i size=2048" \
    "2g-agcount16:2G:-d agcount=16" \
    "1g-s4096:1G:-s size=4096" \
    "2g-b16384:2G:-b size=16384" \
    "512m-v5-minimal:512M:-m finobt=0,rmapbt=0,reflink=0,bigtime=0,inobtcount=0 -i sparse=0,nrext64=0"
do
    name=${case%%:*}; rest=${case#*:}; size=${rest%%:*}; opts=${rest#*:}
    img=/work/$name.img
    rm -f "$img"; truncate -s "$size" "$img" || fail "$name: truncate"
    # shellcheck disable=SC2086
    mkfs.xfs -q -f $opts "$img" >/work/err 2>&1 || fail "$name: mkfs.xfs: $(cat /work/err)"

    vm_verify write "$img" /work/m1 2>/work/err || fail "$name: vm_verify write: $(cat /work/err)"
    repair "$img" "$name: as fio-xfs wrote it"
    mount -t xfs -o loop,rw "$img" /mnt 2>/work/err || fail "$name: mount: $(cat /work/err)"
    check_manifest /work/m1 "$name"
    kernel_changes "$name"
    umount /mnt || fail "$name: umount"
    repair "$img" "$name: after the kernel changed it"

    vm_verify check "$img" 2>/work/err || fail "$name: vm_verify check: $(cat /work/err)"
    vm_verify write2 "$img" /work/m2 2>/work/err || fail "$name: vm_verify write2: $(cat /work/err)"
    repair "$img" "$name: after fio-xfs wrote over the kernel's changes"
    mount -t xfs -o loop,ro "$img" /mnt 2>/work/err || fail "$name: remount: $(cat /work/err)"
    check_manifest /work/m2 "$name (second write)"
    [ "$(cat /mnt/t/kernel-dir/sub/f)" = x ] || fail "$name: the kernel's directory after the second write"
    umount /mnt || fail "$name: umount after the second write"

    say "$name ($size $opts): written, xfs_repair -n clean, mounted and checked, changed by the kernel, read back, written again, clean"
    rm -f "$img"
done

echo "VERIFY PASS"
sync; poweroff -f; sleep 30
