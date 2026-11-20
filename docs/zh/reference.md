# diskedit 命令参考

由 `src/docs.rs` 从 `src/cli.rs` 中的命令 schema 生成。
用 `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit docs_are_generated_in_sync` 重新生成；
CI 会重新生成，并在工作树不一致时失败。不要手工编辑。

## 命令

### `info`

显示分区表及各分区的文件系统识别结果；--grow 追加 :N 的搬移预览

```text
diskedit info <TARGET> [--sector-size N]
diskedit info <TARGET>:N --grow [--sector-size N]
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` / `TARGET` |
| 选项 | `--sector-size`, `--grow` |
| 确认 | 无 |
| 恢复记录 | 只读 |
| 能力 | 只读 |

```text
  显示分区表及各分区的文件系统识别结果（LVM PV 在 fs 字段中显示为
  lvm2_pv；PV 空间通过 `resize --grow-lv` 生效）。只读。
  给出 --grow 时同一份文档增加 `preview` 键，用于把分区 :N 扩容到最后一
  个可用 LBA；它只读取——不写入任何内容：
    grow_part     被扩容的分区
    new_end_lba   扩容后它的结束 LBA
    sector_size   计数 LBA 所用的逻辑块大小
    moves         数组，元素为 {num, first_lba, last_lba, delta_lba, bytes, swap}；
                  swap 标记该区域是在原地重建而非复制
  两种结果下 stdout 都是 JSON；失败时增加 `error`（字符串），并把无法确定
  的布局字段保留为 null。键：
    label         "gpt" | "mbr" | "none"
    damaged       bool——表需要修复，或无法按现状读取
    sector_size   逻辑块大小，单位字节
    size_bytes    目标大小，单位字节
    disk_guid     GPT 磁盘 GUID，表没有该值时为 null
    last_part     `:last` 解析到的分区号；没有时为缺失
    partitions    数组，元素为 {num, first_lba, last_lba, size_bytes, type, fs, name}；
                  type 是 GPT GUID 或 MBR 的 "0xXX"，fs 是检测到的名称
                  （或 "container" / "error"），name 在 MBR 上为 null
```

### `inspect`

显示目标旁的恢复记录（只读）

```text
diskedit inspect <TARGET|PATH>
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` |
| 选项 | 无 |
| 确认 | 无 |
| 恢复记录 | 只读 |
| 能力 | 只读, 伴生文件 |

```text
target：
  TARGET 是整块镜像或块设备；`:N` 分区后缀会被拒绝。
  `--sector-size` 不被接受：inspect 报告的是记录，不是磁盘几何。

path：
  PATH 按结构分派：恢复记录目录内的直接条目
  （`<image_state_root>/images/<key-hash>/...`、`<image>.diskedit/...`
  或 `<state_root>/recovery/<identity-hash>/...`，不含 `record.lock`）
  本身即作为证据解码。隔离区（`.abandoned[.N]`）中的条目会被如此标记，
  不计为活动记录。其他路径一律按 TARGET 处理。

输出：
  证据路径、记录字段（事务、计划、操作、进度、提交与保留状态），或阻止
  给出判定的结构性失败。`identity check: ForeignCheckUnavailable` 表示平台
  没有给出稳定的目标身份；该记录不参与比较。

退出状态：
  0  可读的结果：没有记录、记录完整、记录未完成（输出会点名回滚 / 搁置
     命令），或身份检查无法进行
  3  字节被截断、CRC 或 magic 不匹配、身份不匹配、条目不是普通文件，
     或锚点目录、记录、目标无法读取
  6  该平台不提供这套命令
  `Unverified` 记录会被标注，并沿用其判定对应的退出状态
```

### `records list`

以 JSON 列出恢复记录（省略 TARGET 时为 <state_root> 下的全部）

```text
diskedit records list [TARGET]
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`（可选） |
| 选项 | 无 |
| 确认 | 无 |
| 恢复记录 | 只读 |
| 能力 | 只读, 伴生文件 |

