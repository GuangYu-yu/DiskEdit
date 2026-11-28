# DiskEdit

Edit disks and disk images as byte-addressable storage: partition tables (GPT/MBR), filesystem operations, and partition relocation.

English | [简体中文](README.zh-CN.md)

- **One code path for images and block devices.** A `.img` file and `/dev/sdX` differ only in how the target is opened.
- **GPT/MBR are handled in-process.** Table parsing and commit do not go through libparted or sfdisk. A GPT commit writes the backup entry array, backup header, primary entry array and primary header in that order, syncing after each step, so an interruption at any point leaves at least one self-consistent copy and CRC tells the copies apart. When partitions of a block device are mounted, the kernel re-read cannot succeed; that path writes the table with `sfdisk --no-reread` and syncs the kernel with `partx -u`.
- **Filesystems go through their own tools.** mkfs, fsck, resize, label and UUID are run against an offset loop device (`losetup`), so each filesystem is edited by its official implementation.
- **Long jobs are resumable.** A partition relocation, grow or mkfs interrupted midway continues from its recovery record when the same command is re-run. `records show` and `records list` report the scene, `undo` rolls the recorded writes back, `release` drops the scene.
- **Machine-readable output.** `layout --json` and `records list|verify` print JSON.

## Requirements

- **Linux to write.** Writing is supported on Linux only: elsewhere the dispatch gate refuses a write before the target is opened, while read-only entries are admitted and run where the host has a backend for them — [docs/en/capability-matrix.md](docs/en/capability-matrix.md) has the per-command answer.
- **Rust** with edition 2024 support (1.85 or newer) to build.
- **root** for filesystem-layer commands: they map the partition through `losetup`. Table writes need root only for block devices, never for image files.

| Operation | Image file | Block device |
|---|---|---|
| `layout` / `records` | no root | no root |
| Table writes (`new`/`add`/`delete`/`resize`/`copy`/`set`/`undo`/`replay`/`release`/`move`) | no root | root |
| Filesystem layer (`mkfs`/`resize --fs-only`/`check`/`set label\|uuid`) | root, or the VM carrier | root |

`resize` needs root when it has a filesystem step; `--no-fs` declares that growing the filesystem is not a postcondition, which keeps an image grow rootless. A target whose filesystem cannot be identified (`unknown`) is refused unless `--no-fs` is given — "not recognised" does not mean "nothing there".

