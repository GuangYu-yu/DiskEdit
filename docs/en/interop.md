# Interop: other image formats, remote targets, sparse images

None of the workflows below are implemented by this tool, and none are exercised by its test suite. They are recipes for doing the work outside the tool and then pointing it at the result. The tool itself works on raw files and block devices; another image format is made visible by the host, and the tool never reads or writes the format itself. The commands come from the other projects' documentation; the parts worth reading are the seams, because they say what this tool will and will not notice.

## qcow2 / vmdk / vdi images

The host can make the image appear as a raw file, and the tool then opens it like any other image. QEMU exports a block node over FUSE (QEMU 6.0 or newer, with `/dev/fuse`); `driver=` names the format:

```bash
qemu-storage-daemon \
 --blockdev driver=file,node-name=file,filename=disk.qcow2 \
 --blockdev driver=qcow2,node-name=qcow2,file=file \
 --export type=fuse,id=export,node-name=qcow2,mountpoint=disk.raw,writable=on
# run this tool against disk.raw
```

- Prerequisites: QEMU 6.0 or newer built with the FUSE export, and a usable `/dev/fuse`. The mount point must be a regular file; it is shadowed while the export is active and its contents are not changed. Open it after the mount exists — a handle opened earlier keeps showing the file's own contents.
- The export runs without root: the daemon and the tool need the image file and `/dev/fuse`, nothing else.
- Add `growable=on` when a command extends the image: writes past the current end grow the block node. Without it, such writes fail.
- The image format needs a writable block driver. qcow2 is writable; vmdk and vdi depend on the build and were not tried here.
- The FUSE view is a single file with no partition nodes; this tool addresses partitions by offset, so none are needed.
- Filesystem-level commands still need root: they reach the partition through `losetup`, exactly as on a raw image.
- For this tool an image's identity is the file's `(st_dev, st_ino)` and size. A FUSE mount re-created at the same path is a different inode, so a record written during one mount is not found after the mount is re-created: if an operation crashes, restore the image from its backup rather than expecting a record. Passing the raw view to a guest as a virtio-blk disk with a fixed serial (see the rootless section) replaces that identity with a stable one.
- A qcow2 image may reference a backing file. The overlay records that path, every layer must be present and reachable, and none may be modified by another process while the export is active; `qemu-img info --backing-chain disk.qcow2` lists the chain before you start.
- Do not modify the image from another process, VM or image tool while the export is active. QEMU's warning is explicit: modifying an image in use can destroy it, and querying a changing image can return inconsistent state.
- Container-format parsing (`qcow2`, `vmdk`, `vdi`) happens in the host-side QEMU process. The rootless recipe isolates the filesystem layer, not this parser; an image from an untrusted source is host-side input.
- A kernel nbd device (`qemu-nbd -c /dev/nbd0`) also presents the image as a block device, but that device carries no identity that survives re-creation: every command that writes a table needs `--identity NAME`, and resuming or releasing the record needs the same name. Without a name the write commands are refused (exit 6). The name is an assertion supplied by the caller, not a fact read from the device. Read-only commands (`inspect`, `records list`, `records verify`) accept the same name to locate and verify the record.
- Partition nodes (`/dev/nbd0p1`) exist only after the kernel scans the device; after a change made outside this tool, run `partprobe /dev/nbd0` again before using a partition node.
- Capacity and sector size come from nbd, not from the qcow2/vmdk/vdi metadata; there is nothing to pass as `--sector-size`.
- Detach must succeed before the mapping is considered finished. If `qemu-nbd -d` fails, a block device or connection may remain; check `lsblk` and `/sys/block/nbd*` before reusing the name.

## Exporting a disk as qcow2

```bash
qemu-img convert -f raw -O qcow2 disk.img disk.qcow2
```

- `-f raw` states the source format instead of letting it be probed.
- `-n` skips creating the target volume; the target must already exist.
- `-c` compresses the output; it applies to qcow/qcow2 and to vmdk with `streamOptimized`.
- `-C` is copy offloading, not compression: it can speed up conversion from remote storage and may leave the target fully allocated.

## Sparse and preallocated images

```bash
qemu-img create -f qcow2 -o preallocation=metadata disk.qcow2 20G
qemu-img convert -f raw -O qcow2 disk.img disk.qcow2
```