```text
  以 JSON 列出恢复记录。只列出可解码的记录；未知格式的条目会被略过
  （用 `records scan` 查看）。隔离条目（`.abandoned[.N]`）会被列出并标记，
  但不属于活动记录集合。

  目前还没有索引：每一行都带 "registered": false，记录在内存中枚举。
  未被 TARGET 指向的记录目录不会被搜索——传 TARGET 可查看其中一个，或用
  `records scan` 指向保存镜像的目录或镜像状态根。
```

### `records verify`

按 32 位十六进制 id 复查一条记录

```text
diskedit records verify <TARGET> <ID>
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`, `ID` |
| 选项 | 无 |
| 确认 | 无 |
| 恢复记录 | 只读 |
| 能力 | 只读, 伴生文件 |

```text
  在 TARGET 上定位 id 为 ID 的记录（32 位小写十六进制，与 `records list`
  打印的一致），重新探测 TARGET 的身份，并以 JSON 打印判定。

  退出状态：
    0  恰好一条记录，且身份检查没有失败
    3  未找到该 id（A4），或它出现在两个或更多位置，或这条记录未通过
       身份 / 后端 / 结构检查，或记录、其目录或目标无法读取

  查找失败时打印一行摘要，以计数给出 "hits"（完全匹配）与 "unverified"
  （id 无法解码的条目）。"records" 只出现在行文档中，且总是数组。
```

### `records scan`

枚举容器根下的记录（<state_root>/recovery 或镜像目录）

```text
diskedit records scan <ROOT>
```

| 属性 | 值 |
|---|---|
| 位置参数 | `ROOT` |
| 选项 | 无 |
| 确认 | 无 |
| 恢复记录 | 只读 |
| 能力 | 只读, 伴生文件 |

```text
  枚举容器根下的记录，并以 JSON 打印。

  ROOT 是以下之一：
    <state_root>/recovery      每个直接子目录都是身份哈希记录目录
    <image_state_root>         其 `images` 子目录为每个镜像键保存一个记录目录
    <image_state_root>/images  每个直接子目录都是镜像记录目录
    <image directory>          每个名为 <name>.diskedit 的直接子项都是记录目录

  <state_root> 下除 `recovery` 以外的子目录一律拒绝：记录目录不是容器。
  扫描深度限制为 3，单文件 64 MiB，总量 512 MiB，时间 30 秒。超出任一
  上限会报告结果不完整并以退出码 3 退出；不会打印部分列表。

  目前还没有索引：每一行都带 "registered": false。
```

### `resize`

调整分区及其文件系统大小；--end / --grow-to-end 在不搬移数据的情况下重划分区

```text
diskedit resize <TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes
diskedit resize <TARGET>:N --end LBA [--align A] [--no-fs] --yes
diskedit resize <TARGET>:N --grow-to-end [--align A] [--no-fs] --yes
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`, `SIZE` / `TARGET` / `TARGET` |
| 选项 | `--no-fs`, `--sector-size`, `--yes`, `--allow-move`, `--chunk`, `--dry-run`, `--grow-lv`, `--lv`, `--end`, `--align`, `--grow-to-end` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 外部工具, 设备状态 |