Filesystem steps that need root can run in a VM instead: when the host falls short, `DISKEDIT_VM_BUNDLE` names a verified carrier bundle and the whole plan moves into the guest while the host keeps the record, lock and exit code. Placement is automatic — see [docs/en/interop.md](docs/en/interop.md#rootless-use-through-the-vm-carrier).

External tools are resolved from the fixed system tool path only when a step needs them: `sfdisk`, `partx`, `losetup`, `mount`, and the per-filesystem tools (`mke2fs`/`e2fsck`/`resize2fs`, `mkfs.xfs`/`xfs_growfs`, …). The full table is in [docs/en/internals.md](docs/en/internals.md#external-tools).

Write commands modify the target itself, so a wrong change can leave the original contents unrecoverable. Editing in place needs no room for a second image and leaves no extra file to clean up; back the target up first when its contents matter.

## Build

```bash
cargo build --release
```

The binary is `target/release/diskedit`.

## Quick start

```bash
truncate -s 8G disk.img
diskedit new disk.img --table gpt --yes
diskedit add disk.img 4G --yes
sudo diskedit mkfs disk.img:1 ext4 --yes
diskedit layout disk.img --json | jq .
```

More examples, as printed by `diskedit help`:

```bash
diskedit layout /dev/sdb --json
diskedit add /dev/sdb 10G --dry-run
diskedit resize /dev/sdb:1 +2G --yes
diskedit resize disk.img grow --allow-move --chunk 4 --yes
diskedit undo /dev/sdb --yes
```

`<TARGET>` is an image path or a block device; `:N` addresses partition N (1-based) and `:last` the last partition a command can act on. `diskedit help <CMD>` prints the full help for one command.

`SIZE` accepts an absolute value (`10G`), an increment (`+2G`), a percentage (`+10%`) or the keyword `grow`. Sizes use 1024-based units, and a size that does not land on a sector boundary is truncated (and the truncation reported).

### In-place partition layout

The target is edited in place; no new whole-disk copy is made. A relayout is a series of independent commands: each one asks for its own confirmation, keeps its own record, and leaves the steps that already succeeded applied if a later one fails. An unfinished step resumes from its record.

- **Grow the container first.** The capacity of a raw image is the size of the file, so enlarge it with `truncate -s 20G disk.img` before touching the partitions inside. Extending a GPT disk leaves the backup header at the old end; the next table command relocates it as part of its plan.
- **Growth.** `diskedit resize disk.img:1 +2G --yes`; `resize ... grow` fills the contiguous free space to the right, and `--grow-to-end` pins the end at `last_usable_lba`. A blocker in the way is moved only with `--allow-move`, after the plan is printed.
- **Shrink.** `diskedit resize disk.img:1 -500M --yes` shrinks the filesystem offline and then the entry, on GPT and on MBR primary partitions.
- **Moves.** `diskedit move disk.img:2 --start <LBA> --yes` relocates a partition together with its data and resumes from its record. MBR partitions cannot be moved.
- **Handing space from A to B** (GPT only — MBR has no relocation, so the hole cannot be passed on): shrink A to open the hole, move B into it, then let B grow into the space it vacated:

  ```bash
  diskedit resize disk.img:1 -2G --yes
  diskedit move disk.img:2 --start <LBA just past A> --yes
  diskedit resize disk.img:2 grow --yes
  ```

- **`--delete`.** `diskedit delete disk.img:2 --yes` removes the entry and leaves the following partitions where they are; the hole is reclaimed with the recipe above. A partition the command does not name keeps both its data and its position.
- **Extra empty partition.** `diskedit add disk.img <SIZE> --yes` appends an entry; `resize` never creates one.
- **Preview.** `--dry-run` prints the ranges, the filesystem step and any relocation plan, writes nothing and does not require `--yes`.
- **Start alignment and the NTFS boot sector.** Whenever `move` — or a resize that relocates the partition — shifts an NTFS partition to a new start LBA, the hidden-sectors field at offset 0x1c of its VBR is rewritten to the new LBA, with a warning when the start no longer fits the 32-bit field. `resize` keeps the start where it is; `move :1 --start align` places the partition on the next 1 MiB boundary. A neighbour in the way is refused as a range overlap rather than relocated; MBR partitions cannot be moved at all.
- **No force switch.** A shrink is refused before any table write when the filesystem cannot report a size below the request (the ext pre-check asks `resize2fs -P` for the minimum).

## Supported filesystems

Identification (`layout`): ext, xfs, btrfs, f2fs, ntfs, vfat, exfat, swap, squashfs, erofs, hfsplus, apfs, lvm2_pv, luks, bitlocker, bcachefs, jfs, reiserfs, udf; anything else is `unknown`. The last six are identified only — none of their `mkfs`/`check`/`resize` steps is wired (bcachefs may span several devices, so its member list is not read either).

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
| lvm2_pv | — | ✓ | ✓ | — | — | — |

- `check` runs a read-only consistency check for every type except f2fs (`e2fsck -n`, `ntfsfix -n`, `xfs_repair -n`, `btrfs check`, `fsck.vfat -n`, `fsck.exfat -n`, `fsck.f2fs`); a non-zero tool exit is a failed check, exit 3 — or exit 5 for f2fs, which can rewrite global metadata even without `-f`.
- `resize` grows or shrinks a GPT or MBR primary partition from a size expression (`+2G`, `-500M`, `10G`, `grow`); a shrink runs the filesystem step offline first. MBR covers partitions 1..4 — extended containers and logical partitions are out of scope. `resize --dry-run` prints the plan and writes nothing. `resize --fs-only` only grows the filesystem (offline into its partition, or online through a mountpoint), except the online btrfs form, which can also shrink. If the filesystem cannot shrink, the table is left alone.
- `mkfs` refuses an LVM PV; use `pvcreate(8)` / `wipefs(8)`.
- `ntfs` UUID can only be regenerated (`ntfslabel --new-serial`), not set; that serial is not the Windows volume UUID.
- `swap` grows by rebuilding with `mkswap` after the entry is resized; UUID, PARTUUID and partition number are kept.
- `lvm2_pv` grows to the PV layer only; `resize --grow-lv` continues into the LV and its filesystem. On an image target a shrink runs `pvresize --setphysicalvolumesize` before the entry changes (block devices refuse shrink), which requires that no allocated extent lies beyond the new end (move it first with `pvmove`) and that the VG is not active. The temporary loop can make udev activate the VG again, so also set `lvchange -ky <vg/lv>`; if a holder shows up after `pvresize`, the table is left unchanged.
- OpenWrt combined images (read-only squashfs/erofs root plus a trailing RW overlay in the same partition) grow only the RW layer, and only when the inner filesystem is ext or f2fs.

## Output and exit codes

`layout --json` and `records list|verify` always print one JSON document on stdout; a failure adds an `error` string and leaves undetermined fields `null`. Nothing is printed on stdout before the document is complete, so a consumer can always parse it.

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | unexpected internal error |
| 2 | usage or command syntax error |
| 3 | policy refusal, or a precondition of the request is not met |
| 4 | this run wrote no target bytes and no record entry was created, renamed, moved away or deleted (the records directory and `record.lock` do not count) |
| 5 | partial, records changed while the target was not fully written, or applied with follow-up steps pending or a stale kernel view (the remedy is printed) |
| 6 | the platform or capability is not supported |

`--sector-size N` applies to image files only: an image carries no sector information and is read as 512-byte sectors unless told otherwise (4Kn images must say so). A block device reports its logical sector size through the kernel, and giving the option for one is refused.

## Documentation

- [docs/en/reference.md](docs/en/reference.md) — full command reference: syntax, options, help text and exit status for every command. Generated from the command schema; do not edit by hand.
- [docs/en/capability-matrix.md](docs/en/capability-matrix.md) — platform × media matrix for every command and capability. Generated; do not edit by hand.
- [docs/en/internals.md](docs/en/internals.md) — behaviour boundaries: capability gate, occupancy and locking, target identity, recovery records, resume semantics, external tools, verification scope.
- [docs/en/interop.md](docs/en/interop.md) — working with qcow2/vmdk/vdi, remote targets and sparse images around the tool: commands, seams and what is unverified.
- [docs/policy.md](docs/policy.md) — fallback and refusal policy: when continuing is safe, when to refuse, and which state belongs to this tool.

## License

MIT — see [LICENSES/](LICENSES/).