- `preallocation=metadata` reserves qcow2 metadata only. The qcow2 modes are `off`, `metadata`, `falloc` and `full`; they differ in what the host allocates before the guest writes.
- Sparse allocation changes host storage consumption, not the guest-visible contents of the disk.
- The recovery record's size follows the byte ranges the tool records, not the image's allocated size; do not infer one from the other.
- `copy` is a partition-level copy and produces no whole-disk file, so whole-image allocation policy stays outside the tool.

## Remote images and remote disks

```bash
rbd map pool/image
nbd-client <host> <port> /dev/nbd0
```

- `rbd` and `nbd` devices have no identity this tool can establish from the device interface: no WWN, serial, or stable name it can re-derive. Every command that writes a table needs `--identity NAME`; resuming or releasing the record needs the same name, and without one the write commands are refused (exit 6). This is the tool's identity model, not a property of NBD or RBD: both protocols can name an export, but the tool cannot verify that name against the mapped device. Read-only commands (`inspect`, `records list`, `records verify`) accept the same name to locate and verify the record.
- NBD defines an explicit FLUSH, but durability depends on the client sending it, the server forwarding it, and the backing storage honoring it. `qemu-nbd` defaults to `--cache=writeback`, which is not write-through; a successful `fsync` is not proof of durability without checking the whole path.
- A local `flock` gives no distributed exclusion: two hosts, or two aliases of the same export, take independent locks. Remote targets need a lock whose scope matches the writers.
- `flock` over sshfs or NFS must be checked in the actual deployment before relying on it.

## Rootless use inside a QEMU microvm

Filesystem-layer commands reach the partition through `losetup`, which needs root. Where root is not available, run the same binary inside a guest and let the guest's root do the work; the binary itself is unchanged. QEMU's `microvm` machine is the smallest x86 option: no PCI, no ACPI, up to eight virtio-mmio devices, and it boots only with `-kernel`/`-initrd`.

- No other x86 machine is smaller. `pc` and `q35` add PCI, ACPI and firmware; `isapc` has no virtio-mmio; `nitro-enclave`, `xenfv` and `xenpv` target specific hosts. On arm64 the machine is `-M virt`: it also provides PCIe, so virtio-pci is available as well, but the virtio-mmio devices below work there too.
- The image is attached as a virtio-blk disk. Its serial is the only stable identity the guest can see, so pass a host-derived key. The serial is limited to 20 characters and longer values are silently truncated: derive it from the image rather than from its path, keep distinct images on distinct keys, read the attribute inside the guest instead of assuming a sysfs path, and reject keys that are too long or collide before starting QEMU rather than depending on truncation.
- The state directory goes through virtio-9p. `-virtfs` on its own creates a PCI device that microvm cannot use, so name the device explicitly. The same explicit form works on both architectures. `security_model=none` maps no ownership: files are created with the QEMU process's credentials, so check the resulting ownership in the deployment.
- Keep the ISA serial port: `-serial stdio` attaches the console to it. `isa-serial=off` is only for a guest whose console comes from a virtio console instead.
- `x-option-roms=off` is safe to pass. `pit=off`, `pic=off` and `rtc=off` are not part of the base recipe: disabling PIT and PIC requires a guest CPU with `TSC_DEADLINE`, and `rtc=off` makes the guest rely on `kvmclock`, which the TCG mode does not provide.

```bash
qemu-system-x86_64 -M microvm,x-option-roms=off \
  -nodefaults -no-user-config \
  -display none -serial stdio -monitor none \
  -accel kvm -cpu host -m 256M -smp 1 \
  -kernel bzImage -initrd initramfs.cpio.gz \
  -append "console=ttyS0,115200 quiet reboot=t" \
  -drive id=target,file=disk.img,format=raw,if=none \
  -device virtio-blk-device,drive=target,serial=0123456789abcdef0123 \
  -fsdev local,id=state,path=/var/lib/diskedit,security_model=none \
  -device virtio-9p-device,fsdev=state,mount_tag=state \
  -no-reboot
```

- `-accel kvm` does not fall back. Where `/dev/kvm` is missing or not writable by the caller, replace `-accel kvm -cpu host` with `-accel tcg -cpu max`; the guest is several times slower and the rest of the command is unchanged. Decide by opening `/dev/kvm`, not by looking for the file.