```text
  调整分区及其文件系统大小，按需走分区 -> PV -> LV -> 文件系统。在线/离线
  方式自动选择。GPT 与 MBR 主分区；superfloppy（整盘文件系统）省略 :N——
  只扩容，从不收缩。

  示例：
    diskedit resize /dev/sdb:1 +2G --yes
    diskedit resize disk.img 10G --allow-move --chunk 4 --yes

  SIZE：
    10G       把尺寸设为 10 GiB（单位 k/m/g/t，1024 进制；无单位时为
              纯字节数）
    +2G       扩容 2 GiB
    -500M     收缩 500 MiB（GPT 与 MBR 主分区；文件系统先离线收缩）
    +10%      扩容当前尺寸的 10%（向下取整到 1MiB）
    -10%      收缩当前尺寸的 10%
    grow      扩容到右侧连续的空闲空间
    尺寸可以带小数：1.5G = 1610612736 字节，+1.5% 同理。未落在扇区边界上
    的尺寸会被截断，并报告该截断

  试运行：
    --dry-run 打印算出的范围、文件系统步骤与任何搬移计划，然后在第一次
    写入之前停下；不建记录，也不需要 --yes。数值取自写盘路径所用的同一份
    解析与计划代码。

  就地（--end LBA | --grow-to-end）：
    只重新划分末端而不搬移分区；起点保持在当前起始 LBA（搬移它是
    `diskedit move`）。--grow-to-end 把末端固定在 last_usable_lba（填充
    语义）。末端低于当前末端会连同文件系统一起收缩分区（离线）；被中断的
    收缩无法用 `recover` 回滚。

  约束：
    收缩      小于当前尺寸的 SIZE 在 GPT 与 MBR 主分区（1..4；扩展容器与
              逻辑分区不在此列）上收缩；被中断的收缩在以同一绝对尺寸重跑时续跑。
              挂载中的文件系统拒绝收缩——先卸载
    --no-fs   在 LVM PV 上拒绝（PV 链本身没有文件系统步骤），在收缩上也
              拒绝。作为挡路者被搬移的 swap 分区无论如何都会重建：该步骤
              属于搬移，不属于文件系统
    MBR       与 GPT 使用同一种恢复记录：被中断的扩容或收缩在以同一绝对
              尺寸重跑时续跑；`recover` 则把记录中的表写入回滚
    record    在请求绑定其摘要之前写下的搬移记录不带摘要：它无法与本次
              请求关联，因此重新运行 `resize --allow-move` 会被拒绝。先用
              `inspect` 查看，再用 `recover --release` 释放
    在线      写入经由工具（sfdisk / 文件系统自身的 resize）完成，不经过
              本工具的记录：没有可续跑的内容，`recover` 也无法回滚

  读回：
    扩容成功后读回文件系统大小，且必须在 ext2/3/4（dumpe2fs：一个块组——
    resize2fs 会留下它无法容纳组元数据的尾部）、ntfs（ntfsresize：一个簇）
    与 btrfs（btrfs inspect-internal dump-super：一个扇区大小）上，于一个
    放置单元内到达新范围的末尾。ext 扩容可能最多差一个块组：resize2fs
    丢弃放不进组元数据的尾部，读回把这种情况计为到达末端。f2fs、xfs 与
    vfat 在本次构建中没有读回：工具的成功被记为未验证结果，运行会打印一条
    点名尺寸检查的待办项，退出状态为 5。分区步骤无论如何都会提交；用挂载点
    上的 `df -B1` 与范围末端比较以最终确认。

  相关命令：这是某个分区的唯一入口——它在一次运行中完成计划与应用。
  `info --grow` 预览扩容而不写入；`move` 搬移分区并续跑它自己的中断任务。
  在其分区步骤之后被中断的 resize（LVM 收尾链仍待完成）通过重新运行
  `resize` 续跑。
```

### `move`

移动分区，数据随之搬移（分块复制，可续跑）

```text
diskedit move <TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` |
| 选项 | `--sector-size`, `--yes`, `--align`, `--chunk`, `--start` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 外部工具, 设备状态 |

```text
  移动分区，数据随之搬移（分块复制，可从其恢复记录续跑）。

  位置：
    <LBA>     新的起始 LBA（按 --align 对齐；默认 none 表示按给定的 LBA）
    end       尾部贴合到最后一个可能的位置
    align     从当前起点向上取整到下一个 1 MiB 边界（仅 GPT；--align 不适用）。
              相邻分区不会被搬移；落点落在相邻分区上时按范围重叠拒绝

  swap 分区的搬移方式是提交其条目并在新位置重建该区域（mkswap）；
  其内容不会被复制。

  确认：必须给出。目标区间会被原样覆盖，且被中断的搬移无法用 `recover`
  回滚；请传 --yes。
```

### `copy`

把分区复制到新位置；源不受影响

```text
diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` |
| 选项 | `--sector-size`, `--yes`, `--align`, `--chunk`, `--keep-partuuid`, `--name`, `--start` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 设备状态 |

