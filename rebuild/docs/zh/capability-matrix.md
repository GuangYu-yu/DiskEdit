# diskedit 能力矩阵

由 `src/docs.rs` 从 `CommandDef.capabilities` 与 `CapabilitySnapshot::design(platform, media)` 生成。
用 `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit docs_are_generated_in_sync` 重新生成；
CI 会重新生成，并在工作树不一致时失败。不要手工编辑。

## 能力状态

| 能力 | linux / 块设备 | linux / 镜像 | windows / 块设备 | windows / 镜像 | 其他 / 块设备 | 其他 / 镜像 |
|---|---|---|---|---|---|---|
| ReadTarget | 支持 | 支持 | 不支持 | 支持 | 不支持 | 支持 |
| WriteTable | 支持 | 支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| SectorIo | 支持 | 支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| ExternalTool | 支持 | 支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| KernelResync | 支持 | 不适用 | 不支持 | 不适用 | 不支持 | 不适用 |
| ExclusiveLock | 支持 | 支持 | 设计限制 | 设计限制 | 不支持 | 不支持 |
| AtomicReplace | 支持 | 支持 | 不支持 | 不支持 | 不支持 | 不支持 |
| DirPersist | 支持 | 支持 | 设计限制 | 设计限制 | 不支持 | 不支持 |
| StableIdentity | 支持 | 支持 | 不支持 | 支持 | 不支持 | 支持 |

## 命令

| 命令 | linux / 块设备 | linux / 镜像 | windows / 块设备 | windows / 镜像 | 其他 / 块设备 | 其他 / 镜像 |
|---|---|---|---|---|---|---|
| `info` | 允许 | 允许 | 阻止：ReadTarget（不支持） | 允许 | 阻止：ReadTarget（不支持） | 允许 |
| `inspect` | 允许 | 允许 | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） |
| `records list` | 允许 | 允许 | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） |
| `records verify` | 允许 | 允许 | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） |
| `records scan` | 允许 | 允许 | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） | 阻止：ReadTarget（不支持） | 阻止：AtomicReplace（不支持） |
| `resize` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `move` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `copy` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `delete` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `set` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `check` | 允许 | 允许 | 阻止：ReadTarget（不支持） | 阻止：ExternalTool（不支持） | 阻止：ReadTarget（不支持） | 阻止：ExternalTool（不支持） |
| `mkfs` | 允许 | 允许 | 阻止：AtomicReplace（不支持） | 阻止：AtomicReplace（不支持） | 阻止：AtomicReplace（不支持） | 阻止：AtomicReplace（不支持） |
| `resizefs` | 允许 | 允许 | 阻止：AtomicReplace（不支持） | 阻止：AtomicReplace（不支持） | 阻止：AtomicReplace（不支持） | 阻止：AtomicReplace（不支持） |
| `recover` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `new` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |
| `add` | 允许 | 允许 | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） | 阻止：WriteTable（不支持） |

`admitted` 表示能力闸门对该形态没有报告缺口。不带 WWN、EUI 或可重复序列号的
块设备（nbd、rbd、dm、md）在此仍记为允许，但它的写命令需要 `--identity NAME`；
续跑或释放该记录需要同一个名字，只读的记录命令（`inspect`、`records list`、
`records verify`）也接受这个名字来定位与核对记录。

## 运行时闸门

闸门只评估本次构建能打开的目标形态。本表列出它在 Linux 上评估的 2 种目标形态（`HostPlatform::openable_media`）：linux / 块设备, linux / 镜像。其他形态不进入闸门的决策——`cmd::capability_gap` 只遍历可打开集合。
该表由 Linux 能力集生成；非 Linux 构建的可打开集合在运行时由 `HostPlatform::openable_media` 决定，此处不复制它。

| 命令 | linux / 块设备 | linux / 镜像 |
|---|---|---|
| `info` | 允许 | 允许 |
| `inspect` | 允许 | 允许 |
| `records list` | 允许 | 允许 |
| `records verify` | 允许 | 允许 |
| `records scan` | 允许 | 允许 |
| `resize` | 允许 | 允许 |
| `move` | 允许 | 允许 |
| `copy` | 允许 | 允许 |
| `delete` | 允许 | 允许 |
| `set` | 允许 | 允许 |
| `check` | 允许 | 允许 |
| `mkfs` | 允许 | 允许 |
| `resizefs` | 允许 | 允许 |
| `recover` | 允许 | 允许 |
| `new` | 允许 | 允许 |
| `add` | 允许 | 允许 |

进程级退出会打印失败的原因句。退出码 3 与退出码 4 之后还有第二行 `— this run changed no persistent state`；退出码 6 打印 `unsupported: <cause>`，不追加该行。