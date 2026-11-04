#!/bin/sh
# Build the guest files of the VM backend: kernel, rootfs.img and assets.toml.
#
#   packaging/vm/build.sh [OUTPUT_DIR]
#
# OUTPUT_DIR defaults to build/vm, which git ignores. Install the result on a
# Mac with `paddock vm setup --from OUTPUT_DIR`.
#
# The script runs on Linux without root. It needs curl, tar, gzip, xz, od,
# sha256sum, mke2fs and debugfs (e2fsprogs). The same inputs give the same
# sha256 values. The inputs are:
# - the arm64 Linux kernel of a pinned Kata Containers release
# - a pinned Alpine minirootfs and pinned Alpine packages
# - packaging/vm/agent-init, installed as /sbin/paddock-init
# - the guest paddock binary for aarch64-unknown-linux-musl. Set PADDOCK_BIN
#   to use a binary that exists. Else the script builds it from this
#   repository with cargo, clang, llvm-ar and rust-lld (see build_guest).
#
# VM_CACHE_DIR keeps the downloads (default: build/vm-cache).
# SOURCE_DATE_EPOCH sets the time of every file (default: a fixed date).
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUTPUT_DIR="${1:-$ROOT/build/vm}"
CACHE_DIR="${VM_CACHE_DIR:-$ROOT/build/vm-cache}"
EPOCH="${SOURCE_DATE_EPOCH:-1767225600}"
export LC_ALL=C TZ=UTC
umask 022

mkdir -p "$OUTPUT_DIR" "$CACHE_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/paddock-vm-work.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT INT TERM

# The kernel: Kata Containers ships an uncompressed arm64 Image with the
# virtio, vsock and virtiofs drivers built in. Its name says vmlinux, but
# the file is an Image (see check_kernel).
KATA_VERSION="3.17.0"
KATA_SHA256="647c7612e6edf789d5e14698c48c99d8bac15ad139ffaa1c8bb7d229f748d181"
KATA_URL="https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/kata-static-${KATA_VERSION}-arm64.tar.xz"
KERNEL_VERSION="6.12.28-153"
KATA_KERNEL="./opt/kata/share/kata-containers/vmlinux-${KERNEL_VERSION}"
KATA_CONFIG="./opt/kata/share/kata-containers/config-${KERNEL_VERSION}"

ALPINE="https://dl-cdn.alpinelinux.org/alpine/v3.21"
MINIROOTFS_VERSION="3.21.9"
MINIROOTFS_SHA256="a389940ee42579ead202fb91607fb25698a58a3a5b55c15b4603e5d39cfa4492"
MINIROOTFS_URL="$ALPINE/releases/aarch64/alpine-minirootfs-${MINIROOTFS_VERSION}-aarch64.tar.gz"

