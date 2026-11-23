# 能力基线取数

基线 = 仓库根目录现有实现的对外行为，源码是唯一事实来源。本目录只记录从源码声明、分支与数据表读出的结论，并附源码位置；不运行二进制。

## 产物清单

| 文件 | 内容 |
| --- | --- |
| `commands.md` | 16 个入口的命令面：形态、位置槽、选项、默认值、值域、互斥、确认、目标后缀 |
| `capability-matrix.md` | 文件系统能力矩阵的逐格结论与源码位置（T0 冻结，无待确认项） |
| `judgment-lines.md` | 人类可读文本里的判定行（退出码语义行、恢复三要素）与拒绝成因的固定下一步 |
| `json/README.md` | 机器可读输出的字段来源（结构体符号与位置），字段级契约 |
| `images.md` | 固定镜像配方：每种表、文件系统、目标形态各至少一个 |
| `divergences.md` | 差异登记骨架，条目与 `01-目标与边界.md` 第 5 节 D1–D8 对应 |

## 取数来源（源码位置）

| 事实 | 源码位置 |
| --- | --- |
| 命令声明表 | `src/cli.rs` `COMMANDS`（L634–L1044）、`Variant`/`CommandDef`/`OptionDef`/`Selector`/`TargetSuffix`（L512–L632） |
| 位置槽 | `src/cli.rs` `Slot`/`PositionalDef`（L438–L491） |
| 选项旗标与 requires | `src/cli.rs` `FlagSpec`（L195–L369） |
| 值域 | `src/cli.rs` `ValueDomain`（L38–L67） |
| 目标后缀解析 | `src/dev/target.rs`（L5–L52） |
| 退出码常量 | `src/outcome.rs`（L20–L36） |
| 退出码唯一映射 | `src/outcome.rs` `RunOutcome::exit_code`（L667–L680）、`is_complete`（L654–L665） |
| 文件系统识别集 | `src/domain.rs` `FsKind`（L153–L216）、`ContentKind`（L229–L241） |
| 扩容/缩容能力 | `src/fsops/capabilities.rs` `grow_support`/`shrink_support`（L32–L90） |
| mkfs 工具映射 | `src/fsops/mkfs.rs` `mkfs_tool`（L117–L140） |
| 记录头/帧/恢复判定 | `src/recovery/record.rs` `RecoveryHeader`（L54–L60）、`Record`（L614–L666）、`completion_verdict`（L731–L745）、`decode`（L1295） |
| 载体入口 | `src/exec/*.rs` |

取数用一次性只读命令（不入库），例如：

```
grep -n "name:" src/cli.rs
grep -n "EXIT_" src/outcome.rs
grep -n "fn grow_support\|fn shrink_support" src/fsops/capabilities.rs
```

## 未从静态读出的项

以下需要在有镜像/特权的环境运行取数，记「未执行」，由 `rebuild/tests` 的比对器执行：

- 各命令 `--json` 的实跑样本（字段级契约已从结构体读出，见 `json/README.md`）。
- 盘面效果与声明区间之外的字节摘要。
- 载体路径的实际执行。
