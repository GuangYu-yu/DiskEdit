# sites-parted-frontend-error-report

## 判断

- 本批 12 行（parted 3.8 防御面站点账本）。四态计数：认领 3 / 拒绝 8 / 裁剪 1 / 差异 0。
- 沿承行（00285/00557/00558/00559/00561/00562/01958/01991/01997/02005/02006）沿用迁移时 note（拒绝：调试打印/崩溃脚手架/异常处理器内部细节，SA1-021；认领：报告层单源与用法提示面，SA1-009 + refusal-map）。
- 换版重放新增 1 行（02011 parted/ui.c --fix 自动应答 PED_EXCEPTION_FIX 并打印 "Fixing, due to --fix"，3.8 由 printf 改为 stderr），判**裁剪**：绑定交互异常 prompt 应答机制，本实现恒非交互；修复动作由显式 repair 命令与表写承担（SA1-001/SA1-018、SA3-003）。对照 refusal-map：非拒绝面，无对应 R 行。

## 依据

- 权威依据：plan/baseline/incident-classes.tsv（SA1-001/009/018/021、SA3-003）、plan/baseline/refusal-map.tsv、plan/06-防漂移执行手册.md。
- 参考行原文：plan/baseline/reference/sites/parted.tsv；上游源码对照 /tmp/refsrc/parted-3.8 与 /tmp/src/parted-3.6。
- 新实现事实源：src/app/report/exit.rs、src/app/decide/repair.rs、src/app/execute/recover。

## 撤回条件

- 02011：引入交互异常应答面（含脚本模式等价性定义）时重审；修复动作改为隐式自动修复的外部契约出现时重评。
- 沿承行：其 note 引用的 SA/R 条目撤回条件。