```text
  按字节把分区复制到新位置；源不受影响。--start end 把副本贴合到可用区间
  的末尾。
  没有续跑：被中断的复制无法从断点继续。它会在目标上留下不可回滚的日志，
  因此在用 `diskedit recover --release` 释放它之前，写命令都会拒绝——释放后
  重新运行，从开头重写目标位置。

  PARTUUID：副本得到新生成的 v4 GUID，因此两个分区保持可区分。
  --keep-partuuid 改为复用源的 GUID——此后按 PARTUUID 挂载会解析到其中
  任意一个分区。

  确认：必须给出。目标区间会被源字节覆盖，且日志把这次写入记为不可回滚——
  `recover` 无法恢复原先的内容；请传 --yes。
```

### `delete`

删除分区条目（数据区不会被擦除）

```text
diskedit delete <TARGET>:N --yes
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` |
| 选项 | `--sector-size`, `--yes` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 设备状态 |

```text
  删除一个分区条目（数据区不会被擦除）。
```

### `set`

设置分区属性（name / label / uuid / flag / type）

```text
diskedit set <TARGET>:N name S --yes
diskedit set <TARGET>:N label S --yes
diskedit set <TARGET>:N uuid [U] --yes [--random]
diskedit set <TARGET>:N flag F on|off --yes
diskedit set <TARGET>:N type GUID|0xXX --yes
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`, `PROPERTY`, `VALUE` / `TARGET`, `PROPERTY`, `VALUE` / `TARGET`, `PROPERTY`, `VALUE`（可选） / `TARGET`, `PROPERTY`, `VALUE`, `STATE` / `TARGET`, `PROPERTY`, `VALUE` |
| 选项 | `--random`, `--sector-size`, `--yes` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 外部工具, 设备状态 |

```text
  设置分区属性。
    name          GPT 分区名
    type          GPT：分区类型 GUID（连字符可省略，例如
                  C12A7328-F81F-11D2-BA4B-00A0C93EC93B）；MBR：类型字节（0xXX）
    label / uuid  文件系统标签 / UUID（感知文件系统）。--random 要求目标
                  生成新值（字面值 `random` 视同 --random）。生成方式因文件
                  系统而异：ext 为 tune2fs -U random，xfs 为
                  xfs_admin -U generate，btrfs 为 btrfstune -u；swap 没有
                  生成形式；ntfs 的序列号不是 Windows 卷 UUID
    flag          GPT：esp|boot|legacy|legacy_boot|hidden|required——esp/boot
                  置为 on 会把类型 GUID 切换为 EFI System Partition；置为 off
                  时仅在当前类型为 ESP 时切回（FAT/NTFS 内容回退到 Microsoft
                  basic data，其余回退到 Linux filesystem data；不保留先前的
                  类型 GUID）。其他类型 GUID 保持不变。MBR：boot|hidden
```

### `check`

检查文件系统一致性（不修复；SIT 不匹配时 f2fs 会写入元数据）

```text
diskedit check <TARGET>:N
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` |
| 选项 | `--sector-size` |
| 确认 | 无 |
| 恢复记录 | 只读 |
| 能力 | 只读, 外部工具 |

```text
  检查文件系统一致性，不做修复。所用工具取决于类型：ext 用 e2fsck -n，
  ntfs 用 ntfsfix -n，xfs 用 xfs_repair -n，btrfs 用 btrfs check，
  vfat/exfat 用 fsck -n，f2fs 用 fsck.f2fs。工具以非零退出即为检查失败：
  只读检查器为退出码 3；f2fs 可能已经写入，因此其失败为退出码 5。它不是
  严格只读的：当 SIT 段类型与其记录的类型不一致时，fsck.f2fs 即使不带 -f
  也会重写全局元数据，--dry-run 也阻止不了。
```

### `mkfs`

创建文件系统（销毁分区上的全部数据）

```text
diskedit mkfs <TARGET>:N <FS> --yes [--label S]
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`, `FS` |
| 选项 | `--sector-size`, `--yes`, `--label` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 伴生文件, 外部工具, 设备状态 |

