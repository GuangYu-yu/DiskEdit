# 参考工具账本（T1/T2/T3/T4）

本目录是“向同类工具挖账本”的产出：把参考工具里所有与 DiskEdit 对外行为有关的
防御点、拒绝点、隐式决定、文档告示、历史事故与规范条款，逐条枚举成可 grep、可
勾销的清单。**本阶段只枚举，不处置**：每一行的 `disposition` 都是 `待判`，下一
阶段再顺着账本逐行判定“认领/拒绝/差异/裁剪”。

产品代码未改动。唯一的可执行文件是 `mine_reference.py`，它只读参考源码、只写本
目录，用于重放本账本。

## 1. 语料与版本

版本、上游 URL、SHA-256、覆盖文件数与范围见 [sources.tsv](sources.tsv)。固定
版本优先对齐 QEMU 来宾发行版；gdisk 用可复现的 1.0.10（旧 `defense-census`
记的是不可复现的 `master-2026-11-27`，见第 6 节）。

| 层 | 工具 | 版本 |
| --- | --- | --- |
| T1 | GNU parted / libparted、util-linux sfdisk+libfdisk、gdisk/sgdisk/cgdisk/fixparts | 3.6 / 2.39.3 / 1.0.10 |
| T2 | e2fsprogs、xfsprogs、btrfs-progs、ntfs-3g、f2fs-tools、dosfstools、LVM2 | 1.47.0 / 6.6.0 / 6.6.3 / 2022.10.3 / 1.16.0 / 4.2 / 2.03.16 |
| T3 | testdisk | 7.2 |
| T4 | UEFI 规范第 5 章、Microsoft MBR/EBR 文档 | 2.10 / Win2000-2003 |

T4 的条款文本摘录在 [specs.tsv](specs.tsv)，来源 URL 见
[sources.tsv](sources.tsv)。

## 2. 口径（什么算一个“站点”）

对 `sources.tsv` 里列出的代码文件逐行扫描，命中任一类即记一行：

| 类别 | 含义 | 识别方式（摘要） |
| --- | --- | --- |
| `assert` | 断言 | `PED_ASSERT`/`assert`/`ASSERT`/`g_assert`/`Q_ASSERT`/`BUG_ON` |
| `exception` | 异常/抛出 | `ped_exception_throw`/`throw` |
| `abort` | 中止/panic | `abort(`/`panic(` |
| `fail-goto` | 失败清理跳转 | `goto fail* / error* / err* / out* / done* / cleanup*` |
| `fatal-exit` | 致命退出/用法 | `err(`/`errx(`/`die(`/`fatal(`/`Exit(`/`exit(`/`_exit(`/`usage(`（`exit(0)` 除外） |
| `refusal` | 带拒绝语义的用户消息 | 错误通道 + 三行窗口内出现 cannot/invalid/unsupported/too small/exceed/overlap/corrupt/truncate/mounted/busy 等 |
| `error-report` | 错误通道但不含拒绝词 | `err/errx/warn/warnx/die/perror/fprintf(stderr)/fputs(stderr)/cerr<<`/`log_error|warn|critical`/`display_message`/`screen_buffer_add` |

同一行命中多类时只记一行，`category` 取优先级最高者，全部命中写进 `tags`。
`symbol` 是“最近一个函数样式行”的近似归属，不是编译器级符号解析。

另外三类不是逐行代码站点，但同样是“可继续借鉴”的清单：

- [docs.tsv](docs.tsv)：man/texi/rst 里的 `CAVEATS/BUGS/WARNING/NOTES/SECURITY/LIMITATIONS/KNOWN BUGS/KNOWN ISSUES/PRACTICAL USE` 段落；texi 另收含 warning/data loss/corrupt/unrecoverable 的行。按空行/宏/标题分块，一块一行。
- [incidents.tsv](incidents.tsv)：NEWS/ChangeLog/RELEASE-NOTES/WHATS_NEW 里含 crash/corrupt/data loss/CVE/overflow/truncate/misalign/wrong/race 等风险词的分块。
- [implicit.tsv](implicit.tsv)：`#define` 的 ALIGN/DEFAULT/MAX/MIN/SECTOR/MIB/CHUNK/TIMEOUT/RETRY/CONFIRM/FIRST/LAST，以及 `fsync|fdatasync|sync_file_range`、`BLKRRPART|BLKPG`、`O_EXCL|O_DIRECT|flock|F_SETLK`、`confirm|yes_no|ask_yes|interactive|force`。

## 3. 规模

| 文件 | 行数 |
| --- | --- |
| `sites/*.tsv` | 8,032 个站点 |
| `docs.tsv` | 278 段 |
| `incidents.tsv` | 750 条事故候选 |
| `implicit.tsv` | 542 条隐式决定候选 |
| `specs.tsv` | 29 条规范条款 |

按工具与类别的精确计数见 [summary.tsv](summary.tsv)。风险分布：站点 high 5,713 /
medium 2,319；文档 high 9 / medium 269；事故 high 379 / medium 371；隐式决定全
medium 542。`risk` 是启发式分级，只用于排序，不是处置结论。

