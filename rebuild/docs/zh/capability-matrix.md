本文件是 `docs/en/capability-matrix.md` 的中文译本，不参与生成物逐字比对；能力表以英文版与 `diskedit_core::fs::catalog` 为准。

# diskedit 能力矩阵

本工具可识别的每一种内容各占一行。单元格给出文件系统档案表报告的支持类别；
每类能力执行的步骤序列同样由该表声明。

| 内容 | 扩容 | 收缩 | 属性 | 检查 | 建卷 |
|---|---|---|---|---|---|
| `squashfs` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `erofs` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `ext` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `xfs` | 支持 | 不支持 | 支持 | 支持 | 支持 |
| `btrfs` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `f2fs` | 支持 | 不支持 | 不支持 | 支持 | 支持 |
| `exfat` | 不支持 | 不支持 | 支持 | 支持 | 支持 |
| `vfat` | 支持 | 不支持 | 支持 | 支持 | 支持 |
| `ntfs` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `hfsplus` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `apfs` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `bcachefs` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `jfs` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `reiserfs` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `udf` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `swap` | 支持 | 不支持 | 支持 | 不支持 | 支持 |
| `lvm2_pv` | 不适用 | 支持 | 不支持 | 不支持 | 不支持 |
| `luks` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `bitlocker` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| `unknown` | 不支持 | 不支持 | 不支持 | 不支持 | 不支持 |

## 程序

档案表引用到的外部程序（按身份）：

`E2fsck`, `Resize2fs`, `Dumpe2fs`, `Mke2fs`, `Tune2fs`, `Ntfsresize`, `Ntfsfix`, `Ntfslabel`, `FsckF2fs`, `ResizeF2fs`, `XfsGrowfs`, `XfsRepair`, `XfsAdmin`, `Btrfs`, `Btrfstune`, `Fatresize`, `Fatlabel`, `FsckVfat`, `Exfatlabel`, `FsckExfat`, `Mkswap`, `Swaplabel`, `Pvs`, `Lvs`, `Vgs`, `Pvresize`, `MkfsVfat`, `MkfsExfat`, `MkfsNtfs`, `MkfsXfs`, `MkfsBtrfs`, `MkfsF2fs`, `Mount`, `Umount`, `Losetup`, `Sfdisk`, `Partx`, `Blkid`

## Runtime gate

写入只在 Linux 上支持；其他平台拒绝会写入的命令，只读入口仍可运行。镜像的
表区写入从不需要 root，块设备需要。

| 命令 | 镜像文件 | 块设备 | 其他平台 |
|---|---|---|---|
| `layout` | 可以 | 可以 | 可以 |
| `records list` | 可以 | 可以 | 可以 |
| `records verify` | 可以 | 可以 | 可以 |
| `records show` | 可以 | 可以 | 可以 |
| `resize` | 可以 | 可以（需 root） | 拒绝（6） |
| `move` | 可以 | 可以（需 root） | 拒绝（6） |
| `copy` | 可以 | 可以（需 root） | 拒绝（6） |
| `delete` | 可以 | 可以（需 root） | 拒绝（6） |
| `set` | 可以 | 可以（需 root） | 拒绝（6） |
| `check` | 可以 | 可以 | 可以 |
| `mkfs` | 可以 | 可以（需 root） | 拒绝（6） |
| `resize --fs-only` | 可以 | 可以（需 root） | 拒绝（6） |
| `undo` / `replay` / `release` | 可以 | 可以（需 root） | 拒绝（6） |
| `new` | 可以 | 可以（需 root） | 拒绝（6） |
| `add` | 可以 | 可以（需 root） | 拒绝（6） |

