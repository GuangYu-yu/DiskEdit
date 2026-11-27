# chunk-E 处置报告：libfdisk / dosfstools / f2fs-tools sites（1,919 行）

范围：`05-sites-libfdisk/`（19 批，1,089 行）、`05-sites-dosfstools/`（18 批，373 行）、
`05-sites-f2fs-tools/`（14 批，457 行）。全部行已翻态，无"待判"残留；queue.tsv 未动。
含裁剪/差异行的 31 个批次均已创建同名 `.md`（判断/依据/撤回条件）。

判断口径：
- libfdisk 库内 C 防御断言/失败 goto/打印管道 → 拒绝（架构差异，census SA1-022 入账）；
  表操作校验（对齐/重叠/类型码/槽位/可用界）→ 认领引 crates/core/src/table/edit.rs、gpt/parse.rs
  与 SA1-003/005/006/007/008/010/011/012/013/014/015/018/027、SA3-002/003；
  CHS 类 → SA1-027/028 定案照引；wipe/签名 → SA1-005/006；脚本/JSON/dump → SA1-007/010；
  备份文件 → SA1-020；老盘标 → SA1-016；交互菜单/prompt/TUI → 裁剪。
- dosfstools/f2fs-tools 为依赖工具：固定参数调用（catalog.rs Support 表、program.rs），
  工具内部边界校验调用时生效 → 认领（SA2-010/012）；工具自身 CLI/usage/码页/坏块/显示面 → 裁剪（SA2-011）；
  工具内部断言/不变量 → 拒绝（架构差异-多虑）；f2fs check 不支持 → SA2-013。

## 一、待实现工作项（row_id | 行为 | 标签 | 根因 | 建议硬约束）

1. `libfdisk-00356` | `set flag boot` 前拒绝 MBR 扩展容器记录（参考 fdisk 对 extended 提示 "is an extended partition"） | 缺校验(签名/边界/容量) | `set_mbr_boot` 只查 `require_live_mbr`，未查 `MBR_CONTAINER_TYPES`，boot 旗标可落在扩展容器上 | crates/core/src/table/edit.rs `set_mbr_boot` 前置 `check_mbr_type_byte`；tests/case_runner.rs 补"容器记录 set flag boot 拒绝"单测
2. `dosfstools-00113` | vfat 标签边界（11 字符上限）在决策层前置校验，不依赖 fatlabel 事后失败 | 缺校验(签名/边界/容量) | `set label` 把文本直传 fatlabel，越界要到工具内才失败（失败虽在写前，但拒绝面晚且提示是工具原文） | src/app/decide/edit.rs `content_property` 前置长度校验 + tests/case_runner.rs 单测
3. `dosfstools-00118` | vfat 标签字符集（<0x20 控制字符、`*?.,;:/\|+=<>[]"` 等）决策层前置校验 | 缺校验(签名/边界/容量) | 同上，字符集校验同样缺位于判定层 | 同上落点；与 2 合并为一个校验函数

## 二、差异建议（编号请主控统一登记）

- **D-E1（verify 逐项诊断清单）**：fdisk verify 的逐项错误 + "N errors detected" 汇总未复刻；判定逻辑本身全部已认领（CRC/LBA sanity/MyLBA/overlap/too-big/end-before-start），呈现为 info/inspect 损伤类别与退出码。行：libfdisk-00479/00480、00288/00289（共 4 行）+ 同族呈现行。D-建议：如需人工逐项诊断，可在 info 损伤字段附明细列表。撤回条件：出现解析 verify 文本的外部契约。
- **D-E2（MBR/EBR 读侧宽容）**：参考对 EBR 链尾损坏、extra link/data、坏偏移警告并跳过；本实现整体读失败拒绝（更保守、不猜）。行：libfdisk-00225/00226/00228/00229/00244（6 行）。D-建议：保持拒绝语义并登记差异。撤回条件：出现必须容忍链尾损坏继续读的镜像契约。
- **D-E3（列式人读表格）**：fdisk/cfdisk scols 列式显示未复刻，结构化输出为 info JSON（等价性逐字段测试覆盖字段集）。行：libfdisk-00898、01002-01012 族、print_partition_info 等（8 行）。D-建议：如需人读列式显示可在 report 层加渲染，字段以 info JSON 为准。

另有观察（不落行态）：resize.f2fs 收缩未接线（catalog.rs shrink=Unsupported），f2fs-tools 1.16 具备部分收缩能力；与 SA2-014 的保守取舍同型，如需收缩先建真机用例。

## 三、拆分建议（同批两种结论，建议主控拆批）

