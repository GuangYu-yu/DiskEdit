# VM carrier bundle

A bundle is a directory laid out as described in [interop](../docs/en/interop.md#building-a-bundle):

| File | Purpose |
|---|---|
| `manifest.json` | schema, architecture, binary version, artifact paths and SHA-256, tool list |
| `kernel` | guest kernel built from `kernel.config` |
| `initramfs` | newc or gzip-compressed newc archive containing `/init`, the release binary and the tools from `manifest.json` |
| `init` | the init script in this directory; installed as `/init` inside the archive |
| `kernel.config` | kernel options the guest needs |
| `build.sh` | assembles a bundle from a kernel (or a source tree) and a guest binary |

## Assembling a bundle

```bash
# a kernel you already have
BIN=target/x86_64-unknown-linux-musl/release/DiskEdit \
KERNEL=/path/to/bzImage \
tools/vm/build.sh

# or build one: defconfig + kernel.config, then bzImage/Image
BIN=target/x86_64-unknown-linux-musl/release/DiskEdit \
KERNEL_SRC=/path/to/linux-6.6.30 \
tools/vm/build.sh
```

`OUT` defaults to `dist/vm-bundle`. The script installs every tool listed in `manifest.json` plus the two commands `/init` itself needs (`sh` and `cat`), copies the dynamic libraries those binaries need, and writes the initramfs with its own newc writer — `cpio` is not a build dependency, and timestamps are zeroed so the same inputs produce the same archive.

It fails in place when a tool or a build dependency is missing, and it never rewrites the `tools` array: that array is pinned to the tool registry by `fsops::tests::manifest_derived_from_tool_enum`. The script only fills in `arch`, `binary_version` and the three SHA-256 values.

`CONFIG_BLK_DEV_LOOP=y` is part of `kernel.config` on purpose: the filesystem tools reach a partition inside an image file through a loop device, and the guest has no module directory, so loop support has to be built in.

`manifest.json` here is a template. A build replaces `arch`, `binary_version`, the artifact paths' SHA-256 values and `worker.sha256` with the values of the artifacts it produced. The host refuses a manifest whose schema, architecture, version or hashes do not match the running executable and the files on disk, so an unmodified template never starts a guest.

The `tools` array is not edited by hand: `fsops::tests::manifest_derived_from_tool_enum` asserts it equals the tool registry in the order the registry lists it. Add or remove a tool in `src/fsops/ext.rs`, update this array, and rebuild the initramfs.

## Running the carrier tests

Four test binaries cover the carrier, and none of them run by default: they print
`note: not executed <case>: <why>` and return. Set `DISKEDIT_REQUIRE_CAPABILITIES=1`
on a deliberate run so a skip fails instead — otherwise "the guest never booted"
looks exactly like "everything passed".

They need a built bundle, QEMU, and a Linux host. Building the bundle needs the
kernel toolchain (`build-essential flex bison bc libssl-dev libelf-dev xz-utils`) and
one program per tool in `manifest.json`; `qemu-system-x86` is only needed by the tests:

```bash
sudo apt-get install -y --no-install-recommends \
  build-essential flex bison bc libssl-dev libelf-dev xz-utils \
  e2fsprogs ntfs-3g f2fs-tools xfsprogs btrfs-progs dosfstools exfatprogs \
  fatresize gdisk lvm2 util-linux python3 qemu-system-x86

rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
curl -fsSLO https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.6.30.tar.xz
tar -xf linux-6.6.30.tar.xz
BIN=target/x86_64-unknown-linux-musl/release/DiskEdit \
OUT="$PWD/dist/vm-bundle" KERNEL_SRC="$PWD/linux-6.6.30" tools/vm/build.sh
```

The crash matrix and the hand-off case:

```bash
DISKEDIT_TOOL_PATH=/nonexistent CARGO_TARGET_DIR=target/toolpath-vm \
  cargo test --test vm_crash --test vm_handoff --no-run
DISKEDIT_VM_TESTS=1 DISKEDIT_VM_BUNDLE="$PWD/dist/vm-bundle" \
DISKEDIT_REQUIRE_CAPABILITIES=1 DISKEDIT_TOOL_PATH=/nonexistent \
CARGO_TARGET_DIR=target/toolpath-vm \
  cargo test --test vm_crash --test vm_handoff -- --test-threads=1
```

The differential acceptance:

```bash
DISKEDIT_VM_TESTS=1 DISKEDIT_VM_BUNDLE="$PWD/dist/vm-bundle" \
DISKEDIT_REQUIRE_CAPABILITIES=1 \
  cargo test --test vm_diff -- --test-threads=1
```

The cross-executor hand-off — the carrier stops at "table written, record not advanced"
and the host finishes it:

```bash
DISKEDIT_VM_TESTS=1 DISKEDIT_VM_BUNDLE="$PWD/dist/vm-bundle" \
DISKEDIT_REQUIRE_CAPABILITIES=1 \
  cargo test --test vm_interrupt --features test-faults -- --test-threads=1
```

The target there is an image file, whose partition the guest loop-maps. That is not
incidental: the record is written by the host only, and both legs must resolve to the
same record root. Image records live under a per-user root that honours
`DISKEDIT_STATE_DIR` for any user, whereas block-device records live under
`host_state_root`, which ignores that variable unless the process is root — so with a
block device the carrier leg and the native leg would look in different roots. A block
device target shares the same record and decision code but is not covered by this case:
a non-root process cannot write one, and a root one never takes the carrier.

Four things decide whether the job really lands on the carrier:

- `vm_crash` and `vm_handoff` must run a binary compiled with
  `DISKEDIT_TOOL_PATH=/nonexistent`. That path is a compile-time constant, and a host
  that has the filesystem tools executes the job itself — a `PATH` cleared at run time
  changes nothing. `cargo` does not track `option_env!`, so the tool-less build needs
  its own `CARGO_TARGET_DIR`; without it, it replaces the normal binary.
- `vm_diff` needs an account that can run `sudo`: one leg runs as that user (no
  privilege, so the carrier takes the job) and the other through `sudo` (root, so the
  host does it). As root, both legs would be native and there would be nothing to compare.
- `vm_interrupt` wants the same account arrangement, plus a `--features test-faults`
  build: it stops the carrier leg with `DISKEDIT_FAULT=after-table-write`, and without
  that feature the variable is an empty stub, so the leg finishes instead of stopping
  and the case fails looking like a product bug rather than a build choice.
- `--test-threads=1` in both, because a case judges "is a QEMU left over on this
  machine" by process count, and each case kills its own carrier.

`accel=kvm:tcg` means a host without `/dev/kvm` still runs, only slower.

## Licensing

The guest kernel and the filesystem tools are separate programs distributed under their own licenses (the Linux kernel under GPL-2.0-only, e2fsprogs under GPL-2.0/LGPL, and so on). A bundle that redistributes them must ship their license texts and make the corresponding source available in the manner each license requires. This repository does not include kernel or tool binaries; the bundle is assembled by whoever builds it, and that builder carries the distribution obligations. The DiskEdit binary itself is placed in the initramfs unchanged.
