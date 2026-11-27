Generated from `app/cli/declare.rs`, `app/report/exit.rs` and `diskedit_core::fs::catalog` by `docsgen`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`.
CI compares the working tree with the generator output. Do not edit by hand.

# diskedit capability matrix

One row per content kind the tool identifies. A cell is the support class
reported by the filesystem catalogue; the steps each class runs are declared
there as well.

| content | grow | shrink | attributes | check | mkfs |
|---|---|---|---|---|---|
| `squashfs` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `erofs` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `ext` | supported | supported | supported | supported | supported |
| `xfs` | supported | unsupported | supported | supported | supported |
| `btrfs` | supported | supported | supported | supported | supported |
| `f2fs` | supported | unsupported | unsupported | unsupported | supported |
| `exfat` | unsupported | unsupported | supported | supported | supported |
| `vfat` | unsupported | unsupported | supported | supported | supported |
| `ntfs` | supported | supported | supported | supported | supported |
| `hfsplus` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `apfs` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `bcachefs` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `jfs` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `reiserfs` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `udf` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `swap` | supported | unsupported | supported | unsupported | supported |
| `lvm2_pv` | not applicable | supported | unsupported | unsupported | unsupported |
| `luks` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `bitlocker` | unsupported | unsupported | unsupported | unsupported | unsupported |
| `unknown` | unsupported | unsupported | unsupported | unsupported | unsupported |

## Programs

External programs the catalogue refers to, by identity:

`E2fsck`, `Resize2fs`, `Dumpe2fs`, `Mke2fs`, `Tune2fs`, `Ntfsresize`, `Ntfsfix`, `Ntfslabel`, `FsckF2fs`, `ResizeF2fs`, `XfsGrowfs`, `XfsRepair`, `XfsAdmin`, `Btrfs`, `Btrfstune`, `Fatresize`, `Fatlabel`, `FsckVfat`, `Exfatlabel`, `FsckExfat`, `Mkswap`, `Swaplabel`, `Pvs`, `Lvs`, `Vgs`, `Pvresize`, `Lvextend`, `MkfsVfat`, `MkfsExfat`, `MkfsNtfs`, `MkfsXfs`, `MkfsBtrfs`, `MkfsF2fs`, `Mount`, `Umount`, `Losetup`, `Sfdisk`, `Partx`, `Blkid`

## Runtime gate

Writing is supported on Linux only. Elsewhere the dispatch gate refuses any
command that would write; read-only entries still run. Image files never need
root for table writes; block devices do.

| command | image file | block device | other platforms |
|---|---|---|---|
| `layout` | yes | yes | yes |
| `records list` | yes | yes | yes |
| `records verify` | yes | yes | yes |
| `records show` | yes | yes | yes |
| `undo` | yes | yes (root) | refused (6) |
| `replay` | yes | yes (root) | refused (6) |
| `release` | yes | yes (root) | refused (6) |
| `resize` | yes | yes (root) | refused (6) |
| `move` | yes | yes (root) | refused (6) |
| `copy` | yes | yes (root) | refused (6) |
| `delete` | yes | yes (root) | refused (6) |
| `set` | yes | yes (root) | refused (6) |
| `check` | yes | yes | yes |
| `mkfs` | yes | yes (root) | refused (6) |
| `new` | yes | yes (root) | refused (6) |
| `add` | yes | yes (root) | refused (6) |