# The packages of the root file system: path in the repository and sha256.
PACKAGES="
main/ncurses-terminfo-base-6.5_p20241006-r3.apk:a86fb76ba2ac494ed3a550a5c42d032c486d942966bceae38f6ce95eaa384550
main/libncursesw-6.5_p20241006-r3.apk:c1a5a3a552ad4d44c94454555f38de71b151dc2d608ea2246d92b3bd845b7f3a
main/readline-8.2.13-r0.apk:7dad49f83ecbcfa00c5c7df044a5566b928ac1995651cfff219e1df1c2b93871
main/bash-5.2.37-r0.apk:876b5e7c50bd13bd7537038c16d5598cfad249aba1cca1086a1f7e3e48b0440f
main/libcap2-2.78-r0.apk:7ca70d1d6d2822b108efd9c5909be8d7ffa86031c9dea1c775dd19439d812f52
main/bubblewrap-0.12.0-r0.apk:c2a06b7466bab1a602756dc4c18c12f7df98790b7c74d743bd36b3ee33a2df60
community/passt-2025.01.21-r0.apk:7d36a1541562dfbd19af32712e5b3cd7c3f951e007b717e4b131e8c93086d17e
main/fuse-common-3.16.2-r1.apk:c1f74ce6111e0706310d86f5508dfa264c26e7570efbf99fb5f714bd012f2823
main/fuse3-libs-3.16.2-r1.apk:9b31ec165255b18fff17d8b3a368e396f970dab34c3036c2478a9c4fc69fb128
main/fuse3-3.16.2-r1.apk:35b833b23d7bb465c731891538d7aee330f22d8f3c937c6d5547b64537fdeb17
main/ca-certificates-bundle-20261020-r0.apk:2e421ec229ec0956a5617dd3c9a1dc0129a0cb3e15ebbf452f9379cf6f622450
main/ca-certificates-20261020-r0.apk:1819100af96003e62cf416f4d9b1e5f0ab31661bc0088584d54bcdb57901e861
main/brotli-libs-1.1.1-r0.apk:5dd59a2589d1abbffb65581b10e1fb9237b4849c5665a65acf9451aa983c465a
main/c-ares-1.34.9-r0.apk:8085db68260563d005d936e1c35c7b943109a6392d1e5ee2cedbdacc3f5eb46c
main/libunistring-1.2-r0.apk:6c80d48d4458f4344fa34a28283a5c6147404fb24398b861e981aa5dd9df79da
main/libidn2-2.3.7-r0.apk:189be98c9ce9ec5bd6709ea489bd236f5a6c971bf70e074f9a3ea24585f9d5ec
main/nghttp2-libs-1.69.0-r0.apk:e0546b86eff0bbde2124ea05affa19bed76ddd1382d0ba0bb181cc75b8999eea
main/libpsl-0.21.5-r3.apk:2e9d33bc57bc4db4ba78993ce3e684b1fcef182d86812792885751b47cb9ccd4
main/zstd-libs-1.5.6-r2.apk:2a15740cf272bf3babb438b675f0f894da0b4fa07b42dd86d0d07aee69378251
main/libcurl-8.14.1-r3.apk:a1f8514eb8d591730e871fe7356ec3266409a7f4a3f5745152e7f2b9dea8aea9
main/libexpat-2.10.0-r0.apk:2352b71e3e94d18a97ca28de2e2f36fcf2cb259344377cf8f65ac24406051302
main/pcre2-10.43-r1.apk:525f1b4c7560ef6561b0a43e6a160bcda368254c58595fa43eff672b01031be1
main/git-2.47.3-r0.apk:6449c9bd31b7e47221a54f68f6fddb4122351f65b9a1b371b737872ef5a1b68b
main/libeconf-0.6.3-r0.apk:db25f8abaf1ec61f5dcc0ff9b55271a3f6e43037e430b0c126ac000483e17dad
main/libuuid-2.40.4-r1.apk:d392ac96027cbe5ca9bc270d2aa4e99747b0eae3cf65a0087f2c6f13a7382702
main/libblkid-2.40.4-r1.apk:61339f8737b05062f662cfc30c65cb34f10ccc53573846653551c66156c4aa0b
main/libcom_err-1.47.1-r1.apk:0798fedbc002cada8e74a7cb4cdae885e5f0c1b5fd59b15af5b2903b95d30153
main/e2fsprogs-libs-1.47.1-r1.apk:c817ddefbfa19245cce0c62820a1eb50c771f4999232d0a1c9e57521a58508d3
main/e2fsprogs-1.47.1-r1.apk:c28dddb51a40a91820a9f0dcd32f19abf23c8256543d7afc4f87363b28885a66
"

# The C headers and the libc of the cross build of the guest binary.
SYSROOT_PACKAGES="
main/musl-1.2.5-r12.apk:f37e73d41880bbc653b771c7d8962c8739bff773503f5155c14fab1bd7c4d52f
main/musl-dev-1.2.5-r12.apk:638945ad611696b7be27f46c4839a3f487ccd1479b648dfe5e2bc87c4710d587
"

# The kernel options that the guest needs, built in (=y).
KERNEL_OPTIONS="
VIRTIO VIRTIO_PCI VIRTIO_BLK VIRTIO_CONSOLE HW_RANDOM_VIRTIO
VSOCKETS VIRTIO_VSOCKETS FUSE_FS VIRTIO_FS
EXT4_FS OVERLAY_FS TMPFS DEVTMPFS UNIX
NAMESPACES USER_NS PID_NS NET_NS UTS_NS IPC_NS
CGROUPS MEMCG CGROUP_PIDS SECCOMP_FILTER
"

TARGET="aarch64-unknown-linux-musl"

say() { printf '%s\n' "$*" >&2; }
die() { say "build.sh: $*"; exit 1; }

# Download a URL into the cache once, and check its sha256. POSIX sh has
# no local variables: each function uses its own names.
fetch() {
  url="$1" dest="$2" want="$3"
  if [ ! -f "$dest" ]; then
    say "Fetching $url"
    curl -fsSL --retry 3 -o "$dest.tmp" "$url"
    mv "$dest.tmp" "$dest"
  fi
  got=$(sha256sum "$dest" | cut -d' ' -f1)
  [ "$got" = "$want" ] || die "$dest has sha256 $got, expected $want"
}

# Unpack the Alpine packages of a list into a folder. An apk file is a
# gzip tar stream. Its .PKGINFO and .SIGN.* files are not part of the
# system.
unpack_packages() {
  pkg_list="$1" pkg_dest="$2"
  for item in $pkg_list; do
    rel="${item%%:*}"
    pkg_file="$CACHE_DIR/${rel#*/}"
    fetch "$ALPINE/${rel%/*}/aarch64/${rel#*/}" "$pkg_file" "${item##*:}"
    tar -xzpf "$pkg_file" -C "$pkg_dest" --warning=no-unknown-keyword 2>/dev/null \
      || tar -xzpf "$pkg_file" -C "$pkg_dest"
  done
  rm -f "$pkg_dest"/.PKGINFO "$pkg_dest"/.SIGN.* "$pkg_dest"/.pre-* "$pkg_dest"/.post-* "$pkg_dest"/.trigger
}

