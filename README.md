# DiskEdit

把磁盘与磁盘镜像统一当作**可按偏移读写的字节存储**，在其上做分区表编辑、文件系统操作与分区搬移。

- **分区表**：GPT/MBR 的解析与写入不经过 libparted/sfdisk。GPT 提交按「备条目数组 → 备头 → 主条目数组 → 主头」四步，每步 `sync`，任意落点断电都至少留一份自洽副本，不一致可经 CRC 检出。块设备上挂载中的分区内核重读必然失败，该路径改用 `sfdisk --no-reread` 写表 + `partx -u` 同步内核。
- **文件系统**：mkfs/fsck/resize/label/uuid 经 offset loop（`losetup`）调用各 FS 官方工具。
- **镜像与块设备**：`.img` 与 `/dev/sdX` 走同一条代码路径，差别只在打开方式。

## 运行要求

**权限**：

| 操作 | 镜像文件 | 块设备 |
|---|---|---|
| `info` / `plan` | 不需要 root | 不需要 root |
| 分区表写入（`new`/`add`/`del`/`resize-part`/`copy`/`set`/`undo`） | 不需要 root | 需要 root |
| FS 层（`mkfs`/`resizefs`/`check`/`set label\|uuid`） | 需要 root | 需要 root |

`resize` 分两种：需要动文件系统时要 root；目标 FS 无法识别（`unknown`）时只改分区表，镜像上无需 root。

FS 层要 root 是因为经 `losetup` 映射分区；块设备写入靠设备节点权限，权限不足时在打开阶段即失败。

**外部工具**（按需存在即可，用到才查找）：

| 用途 | 工具 |
|---|---|
| 内核同步 / 在线写表 | `sfdisk`、`partx` |
| loop 与挂载 | `losetup`、`udevadm`、`mount`、`umount` |
| ext2/3/4 | `mke2fs`、`e2fsck`、`resize2fs`、`dumpe2fs`、`tune2fs` |
| xfs | `mkfs.xfs`、`xfs_growfs`、`xfs_repair`、`xfs_admin` |
| btrfs | `mkfs.btrfs`、`btrfs`、`btrfstune` |
| ntfs | `mkfs.ntfs`、`ntfsresize`、`ntfsfix`、`ntfslabel` |
| f2fs | `mkfs.f2fs`、`fsck.f2fs`、`resize.f2fs` |
| vfat | `mkfs.vfat`、`fsck.vfat`、`fatresize`、`fatlabel` |
| exfat | `mkfs.exfat`、`fsck.exfat`、`exfatlabel` |
| swap | `mkswap`、`swaplabel` |
| LVM | `pvs`、`lvs`、`vgs`、`pvresize`、`lvextend` |

所有外部调用注入 `LC_ALL=C`，避免本地化输出破坏解析。

## 命令

`<TARGET>` 为镜像路径或块设备；`:N` 寻址第 N 个分区（1 起）。`diskedit help <CMD>` 查看单命令详助。

| 命令 | 作用 | 写盘 |
|---|---|---|
| `info <TARGET>` | 分区表、逐分区 FS 识别、LVM 布局 | 否 |
| `resize <TARGET>:N <SIZE>` | 分区 + FS 一起扩缩，自动选在线/离线 | 是 |
| `move <TARGET>:N --start <LBA\|end>` | 移动分区，数据跟随 | 是 |
| `copy <TARGET>:N --start <LBA\|end> [--name S]` | 复制分区到新位置，源不动 | 是 |
| `create <TARGET> [--size B] [--name S] [--fs F]` | 在空闲空间创建分区 | 是 |
| `delete <TARGET>:N --yes` | 删除分区表项（不擦数据区） | 是 |
| `set <TARGET>:N name S \| label S \| uuid U \| flag F on\|off` | 设置分区属性 | 是 |
| `check <TARGET>:N` | FS 一致性检查 | 视工具 |
| `mkfs <TARGET>:N <FS> --yes` | 创建文件系统 | 是 |
| `resizefs <TARGET>:N` / `<MOUNTPOINT> [BYTES] --online` | 单独调整 FS | 是 |
| `undo <TARGET> --yes` | 按 journal 撤销本工具的写入 | 是 |
| `new` / `add` / `del` / `resize-part` / `plan` / `apply` | 低阶命令 | 见详助 |

`resize` 的 `SIZE` 支持绝对值（`10G`）、增量（`+2G` / `-500M`）、百分比（`+10%` / `-10%`）与 `grow` 关键字。

`--sector-size N` 是全局选项：镜像文件不携带扇区信息，默认按 512 处理，4Kn 镜像须显式指定。`resizefs` 的在线形式（挂载点）不适用该选项，显式拒绝。

**`check` 不是零写入**：ext 走 `e2fsck -fp`（preen 自动修复），ntfs 走 `ntfsfix -d`（清 dirty 位），两者都可能写入。只读检查为 xfs（`xfs_repair -n`）、btrfs（`btrfs check`）、vfat（`fsck.vfat -n`）、exfat（`fsck.exfat -n`）、f2fs（`fsck.f2fs` 无参）。

**LVM PV 不能用 `mkfs` 重建**：工具显式拒绝，须用 `pvcreate(8)` / `wipefs(8)`。

## 支持的 FS

**识别**（`fsid.rs`，按魔数）：ext、xfs、btrfs、f2fs、ntfs、vfat、exfat、swap、squashfs、erofs、hfsplus、apfs、lvm2_pv；都不匹配则为 `unknown`。

**动作覆盖**：

| FS | mkfs | 扩容 | 缩容 | check | label | uuid |
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

- `ntfs` 的 uuid 只能生成随机新序号，`ntfslabel` 不支持指定值。
- `swap` 的扩容是扩完表项后用 `mkswap` 重建，不搬数据；UUID、PARTUUID 与分区号保持。
- `lvm2_pv` 的扩容只到 PV 层，加 `--grow-lv` 才继续扩 LV 及其文件系统。

缩容时先缩 FS、后改分区边界；FS 缩不动就不动表。

**OpenWrt combined 布局**（同分区内 squashfs/erofs 只读根 + 尾部 RW overlay）：扩容识别内层 FS 后经 offset loop 只扩 RW 层，只扩不缩；内层非 ext/f2fs 时拒绝。多设备 btrfs 拒绝扩容。

## License

MIT（见 `LICENSES/`）