## 4. 列定义

`sites/<tool>.tsv`：

| 列 | 含义 |
| --- | --- |
| `site_id` | 稳定 ID：`<tool>-<序号>`，按文件/行排序生成 |
| `tool` / `version` / `layer` | 工具、固定版本、参考层 |
| `area` | gpt/mbr/table-edit/align/probe/resize/mkfs/check/label-uuid/swap/lvm/recovery/frontend/misc |
| `category` / `tags` | 主类别 / 全部命中类别 |
| `file` / `line` / `symbol` | 相对路径、1 起行号、近似函数名 |
| `finding` | 原始上下文（最多 3 行，240 字符） |
| `risk` | high/medium/low 启发式 |
| `disposition` | 本阶段固定 `待判` |
| `note` | 预留给处置理由 |

`docs.tsv` 用 `doc_id` 与 `section`；`incidents.tsv` 用 `incident_id` + `release`；
`implicit.tsv` 用 `implicit_id` + `kind`；`specs.tsv` 用 `spec_id` + `section` +
`clause`。

## 5. 与旧 `defense-census.tsv` 的对账

旧 census 只数“断言/异常/失败 goto”三类，且 gdisk 为 0。本账本在同一窄口径上
复现了旧数字，同时补上了它看不见的站点：

- parted：libparted 核心 **816 / 326 / 487**，与旧行完全一致；前端 `parted/*.c`
  另加 **39 / 24 / 83**，合计 855 / 350 / 570。
- libfdisk：核心 **527 / 0 / 84**，与旧行完全一致；`sfdisk.c`/`fdisk.c`/`cfdisk.c`
  另加 **62 / 0 / 27**，合计 589 / 0 / 111。
- gdisk：1.0.10 在窄口径下仍是 **0 / 0 / 0**；但本账本录得 **180** 个站点
  （29 fatal-exit、118 error-report、33 refusal）。这说明旧的 0 是**度量口径
  盲区**，不是“没有防御面”。

因此旧 `defense-census.tsv` 的 gdisk 行不应再作为“已覆盖”的证据；后续处置应
以 `sites/gdisk.tsv` 为准。

## 6. 明确的盲区（下一阶段才补）

1. **只做了源码枚举，没有黑盒实测。** 组合行为、真实工具版本差异、失败后的半写
   状态仍要靠 QEMU/真机会话（对应原计划 SA3）。
2. **T2 只覆盖 DiskEdit 调用的子命令与支持路径**，不是整个库；内核侧 e2fs/xfs/
   btrfs/ntfs 行为不在本账本。
3. **没有跨版本矩阵**：每个工具只固定一个版本。
4. **gdisk 版本与旧 census 不同**（1.0.10 vs master-2026-11-27），已在上文说明。
5. **文档只收特定章节**：没有这些章节的工具（如 testdisk、部分 f2fs）计 0 是
   事实，不是漏扫；man 正文（DESCRIPTION/OPTIONS）未逐句收。
6. **事故只收风险词命中的分块**，不是完整 changelog。
7. **EBR 链算法未取到完整规范**：Microsoft 文档只给了存在性、签名与放置规则；
   链式指针字段与遍历算法需要在下一阶段用源码/黑盒补齐。
8. **规范只覆盖 UEFI 2.10 第 5 章 + MS MBR/EBR**；ATA/SCSI 只取到对齐相关段落。
9. `symbol` 是启发式近似；精确到函数需编译器/ctags 级解析。

## 7. 重放

参考源码解包到 `/tmp/refsrc/src`（版本见 `sources.tsv`），然后：

```sh
REF_SRC=/tmp/refsrc/src python3 plan/baseline/reference/mine_reference.py
```

脚本只写本目录，不触碰 `src/`、`plan/gates.tsv`、`plan/instruments/` 或
`plan/census.json`。重放后可用以下命令核对规模：

```sh
awk -F'\t' 'FNR>1{n++} END{print n}' plan/baseline/reference/sites/*.tsv   # 8032
awk -F'\t' 'NR>1{n++} END{print n}' plan/baseline/reference/docs.tsv      # 278
```

## 8. 下一阶段（处置，不在本轮）

逐行把 `disposition` 从 `待判` 改成以下四态之一，并在 `note` 写理由与证据：

- `认领`：DiskEdit 有等价对外行为，且有测试锚点（covers/用例/QEMU 场景）。
- `拒绝`：DiskEdit 明确拒绝，退出码与旧版对齐，且有用例。
- `差异`：有意不同，登记到 `plan/divergences.md` 的 D 表并带撤回条件。
- `裁剪`：明确不覆盖，按 `plan/04` 的六项登记（范围/影响/替代/复核/触发/责任人）。

建议顺序：先 `gpt`/`mbr`/`table-edit` 的 high 风险写路径，再 `resize`、`label-uuid`、
`mkfs`，最后 `swap`/`lvm`/`recovery` 与 T4 条款。每批按原计划的 ≤20 类上限收口。