# The kernel must be an arm64 Linux Image: VZLinuxBootLoader does not boot
# an ELF vmlinux. The arm64 boot header has the magic "ARM\x64" at byte 56.
check_kernel() {
  kernel="$1" config="$2"
  magic=$(od -A n -t x1 -j 56 -N 4 "$kernel" | tr -d ' \n')
  [ "$magic" = "41524d64" ] || die "$kernel is not an arm64 Linux Image (magic $magic at byte 56)"
  for opt in $KERNEL_OPTIONS; do
    grep -qx "CONFIG_$opt=y" "$config" || die "the kernel has no CONFIG_$opt=y"
  done
}

# Build the guest paddock binary from this repository. The C code of the
# dependencies (ring, sqlite) builds with clang against the Alpine musl
# headers. rust-lld links the binary statically. The path prefixes are
# remapped, so the binary does not depend on the folder of the checkout.
# It needs: rustup target add aarch64-unknown-linux-musl, clang, llvm-ar.
build_guest() {
  sysroot="$WORKDIR/sysroot"
  mkdir -p "$sysroot"
  unpack_packages "$SYSROOT_PACKAGES" "$sysroot"
  cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  (
    cd "$ROOT"
    CC_aarch64_unknown_linux_musl=clang \
    CFLAGS_aarch64_unknown_linux_musl="--target=aarch64-linux-musl --sysroot=$sysroot" \
    AR_aarch64_unknown_linux_musl=llvm-ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="--remap-path-prefix=$ROOT=/paddock --remap-path-prefix=$cargo_home=/cargo" \
      "${CARGO:-cargo}" build --release --locked --target "$TARGET" \
      --target-dir "$ROOT/build/vm-target" --bin paddock >&2
  )
  printf '%s\n' "$ROOT/build/vm-target/$TARGET/release/paddock"
}

# 1. The kernel.
KATA_TAR="$CACHE_DIR/kata-static-${KATA_VERSION}-arm64.tar.xz"
fetch "$KATA_URL" "$KATA_TAR" "$KATA_SHA256"
tar -xJf "$KATA_TAR" -C "$WORKDIR" "$KATA_KERNEL" "$KATA_CONFIG"
check_kernel "$WORKDIR/$KATA_KERNEL" "$WORKDIR/$KATA_CONFIG"
cp "$WORKDIR/$KATA_KERNEL" "$OUTPUT_DIR/kernel"
chmod 644 "$OUTPUT_DIR/kernel"

# 2. The guest binary.
if [ -n "${PADDOCK_BIN:-}" ]; then
  [ -f "$PADDOCK_BIN" ] || die "PADDOCK_BIN $PADDOCK_BIN is not a file"
  GUEST_BIN="$PADDOCK_BIN"
else
  GUEST_BIN=$(build_guest)
fi
# An ELF file for aarch64 (machine 183 at byte 18) and no interpreter.
[ "$(od -A n -t x1 -j 18 -N 2 "$GUEST_BIN" | tr -d ' \n')" = "b700" ] \
  || die "$GUEST_BIN is not an aarch64 binary"
! grep -q "ld-linux\|ld-musl" "$GUEST_BIN" || die "$GUEST_BIN is not a static binary"

