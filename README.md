# DiskEdit

Edit disks and disk images as byte-addressable storage: partition tables (GPT/MBR), filesystem operations, and partition relocation.

English | [简体中文](README.zh-CN.md)

- **One code path for images and block devices.** A `.img` file and `/dev/sdX` differ only in how the target is opened.
- **GPT/MBR are handled in-process.** Table parsing and commit do not go through libparted or sfdisk. A GPT commit writes the backup entry array, backup header, primary entry array and primary header in that order, syncing after each step, so an interruption at any point leaves at least one self-consistent copy and CRC tells the copies apart. When partitions of a block device are mounted, the kernel re-read cannot succeed; that path writes the table with `sfdisk --no-reread` and syncs the kernel with `partx -u`.
- **Filesystems go through their own tools.** mkfs, fsck, resize, label and UUID are run against an offset loop device (`losetup`), so each filesystem is edited by its official implementation.
- **Long jobs are resumable.** A partition relocation, grow or mkfs interrupted midway continues from its recovery record when the same command is re-run. `inspect` and `records` report the scene, `undo` rolls the recorded writes back, `abandon` drops the scene.
- **Machine-readable output.** `info`, `plan` and `records list|verify|scan` print JSON; `plan --grow` carries the relocation preview in the same document as the report.

## Requirements

- **Linux.** Writing is supported on Linux only. Elsewhere the dispatch gate refuses any command whose primitives are missing, with exit code 6 and nothing written; on an image file `info` and `plan` still run read-only while evidence and write commands are refused — [docs/capability-matrix.md](docs/capability-matrix.md) has the per-command answer.
- **Rust** with edition 2024 support (1.85 or newer) to build.
- **root** for filesystem-layer commands: they map the partition through `losetup`. Table writes need root only for block devices, never for image files.

| Operation | Image file | Block device |
|---|---|---|
| `info` / `plan` / `inspect` / `records` | no root | no root |
| Table writes (`new`/`add`/`delete`/`resize-part`/`copy`/`set`/`undo`/`move`) | no root | root |
| Filesystem layer (`mkfs`/`resizefs`/`check`/`set label\|uuid`) | root | root |

`resize` needs root when it has a filesystem step; `--no-fs` declares that growing the filesystem is not a postcondition, which keeps an image grow rootless. A target whose filesystem cannot be identified (`unknown`) is refused unless `--no-fs` is given — "not recognised" does not mean "nothing there".

External tools are looked up on `PATH` only when a step needs them: `sfdisk`, `partx`, `losetup`, `mount`, and the per-filesystem tools (`mke2fs`/`e2fsck`/`resize2fs`, `mkfs.xfs`/`xfs_growfs`, …). The full table is in [docs/internals.md](docs/internals.md#external-tools).

## Build

```bash
cargo build --release
```

The binary is `target/release/DiskEdit` (help text and messages print it as `diskedit`).

## Quick start

```bash
truncate -s 8G disk.img
diskedit new disk.img --table gpt --yes
diskedit add disk.img 4G --yes
sudo diskedit mkfs disk.img:1 ext4 --yes
diskedit info disk.img | jq .
```

More examples, as printed by `diskedit help`:

```bash
diskedit info /dev/sdb
diskedit plan /dev/sdb:2 --grow
diskedit resize /dev/sdb:1 +2G --yes
diskedit resize disk.img grow --allow-move --chunk 4 --yes
diskedit undo /dev/sdb --yes
```

`<TARGET>` is an image path or a block device; `:N` addresses partition N (1-based) and `:last` the last partition a command can act on. `diskedit help <CMD>` prints the full help for one command.

`SIZE` accepts an absolute value (`10G`), an increment (`+2G`), a percentage (`+10%`) or the keyword `grow`. Sizes use 1024-based units, and a size that does not land on a sector boundary is truncated (and the truncation reported).

## Supported filesystems

Identification (`info`): ext, xfs, btrfs, f2fs, ntfs, vfat, exfat, swap, squashfs, erofs, hfsplus, apfs, lvm2_pv; anything else is `unknown`.

| FS | mkfs | grow | shrink | check | label | UUID |
|---|---|---|---|---|---|---|
| ext2/3/4 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| xfs | ✓ | ✓ | — | ✓ | ✓ | ✓ |
| btrfs | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| ntfs | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| f2fs | ✓ | ✓ | — | ✓ | — | — |
| vfat | ✓ | ✓ | — | ✓ | ✓ | — |
| exfat | ✓ | — | — | ✓ | ✓ | — |
| swap | ✓ | ✓ | — | — | ✓ | ✓ |
| lvm2_pv | — | ✓ | — | — | — | — |

- `check` is read-only in every case (`e2fsck -n`, `ntfsfix -n`, `xfs_repair -n`, `btrfs check`, `fsck.vfat -n`, `fsck.exfat -n`, `fsck.f2fs`); a non-zero tool exit is a failed check (exit code 3).
- `resize` only grows. Shrinking is `resize-part --end`: it shrinks the filesystem offline first, then moves the boundary. `resizefs` only grows, except the online btrfs form, which can also shrink. If the filesystem cannot shrink, the table is left alone.
- `mkfs` refuses an LVM PV; use `pvcreate(8)` / `wipefs(8)`.
- `ntfs` UUID can only be regenerated (`ntfslabel --new-serial`), not set; that serial is not the Windows volume UUID.
- `swap` grows by rebuilding with `mkswap` after the entry is resized; UUID, PARTUUID and partition number are kept.
- `lvm2_pv` grows to the PV layer only; `resize --grow-lv` continues into the LV and its filesystem.
- OpenWrt combined images (read-only squashfs/erofs root plus a trailing RW overlay in the same partition) grow only the RW layer, and only when the inner filesystem is ext or f2fs.

## Output and exit codes

`info`, `plan` and `records list|verify|scan` always print one JSON document on stdout; a failure adds an `error` string and leaves undetermined fields `null`. Nothing is printed on stdout before the document is complete, so a consumer can always parse it.

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | unexpected internal error |
| 2 | usage or command syntax error |
| 3 | policy refusal, or a precondition of the request is not met |
| 4 | operation failed with no persistent modification |
| 5 | partial, applied with follow-up steps pending, or a stale kernel view (the remedy is printed) |
| 6 | the platform or capability is not supported |

`--sector-size N` applies to image files only: an image carries no sector information and is read as 512-byte sectors unless told otherwise (4Kn images must say so). A block device reports its logical sector size through the kernel, and giving the option for one is refused.

## Documentation

- [docs/reference.md](docs/reference.md) — full command reference: syntax, options, help text and exit status for every command. Generated from the command schema; do not edit by hand.
- [docs/capability-matrix.md](docs/capability-matrix.md) — platform × media matrix for every command and capability. Generated; do not edit by hand.
- [docs/internals.md](docs/internals.md) — behaviour boundaries: capability gate, occupancy and locking, target identity, recovery records, resume semantics, external tools, verification scope.
- [docs/interop.md](docs/interop.md) — working with qcow2/vmdk/vdi, remote targets and sparse images around the tool: commands, seams and what is unverified.

## License

MIT — see [LICENSES/](LICENSES/).