```text
  创建文件系统。销毁分区上的全部数据；先擦除残留的签名。支持：ext2/3/4、
  xfs、btrfs、f2fs、vfat、exfat、ntfs、swap。
```

### `resizefs`

仅调整文件系统大小——分区条目保持不变

```text
diskedit resizefs <TARGET>:N [BYTES] --yes
diskedit resizefs <MOUNTPOINT> [BYTES] --yes
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`, `BYTES`（可选） / `TARGET`, `BYTES`（可选） |
| 选项 | `--sector-size`, `--yes` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 伴生文件, 外部工具, 设备状态 |

```text
  调整文件系统大小。形式决定模式：<TARGET>:N 为离线，把文件系统扩容到其
  分区内；挂载点为在线，作用于已挂载的分区。区域内没有可扩容的文件系统时
  拒绝：LVM PV 通过 `resize --grow-lv` 获取空间，无法识别的类型没有可做的
  操作。首次挂载前的 OpenWrt overlay 层在此无事可做（退出码 0）：fstools
  在首次挂载时创建它，填满尾部区域。

  在线形式：分区表从不改动；文件系统在当前分区内扩容（btrfs 也可收缩）。
  超出当前分区的尺寸会被拒绝——先运行 `diskedit resize` 扩大分区。目标
  尺寸是绝对量（单位 k/m/g/t，1024 进制；无单位时为纯字节数；允许小数）；
  省略表示扩容填满分区。

  ext2/3/4、ntfs 与 btrfs 的扩容会读回，且必须在一个放置单元内到达分区
  末端（ext2/3/4 为一个块组，其尾部 resize2fs 可能留下未用）。f2fs、xfs
  与 vfat 没有读回——工具的成功保持未验证，运行会打印一条点名尺寸检查的
  待办项，退出状态为 5。
```

### `recover`

以恢复记录收尾：回放它，或释放它而不回滚

```text
diskedit recover <TARGET> --yes
diskedit recover <TARGET> --replay-table-only --yes
diskedit recover <TARGET> --release [--discard-replay] --yes
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` / `TARGET` / `TARGET` |
| 选项 | `--sector-size`, `--yes`, `--replay-table-only`, `--release`, `--discard-replay` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，无恢复记录 |
| 能力 | 目标数据, 伴生文件, 外部工具, 设备状态 |

```text
  以本目标的恢复记录收尾。两种结果共用一条命令：

  replay    （默认）放回被记录操作覆盖的字节——分区表条目以及它就地改动的
            文件系统元数据（NTFS 备份扇区指针）。已搬移的数据无法回滚，
            外部文件系统工具所做的写入也不会被撤销。--replay-table-only
            只回放一次数据已搬移完成的移动操作所记录的表区域。
  --release 丢弃恢复文件，保留它们所描述的写入。这不要求记录可读，也不
            回放任何内容。当场景仍持有至少一个回放可恢复的区间——或载荷已
            损坏、因而数量未知——时，除非给出 --discard-replay，否则释放会
            被拒绝；该旗标确认放弃一个可回放的区间，它不是 --yes 的另一种
            写法。

  示例：
    diskedit recover /dev/sdb --yes
    diskedit recover /dev/sdb --release --yes
```

### `new`

创建全新的分区表

```text
diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET` |
| 选项 | `--sector-size`, `--yes`, `--table`, `--overwrite-data` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 设备状态 |

```text
  创建全新的分区表；覆盖任何既有表。默认 gpt。写表会把条目数组写到磁盘
  前端，gpt 还会在末尾扇区写一份备份；mbr 则清除残留的 GPT 头签名——这些
  字节可能属于某个既有分区。这种冲突会被点名并拒绝；--overwrite-data
  接受覆盖这些字节（仅给 --yes 只确认重建）。
```

### `add`

新增分区；--start 指定它的起点

```text
diskedit add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes
```

| 属性 | 值 |
|---|---|
| 位置参数 | `TARGET`, `SIZE` |
| 选项 | `--sector-size`, `--yes`, `--align`, `--name`, `--part-number`, `--start`, `--type-guid` |
| 确认 | 必须（`--yes`） |
| 恢复记录 | 写入，有恢复记录 |
| 能力 | 目标数据, 伴生文件, 设备状态 |