1. `sites-libfdisk-mbr-assert`：91 拒绝（非空断言）+ 13 认领（EBR 链显式错误替代断言）→ `sites-libfdisk-mbr-assert-chain`。
2. `sites-libfdisk-table-edit-assert-disk-utils`：cfdisk TUI 裁剪 52 与 sfdisk 核心认领/拒绝 10 混批 → `sites-libfdisk-table-edit-assert-sfdisk`。
3. `sites-libfdisk-table-edit-refusal`：命令语义认领 91 与交互/老盘标/备份裁剪 21 混批 → `sites-libfdisk-table-edit-refusal-frontend`。
4. `sites-dosfstools-mkfs-fatal-exit`：FAT 结构校验认领 24 与 CLI 选项面裁剪 46 混批 → `sites-dosfstools-mkfs-fatal-exit-cli`。
5. `sites-f2fs-tools-check-string-channel`：测试驱动拒绝与运行输出认领混批（8 行，量小可不强拆）。

## 四、四态计数

| 工具 | 认领 | 拒绝 | 差异 | 裁剪 | 合计 |
| --- | --- | --- | --- | --- | --- |
| libfdisk（19 批） | 325 | 549 | 18 | 197 | 1,089 |
| dosfstools（18 批） | 119 | 20 | 0 | 234 | 373 |
| f2fs-tools（14 批） | 4 | 439 | 0 | 14 | 457 |
| **合计（51 批）** | **448** | **1,008** | **18** | **445** | **1,919** |

分批明细（认领/拒绝/差异/裁剪）：
- libfdisk：align-assert 0/5/0/0；align-fail-goto 1/0/0/0；gpt-assert 7/100/0/0；gpt-error-report 20/3/0/0；gpt-fail-goto 32/6/0/2；gpt-refusal 24/2/2/0；label-uuid-assert 0/10/0/0；label-uuid-refusal 0/1/0/0；mbr-assert 13/91/0/0；mbr-error-report 11/1/4/13；mbr-fail-goto 2/0/0/9；mbr-refusal 15/0/3/1；table-edit-assert-disk-utils 6/3/1/52；table-edit-assert-libfdisk 0/301/0/0；table-edit-error-report 16/20/1/25；table-edit-fail-goto 22/2/6/29；table-edit-fatal-exit 63/1/0/45；table-edit-refusal 91/2/1/21；table-edit-string-channel 2/1/0/0。
- dosfstools：check-error-report 0/0/0/20；check-fatal-exit 0/4/0/25；check-refusal 0/1/0/10；check-string-channel 1/0/0/12；frontend-error-report 9/0/0/2；frontend-fatal-exit 47/9/0/0；frontend-refusal 4/0/0/18；frontend-string-channel 0/0/0/7；label-uuid-error-report 7/0/0/1；label-uuid-fatal-exit 9/0/0/12；label-uuid-refusal 3/1/0/1；mkfs-error-report 3/0/0/31；mkfs-fatal-exit 24/5/0/46；mkfs-refusal 6/0/0/14；mkfs-string-channel 6/0/0/23；probe-error-report 0/0/0/1；probe-refusal 0/0/0/1；probe-string-channel 0/0/0/10。
- f2fs-tools：check-assert 0/299/0/0；check-error-report 0/2/0/1；check-fail-goto 0/62/0/0；check-fatal-exit 0/1/0/6；check-refusal 0/1/0/1；check-string-channel 3/5/0/0；misc-assert 0/12/0/0；misc-fail-goto 0/13/0/0；misc-refusal 0/1/0/0；mkfs-assert 0/2/0/0；mkfs-fail-goto 1/13/0/0；mkfs-fatal-exit 0/0/0/6；resize-assert 0/28/0/0。

## 五、同名 md（31 份）

`05-sites-libfdisk/`：gpt-fail-goto、gpt-refusal、mbr-error-report、mbr-fail-goto、mbr-refusal、
table-edit-assert-disk-utils、table-edit-error-report、table-edit-fail-goto、table-edit-fatal-exit、
table-edit-refusal（10 份）。
`05-sites-dosfstools/`：check-error-report、check-fatal-exit、check-refusal、check-string-channel、
frontend-error-report、frontend-refusal、frontend-string-channel、label-uuid-error-report、
label-uuid-fatal-exit、label-uuid-refusal、mkfs-error-report、mkfs-fatal-exit、mkfs-refusal、
mkfs-string-channel、probe-error-report、probe-refusal、probe-string-channel（17 份）。
`05-sites-f2fs-tools/`：check-error-report、check-fatal-exit、check-refusal、mkfs-fatal-exit（4 份）。

撤回条件汇总：交互面 → 引入交互式界面或出现依赖 prompt 的契约时按 ask 面重建；老盘标 → SA1-016 条件；
备份文件 → SA1-020 条件；CHS → SA1-028 条件；脚本/类型清单 → SA1-010 条件（先定义冲突处理与身份校验）；
dosfstools CLI/码页/坏块/显示面 → 固定参数不足以覆盖合法用法或上游改参时重评；交互修复 → 决定提供
vfat 修复写路径时先建真机用例；f2fs 子工具面 → 出现相应契约时先建真机用例再纳入档案表；
差异三类 → 见上文各行撤回条件。
