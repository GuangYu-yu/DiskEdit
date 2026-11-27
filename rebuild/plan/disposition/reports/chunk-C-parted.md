# chunk-C-parted 处置报告（05-sites-parted/，35 批 2025 行）

批次：35 个 tsv 全部翻态，行原文 `plan/baseline/reference/sites/parted.tsv`（parted 3.6/libparted/libparted-3.6 前端，T1 防御面账本）。

## 四态计数

| 态 | 行数 | 说明 |
| --- | --- | --- |
| 认领 | 217 | 其中 1 行标 `待实现`（见下）；其余引 SA / refusal-map R 编号 / 代码路径 / 测试锚 |
| 拒绝 | 697 | 断言/异常/失败 goto 的防御类（Rust 所有权/类型/Result 承担，SA1-021 census）；老古董 C 细节（i18n、stdout 刷新、崩溃诊断、进度 timer、信号告示） |
| 裁剪 | 1111 | SA1-016 老盘标、SA1-017 未登记族/识别精度层、SA1-028 CHS、SA2-014 FAT 扩缩容、交互 prompt 面、MBR 逻辑分区写侧、坏块扫描、非 Linux 平台后端 |
| 差异 | 0 | 无 D-建议行；边界情形（对齐告示牌、align-check、引导器/fstab 提示、parted 已移除 resize）按"有意不采纳"判拒绝并在 note 说明理由 |

含裁剪行的 26 个批次已各建同名 md（判断/依据/撤回条件）。

## 待实现工作项（1 项）

```
parted-01412 | GPT 头 revision 高于 0x00010200（如 1.03+）时拒绝识别该副本，不读未知版本结构 | 缺校验(签名/边界/容量) | crates/core/src/table/gpt/header.rs 只读入 revision、parse.rs 仅在主备副本间比较相等，无版本上限校验，未知 revision 可能被误读 | 建议硬约束：parse 层对 revision>0x00010200 返回 UnsupportedRevision 类错误并加单测（tests/case_runner.rs）
```

## 提示面结论（无缺失工作项，均已有归属或有意不采纳）

- 已认领的提示面：错误打印（SA1-009 + refusal-map）、用法提示（R0046/R0078）、未知盘标/打不开（R0132/R0143）、占用闸（SA1-004）、双副本损坏（R0131）、损坏回退（SA3-003）、TableFull/重叠/越界（edit.rs/R0168/R0182）。
- 有意不采纳（拒绝，理由入 note）：parted 的对齐性能告示牌与 align-check 输出（对齐由构造保证，SA1-008/SA1-014）、"重装引导器/更新 fstab"提示（依赖猜测用户环境，违背不猜边界）、断言崩溃告示牌（SA1-021，Rust panic 承担）。

## 差异建议

- 无需新登记的 divergence。parted 3.x 已移除 `resize` 命令而本实现按 sfdisk 语义提供 resize（SA1-004/013/014），属既定范围选择，已在 note 引 SA，不另立差异行。

## 拆分建议

1. `sites-parted-label-uuid-*` 五批混装 GPT/dos（在范围内）与 12 类老盘标（裁剪）：建议按盘标归属拆为 `label-uuid-gpt`、`label-uuid-dos`、`label-uuid-old`（老盘标整批裁剪），主控重算时行可平移。
2. `sites-parted-frontend-fail-goto-parted` / `frontend-exception-parted` 混有交互命令体（裁剪）与 parted 已有非交互等价的拒绝行（认领）：建议把交互命令体聚为 `frontend-parted-interactive`。
3. `fs/r/fat`（SA2-014 裁剪）与 `fs/fat`（探测，拒绝）散布在 frontend/assert/resize 三处：建议聚合为 `sites-parted-fs-r-fat` 单批统一按 SA2-014 处置。

## 备注

- 判断口径：防御类站点先看是否纯 C 生命周期/空指针（拒绝，SA1-021）；绑定真实盘面校验的查实现（认领引代码路径/SA/R）；绑定已裁剪特性的引 SA1-016/017/028、SA2-014 或 edit.rs 代码路径（裁剪）；逐行结论与依据见各批次 tsv 的 state+note，撤回条件见各批次同名 md。
- 未改动 queue.tsv（由主控统一重算）；未改任何代码。