On arm64 the machine is `-M virt` and the console is PL011:

```bash
qemu-system-aarch64 -M virt \
  -nodefaults -no-user-config \
  -display none -serial stdio -monitor none \
  -accel kvm -cpu host -m 256M -smp 1 \
  -kernel Image -initrd initramfs.cpio.gz \
  -append "console=ttyAMA0,115200 quiet" \
  -drive id=target,file=disk.img,format=raw,if=none \
  -device virtio-blk-device,drive=target,serial=0123456789abcdef0123 \
  -fsdev local,id=state,path=/var/lib/diskedit,security_model=none \
  -device virtio-9p-device,fsdev=state,mount_tag=state \
  -no-reboot
```

- Guest kernel: `CONFIG_VIRTIO_MMIO`, `CONFIG_VIRTIO_BLK`, `CONFIG_NET_9P_VIRTIO` and `CONFIG_9P_FS` on both architectures. The console driver differs: 8250 on x86, `CONFIG_SERIAL_AMBA_PL011` on arm64. initramfs: the static binary, the filesystem tools the command needs, and an init that mounts `state` and executes the binary.
- Shutdown and exit status: init syncs, writes the program's exit status to a file on the 9p share, then ends the guest. On x86, pass `reboot=t`: a forced reboot becomes a triple fault, and `-no-reboot` makes QEMU exit. On arm64 there is no `reboot=t` path; use PSCI `SYSTEM_OFF` for shutdown, and do not treat `SYSTEM_RESET` as a successful shutdown. Verify QEMU's exit behaviour for both paths separately. QEMU's own exit status says how the VM ended, not what the program returned; the wrapper reads the status file and returns that value.
- Identity: the record directory is derived from the device identity the guest sees, which differs from the host's view of the same image. Records written inside the guest resume only through that path; `records` and `inspect` on the host do not list them. Treat the guest as its own namespace.
- Locking: take the target lock on the host before starting QEMU and release it after the guest exits. The lock must cover the original image, not only a FUSE view in front of it, and a lock taken inside the guest does not exclude a host process.
- Persistence: write the status to a temporary file, `fsync` it, rename it, `fsync` the directory, and only then end the guest. Verify that both calls reach host storage over 9p; a successful syscall and an atomic rename do not by themselves prove durability.
- Signals and streams: killing QEMU terminates the VM; it does not deliver the same signal to the program inside. Forwarding signals, or separating stdout from stderr, needs another channel (a virtio-serial port, for example); the serial console is a single stream.

## Forced NTFS resize on a damaged volume

There is no single mode that always skips the filesystem step: `resize --end` and `resize --grow-to-end` run the filesystem step that matches the size change (grow for a larger end, shrink for a smaller one) unless `--no-fs` is given. `--no-fs` is itself refused for a shrink (the new end would cut into unshrunk metadata) and for finishing an already-extended partition whose filesystem step is still pending. `recover --replay-table-only` is the separate table-only recovery path: it replays the recorded table regions of a `move` whose data has already moved, and does not restore the data those ranges hold. So a manual `ntfsresize --force` followed by a plain `resize --end` means `ntfsresize` runs again; pass `--no-fs` when the filesystem must be left untouched. This tool invokes it as `ntfsresize -f -f` (grow, after a `--no-action` rehearsal) and `ntfsresize -f -f -s <bytes>` (shrink), so the volume's dirty flag alone does not block that step.

Whether the combined sequence is safe has not been run; treat it as unverified. `--force` bypasses the consistency-check guard; it does not repair filesystem damage. After a forced `ntfsresize` the volume is marked for a check: Windows may request or schedule one, and `chkdsk` should be run and the volume verified before normal use.

## Boundaries shared by every recipe

- **Exclusive access.** None of these recipes makes concurrent writers safe. While an export, a mount or a guest is active, do not let another tool, VM or `qemu-img` command write the same image; QEMU's warnings say this can destroy it.
- **Parser trust.** Running the filesystem work in a guest isolates the filesystem layer only. The container format is parsed by a host process, so an image from an untrusted source is host-side input.
- **Backups.** A recovery record covers the byte ranges this tool recorded. It does not cover filesystem-internal damage, image-format metadata, a missing backing layer, or inconsistencies left by a power loss. Keep a backup of anything that cannot be recreated.