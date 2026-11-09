# Interop: other image formats, remote targets, sparse images

None of the workflows below are implemented by this tool, and none are exercised by its test suite. They are recipes for doing the work outside the tool and then pointing it at the result. The commands come from the other projects' documentation; the parts worth reading are the seams, because they say what this tool will and will not notice.

## qcow2 / vmdk / vdi images

```bash
qemu-nbd -c /dev/nbd0 disk.qcow2
partprobe /dev/nbd0
# run this tool against /dev/nbd0
qemu-nbd -d /dev/nbd0
```

- Partition nodes (`/dev/nbd0p1`) exist only after the kernel scans the device; after a table write, run `partprobe /dev/nbd0` again before using a partition node.
- `qemu-nbd -P` / `--partition` is not a scanning option: it exports a single partition and is deprecated in favour of `--image-opts`. Partition discovery relies on `partprobe`.
- Capacity and sector size come from nbd, not from the qcow2/vmdk/vdi metadata; there is nothing to pass as `--sector-size`.
- Detach must succeed for the operation to count as finished. If `qemu-nbd -d` fails, a block device or connection may remain; if this tool has already cleaned up its record, no record describes the state any more — check `lsblk` and `/sys/block/nbd*` before reusing the name.

## Exporting a disk as qcow2

```bash
qemu-img convert -f raw -O qcow2 disk.img disk.qcow2
```

- Give `-f raw` explicitly; the source format is not to be probed.
- `-n` (`--no-create`) skips creating the target and requires it to exist untouched; `-C` (`--compress`) compresses the output and is normally only meaningful with qcow2. They control different things — pick the one matching the intent instead of relying on version defaults for an existing target.

## Sparse and preallocated images

```bash
qemu-img create -f qcow2 -o preallocation=metadata disk.qcow2 20G
qemu-img convert -f raw -O qcow2 disk.img disk.qcow2
```

- `copy` is a partition-level copy and produces no whole-disk file, so whole-disk sparseness and preallocation stay outside the tool.
- When a raw image is written by other means, whether unallocated zeroes are materialised is a question about the recovery record's size, not about the image's sparseness policy.

## Remote images and remote disks

```bash
rbd map pool/image
nbd-client <host> <port> /dev/nbd0
```

- `fsync` forwarding through NBD is credible: the protocol has an explicit flush, and `qemu-nbd` writes through by default. That is not a statement about every server implementation.
- `flock` has no documented guarantee over rbd or nbd. Block-level transport does not imply local-block-device lock semantics; do not assume the advisory lock described in [internals.md](internals.md#concurrency-boundary) behaves the same way here.
- `flock` over sshfs or NFS must be checked in the actual deployment before relying on it.

## Forced NTFS resize on a damaged volume

There is no table-only mode: `resize-part` always runs the filesystem step that matches the size change (grow for a larger end, shrink for a smaller one), so a manual `ntfsresize --force` followed by `resize-part` means `ntfsresize` runs again. This tool invokes it as `ntfsresize -f -f` (grow, after a `--no-action` rehearsal) and `ntfsresize -f -f -s <bytes>` (shrink), so the volume's dirty flag alone does not block that step.

Whether the combined sequence is safe has not been run; treat it as unverified. After any forced `ntfsresize`, the volume is marked for a consistency check, which Windows clears with `chkdsk`.