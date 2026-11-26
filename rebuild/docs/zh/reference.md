本文件是 `docs/en/reference.md` 的中文译本，不参与生成物逐字比对；命令集合由 `tests/generated_docs.rs` 核对，新增命令必须同步补这里。

# diskedit 命令参考

对不带 WWN、EUI 或可重复序列号的块设备（nbd、rbd、dm、md），写命令必须给出
`--identity NAME`；镜像与自带身份的设备拒绝该旗标。

## 命令

### `info`

显示目标与分区概况

```text
diskedit info <TARGET> [--sector-size N]
diskedit info <TARGET>:N --grow [--sector-size N]
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 带或不带后缀 | 无 | `--sector-size`, `--grow` |

### `inspect`

逐分区显示起止与内容

```text
diskedit inspect <TARGET>
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 不带分区后缀 | 无 | `--identity` |

### `records list`

列出记录目录条目

```text
diskedit records list [TARGET]
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 无目标 | 无 | `--identity` |

### `records verify`

校验一条记录

```text
diskedit records verify <TARGET> <ID>
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 不带分区后缀 | 无 | `--identity` |

### `records scan`

扫描记录目录

```text
diskedit records scan <ROOT>
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 不带分区后缀 | 无 | — |

### `resize`

改变分区大小

```text
diskedit resize <TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes
diskedit resize <TARGET>:N --end LBA [--align A] [--no-fs] --yes
diskedit resize <TARGET>:N --grow-to-end [--align A] [--no-fs] --yes
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 带或不带后缀 | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--allow-move`, `--chunk`, `--grow-lv`, `--lv`, `--no-fs`, `--dry-run`, `--end`, `--grow-to-end`, `--align` |

### `move`

平移一个分区

```text
diskedit move <TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 必须带 `:N` 或 `:last` | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--start`, `--align`, `--chunk` |

### `copy`

复制分区到新位置并新增分区

```text
diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 必须带 `:N` 或 `:last` | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--start`, `--align`, `--chunk`, `--name`, `--keep-partuuid` |

### `delete`

删除一个分区

```text
diskedit delete <TARGET>:N --yes
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 必须带 `:N` 或 `:last` | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity` |

### `set`

修改分区或文件系统属性

```text
diskedit set <TARGET>:N name S --yes
diskedit set <TARGET>:N label S --yes
diskedit set <TARGET>:N uuid [U] --yes [--random]
diskedit set <TARGET>:N flag F on|off --yes
diskedit set <TARGET>:N type GUID|0xXX --yes
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 必须带 `:N` 或 `:last` | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--random` |

### `check`

检查一个分区的文件系统

```text
diskedit check <TARGET>:N
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 必须带 `:N` 或 `:last` | 无 | `--sector-size` |

### `mkfs`

在分区上建卷

```text
diskedit mkfs <TARGET>:N <FS> --yes [--label S]
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 必须带 `:N` 或 `:last` | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--label` |

### `resizefs`

调整分区内文件系统大小

```text
diskedit resizefs <TARGET>:N [BYTES] --yes
diskedit resizefs <MOUNTPOINT> [BYTES] --yes
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 带或不带后缀 | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity` |

### `recover`

回滚、重放或放弃记录

```text
diskedit recover <TARGET> --yes
diskedit recover <TARGET> --replay-table-only --yes
diskedit recover <TARGET> --release [--discard-replay] --yes
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 不带分区后缀 | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--replay-table-only`, `--release`, `--discard-replay` |

## 底层命令

### `new`

新建分区表

```text
diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 不带分区后缀 | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--table`, `--overwrite-data` |

### `add`

新增一个分区

```text
diskedit add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes
```

| 属性 | 取值 | 选项 |
|---|---|---|
| 不带分区后缀 | 必须（`--yes`） | `--sector-size`, `--yes`, `--identity`, `--start`, `--align`, `--part-number`, `--name`, `--type-guid` |

## 选项

| 选项 | 取值 | 说明 |
|---|---|---|
| `--align` | `A` | 放置对齐：mib|none |
| `--allow-move` | — | 打印计划后搬移挡路的分区 |
| `--chunk` | `MiB` | 恢复粒度，单位 MiB，1..=64；新任务默认 4 |
| `--discard-replay` | — | 确认放弃 `recover` 仍可回放的区间 |
| `--dry-run` | — | 打印计划并在第一次写入前停止；不需要 --yes |
| `--end` | `LBA` | 就地扩容或收缩的新结束 LBA |
| `--grow` | — | 追加分区 :N 的搬移预览 |
| `--grow-lv` | — | 同时扩容 LV：由 --lv 指定的一个，否则为唯一的 LV |
| `--grow-to-end` | — | 把末端固定在最后一个可用 LBA（填充语义） |
| `--identity` | `NAME` | 为不带 WWN、EUI 或可重复序列号的目标（nbd、rbd、dm、md）命名 |
| `--keep-partuuid` | — | 复用源的 PARTUUID，而不是生成新的 |
| `--label` | `S` | 文件系统标签（省略即空标签，不是默认值） |
| `--lv` | `NAME` | 要扩容的 LV（隐含 --grow-lv） |
| `--name` | `S` | 该命令创建的条目名 |
| `--no-fs` | — | 跳过扩容分区的文件系统步骤 |
| `--overwrite-data` | — | 接受覆盖既有分区所占据的字节 |
| `--part-number` | `N` | 条目槽位，从 1 起；必须为空 |
| `--random` | — | 要求目标生成新值 |
| `--release` | — | 丢弃恢复文件，保留它们所描述的写入 |
| `--replay-table-only` | — | 只回放一次数据已搬移完成的移动操作所记录的表区域 |
| `--sector-size` | `N` | raw 镜像的逻辑扇区大小覆盖值（2 的幂，最大 65536 字节） |
| `--start` | `LBA|end|align` | 新起点：一个 LBA，`end` 表示尾部贴合，或 `align` 表示下一个 1 MiB 边界 |
| `--table` | `gpt|mbr` | 分区表类型：gpt（默认）或 mbr |
| `--type-guid` | `GUID|0xNN` | GPT 类型 GUID 文本，或 MBR 类型字节 0xNN |
| `--yes` | — | 写入开始前予以确认 |

