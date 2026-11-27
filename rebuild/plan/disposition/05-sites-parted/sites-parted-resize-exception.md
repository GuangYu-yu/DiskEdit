# sites-parted-resize-exception 处置判断

## 判断

- 本批 6 行（parted 3.6 防御面站点账本）。四态计数：认领 0 / 拒绝 0 / 裁剪 6 / 差异 0。
- 断言/异常/失败 goto 的防御类站点：C 空指针与生命周期断言、分配失败清理 goto 由 Rust 所有权/类型/Result 承担，判拒绝（架构差异-多虑）；防御面计数引 SA1-021（census:parted）。
- 错误提示/提示面类站点：refusal-map 已有对应拒绝面的判认领并引 R 编号或 SA；绑定交互 prompt 的判裁剪（本实现恒非交互）。
- 老古董 C 实现细节（i18n/iconv/gettext、stdout 刷新、崩溃诊断脚手架、进度 timer）：判拒绝，理由 Rust 架构差异。
- 涉及真实盘面校验且本实现缺失的判认领+待实现（本批如有，见 note 中「待实现」标记）。

## 裁剪依据

- **SA2-014**：FAT（vfat）扩缩容：档案表按不支持拒绝，不接 fatresize。

## 依据

- 权威依据：plan/baseline/incident-classes.tsv（SA1-016/017/020/021/025/027/028、SA2-012/014、SA3-003）、plan/baseline/refusal-map.tsv（R 行）、plan/06-防漂移执行手册.md（不猜边界）。
- 参考行原文：plan/baseline/reference/sites/parted.tsv（site_id 索引）。
- 新实现事实源：crates/core/src/table（mbr/gpt）、crates/core/src/fs/catalog.rs（档案表）、crates/core/src/geometry、src/app/observe（R0131/R0132/R0134/R0143）、src/app/execute（filesystem.rs/write.rs/transaction.rs）、src/app/report/exit.rs（SA1-009）、src/platform/linux（SA1-013/SA1-025）。

## 撤回条件

- SA2-014（FAT 扩缩容）：上游 fatresize 修复分区设备视图断言、能证明 -s 语义安全后重新评估。

## 差异与拆分

- 本批无差异（D-建议）行；结论混合时拆分建议见 plan/disposition/reports/chunk-C-parted.md。