# 3. The root file system.
ROOTFS_DIR="$WORKDIR/rootfs"
mkdir -p "$ROOTFS_DIR"
MINIROOTFS_TAR="$CACHE_DIR/alpine-minirootfs-${MINIROOTFS_VERSION}-aarch64.tar.gz"
fetch "$MINIROOTFS_URL" "$MINIROOTFS_TAR" "$MINIROOTFS_SHA256"
tar -xzpf "$MINIROOTFS_TAR" -C "$ROOTFS_DIR"
unpack_packages "$PACKAGES" "$ROOTFS_DIR"
# Merged /usr, as on the hosts that paddock knows: /bin, /sbin and /lib are
# links into /usr. The sandbox of a run links /bin to usr/bin, so a harness
# command such as /bin/sh must live below /usr.
for d in bin sbin lib; do
  if [ -d "$ROOTFS_DIR/$d" ] && [ ! -L "$ROOTFS_DIR/$d" ]; then
    mkdir -p "$ROOTFS_DIR/usr/$d"
    for f in "$ROOTFS_DIR/$d"/* "$ROOTFS_DIR/$d"/.[!.]*; do
      [ -e "$f" ] || [ -L "$f" ] || continue
      n=$(basename "$f")
      [ -e "$ROOTFS_DIR/usr/$d/$n" ] || [ -L "$ROOTFS_DIR/usr/$d/$n" ] || mv "$f" "$ROOTFS_DIR/usr/$d/$n"
    done
    rm -rf "${ROOTFS_DIR:?}/$d"
    ln -s "usr/$d" "$ROOTFS_DIR/$d"
  fi
done

install -m 755 "$GUEST_BIN" "$ROOTFS_DIR/usr/bin/paddock"
install -m 755 "$ROOT/packaging/vm/agent-init" "$ROOTFS_DIR/sbin/paddock-init"
# paddock-vm passes init=/sbin/paddock-init. /sbin/init starts it too, so
# the image also boots with another kernel command line. No getty runs.
ln -sf paddock-init "$ROOTFS_DIR/sbin/init"
rm -f "$ROOTFS_DIR/etc/inittab"
# The mount point of the writable disk of the guest agent.
mkdir -p "$ROOTFS_DIR/var/lib/paddock"
# The first folders of macOS project paths. The root file system is
# read-only: the guest agent puts a tmpfs on one of these empty folders and
# mounts the project at its host path below it.
mkdir -p "$ROOTFS_DIR/Users" "$ROOTFS_DIR/private" "$ROOTFS_DIR/Volumes"
printf 'paddock\n' > "$ROOTFS_DIR/etc/hostname"
# The guest user of the runs (uid 1000).
printf 'paddock:x:1000:1000:paddock:/var/lib/paddock/home:/bin/sh\n' >> "$ROOTFS_DIR/etc/passwd"
printf 'paddock:x:1000:\n' >> "$ROOTFS_DIR/etc/group"
printf 'paddock:!::0:::::\n' >> "$ROOTFS_DIR/etc/shadow"

# Every file gets the same time. mke2fs reads the files in name order.
find "$ROOTFS_DIR" -exec touch -h -d "@$EPOCH" {} +

# 4. The raw ext4 image: a fixed UUID, hash seed and time.
IMG="$OUTPUT_DIR/rootfs.img"
rm -f "$IMG"
E2FSPROGS_FAKE_TIME="$EPOCH" mke2fs -q -F -t ext4 -b 4096 -L paddock-root \
  -U 5b1c5e3a-4d8b-4c34-9b0f-7061646f636b \
  -E hash_seed=5b1c5e3a-4d8b-4c34-9b0f-7061646f636b,root_owner=0:0 \
  -O ^has_journal -m 0 -N 8192 -I 256 \
  -d "$ROOTFS_DIR" "$IMG" 64M
# A build without root extracts the files with the user's own ids. Every
# file of the image belongs to root. mke2fs copies the change time of each
# file, which touch cannot set: it gets the fixed time too.
(
  cd "$ROOTFS_DIR"
  find . | sort | while IFS= read -r p; do
    p="${p#.}"
    p="${p:-/}"
    printf 'sif "%s" uid 0\nsif "%s" gid 0\nsif "%s" ctime @%s\n' "$p" "$p" "$p" "$EPOCH"
  done
) > "$WORKDIR/owners.debugfs"
E2FSPROGS_FAKE_TIME="$EPOCH" debugfs -w -f "$WORKDIR/owners.debugfs" "$IMG" >/dev/null 2>"$WORKDIR/debugfs.log" \
  || { cat "$WORKDIR/debugfs.log" >&2; die "debugfs failed"; }
if grep -v '^debugfs' "$WORKDIR/debugfs.log" | grep -q .; then
  cat "$WORKDIR/debugfs.log" >&2
  die "debugfs reported errors"
fi
chmod 644 "$IMG"

# 5. assets.toml. The URLs follow the pattern of the GitHub releases. The
# files are there only after a release attaches them.
REPO="${GITHUB_REPOSITORY:-harnessix/paddock}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
entry() {
  name="$1" file="$2" version="$3"
  cat <<EOF
[[file]]
name = "$name"
url = "https://github.com/$REPO/releases/download/v$VERSION/$name"
sha256 = "$(sha256sum "$file" | cut -d' ' -f1)"
size = $(wc -c < "$file" | tr -d ' ')
version = "$version"

EOF
}
{
  printf '# The guest files of the VM backend. packaging/vm/build.sh writes this list.\n'
  printf 'version = "%s"\n\n' "$VERSION"
  entry kernel "$OUTPUT_DIR/kernel" "$KERNEL_VERSION"
  entry rootfs.img "$IMG" "alpine-$MINIROOTFS_VERSION-paddock-$VERSION"
} > "$OUTPUT_DIR/assets.toml"

say "Built the VM files in $OUTPUT_DIR:"
(cd "$OUTPUT_DIR" && sha256sum kernel rootfs.img && wc -c kernel rootfs.img | sed '$d') >&2