```text
  新增一个持有 SIZE 的分区。不给 --start 时，条目放入第一个能容纳它的、
  按 1 MiB 对齐的空闲间隙；--align 改变对齐方式（none 表示按给定的 LBA）。
  给出 --start 时，分区从该 LBA 起（按给定值），放不下则拒绝。SIZE 是
  绝对字节数（允许 K/M/G/T 后缀）；相对形式（+32M、-32M、32%）被拒绝——
  新分区没有可供相对的旧尺寸。不给 --part-number 时，条目占用第一个未使用
  的槽位。GPT 上类型默认为 Linux filesystem data；MBR 上 --type-guid 取
  0xNN，默认为 0x83（Linux）。
```

## 全局选项

| 旗标 | 取值 | 说明 |
|---|---|---|
| `--align` | `A` | 放置对齐：mib\|none。add 的自动放置默认为 1 MiB；显式的 --start（add/move/copy）与 resize 计算出的末端只在给出 --align 时对齐；move 的 `--start align` 恒为 1 MiB |
| `--allow-move` | — | 允许搬移挡路的分区：先打印计划，然后继续写入 |
| `--chunk` | `MiB` | 恢复粒度，单位 MiB，1..=64；续跑的任务沿用记录中的值，新任务使用 4 |
| `--discard-replay` | — | 确认放弃 `recover` 仍可回放的区间 |
| `--dry-run` | — | 打印计划并在第一次写入前停止；不需要 --yes |
| `--end` | `LBA` | `resize` 的新结束 LBA（就地扩容或收缩） |
| `--grow-lv` | — | 同时扩容 LV：由 --lv 指定的一个，否则为唯一的 LV |
| `--grow-to-end` | — | 把末端固定在最后一个可用 LBA（填充语义） |
| `--grow` | — | 追加分区 :N 的搬移预览 |
| `--keep-partuuid` | — | 复用源的 PARTUUID，而不是生成新的 |
| `--label` | `S` | 文件系统标签（省略即空标签，不是默认值） |
| `--lv` | `NAME` | 要扩容的 LV（隐含 --grow-lv） |
| `--name` | `S` | 该命令创建的条目名 |
| `--no-fs` | — | 跳过扩容分区的文件系统步骤 |
| `--overwrite-data` | — | 接受覆盖既有分区所占据的字节 |
| `--part-number` | `N` | 条目槽位，从 1 起；必须为空 |
| `--random` | — | 要求目标生成新值 |
| `--release` | — | 丢弃恢复文件，保留它们所描述的写入 |
| `--replay-table-only` | — | 允许 `recover` 只回放一次数据已搬移完成的移动操作所记录的表区域；这些区域所指的数据不会被恢复 |
| `--sector-size` | `N` | raw 镜像的逻辑扇区大小覆盖值（2 的幂，最大 65536 字节——这是实现限制，不是格式要求） |
| `--start` | `LBA\|end\|align` | 新起点：一个 LBA，`end` 表示尾部贴合，或 `align` 表示下一个 1 MiB 边界（仅 move） |
| `--table` | `gpt\|mbr` | 分区表类型：gpt（默认）或 mbr |
| `--type-guid` | `GUID\|0xNN` | GPT：类型 GUID 文本，例如 0fc63daf-8483-4772-8e79-3d69d8477de4；MBR：类型字节 0xNN |
| `--yes` | — | 写入开始前予以确认 |

## 退出状态

| 代码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 意外的内部错误 |
| 2 | 用法或命令语法错误 |
| 3 | 策略拒绝，或请求的前置条件不满足 |
| 4 | 本次运行没有写入目标字节，也没有创建、重命名、移走或删除记录条目（记录目录与 record.lock 不计入） |
| 5 | 部分完成：目标未完全写入期间记录发生了变化，或者已应用但仍有后续步骤待完成或内核视图陈旧（会打印补救办法） |
| 6 | 平台或能力不受支持 |