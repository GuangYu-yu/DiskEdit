# CLI 面终态重造：一批改完零残留

## Summary

按已收敛的终态设计，在本分支（test 线）一批完成 CLI 面改名与合一，旧词零残留、文档零假词；三条命名法成为 audit 可机检条款；docsgen 交叉 lint 补文档漂移洞；每个面变更各登记一条 D（撤回条件 = 首个外部用户）；R 号重锚随批走，由 audit.py 现有校验兜底。

## 终态命令面（定案，不再讨论）

```
读几何：   layout（--json 结构化明细；原 info + inspect 的逐分区明细合一）
读日志：   records list [--root ROOT] | records verify | records show（原 inspect 的记录现场呈现）
日志操作：  undo / replay / release（原 recover 三形态；--discard-replay 旗标名不变，随 release）
写面：     resize 合一（fs 轴：默认 盘+fs / --no-fs 仅分区 / --fs-only 仅文件系统=原 resizefs / 挂载点在线形式）
身份：     --alias（原 --identity）
预览：     info --grow 退役 → add --dry-run（落位判定 add 现成，补输出通道）
```

## Current State（已核实的落点）

- 命令注册表：[src/app/cli/declare.rs](file:///workspace/rebuild/src/app/cli/declare.rs)（569 行，15 命令单源：info/inspect/records list/verify/scan/resize/move/copy/delete/set/check/mkfs/resizefs/recover/new/add）；
- 解析：[src/app/cli/parse.rs](file:///workspace/rebuild/src/app/cli/parse.rs)（1084 行，`--grow` 强制 `:N`、Effect 映射、`RecoverRequest.replay_table_only` 等）；
- 命令实现分层：observe（读：[observe/records.rs](file:///workspace/rebuild/src/app/observe/records.rs) 等）、decide（[decide/recover.rs](file:///workspace/rebuild/src/app/decide/recover.rs)、resizefs.rs 等）、execute（[execute/recover/](file:///workspace/rebuild/src/app/execute/recover) 目录、resize.rs）；
- 文档生成：[src/docsgen.rs](file:///workspace/rebuild/src/docsgen.rs) 生成 docs/en/reference.md 等，[tests/generated_docs.rs](file:///workspace/rebuild/tests/generated_docs.rs) 逐字比对——生成链已有，交叉 lint 是补 prose 洞；
- 守卫测试：[tests/contract_surface.rs](file:///workspace/rebuild/tests/contract_surface.rs)（面枚举）、[tests/residue.rs](file:///workspace/rebuild/tests/residue.rs)（静态禁词）、[tests/refusal_map.rs](file:///workspace/rebuild/tests/refusal_map.rs)、[tests/case_runner.rs](file:///workspace/rebuild/tests/case_runner.rs)；
- 账本：refusal-map.tsv 约 197 行引用命令名（旧列引旧二进制原文**不改**；新列引本实现消息与文件路径的**随批改**）；divergences.md 现在 D1–D19；
- 仪器：[plan/instruments/audit.py](file:///workspace/rebuild/plan/instruments/audit.py)。

## Proposed Changes（执行顺序即依赖顺序）

### 第 1 步：declare.rs + parse.rs——注册表与请求层

- `recover` 拆三个命令：`undo`（回放被覆盖原字节）、`replay`（补完已搬移作业的表区写入，承接 `replay_table_only` 语义，**不再需要旗标**——它升格为独立动词）、`release`（保留写入清记录，`--discard-replay` 随行）；
- Request 结构：`RecoverRequest` 拆 `UndoRequest` / `ReplayRequest` / `ReleaseRequest`；`InfoRequest`+`InspectRequest` 合为 `LayoutRequest`（加 `json: bool`）；`ResizefsRequest` 并入 `ResizeRequest`（加 `fs_only: bool`、挂载点在线变体）；全部 `identity: Option<String>` 字段改名 `alias`；
- `records scan` 变体并入 `records list`（加 `--root ROOT` 选项）；`add` 加 `--dry-run`；
- macro `impl_identity_request!` → `impl_alias_request!`；`--identity` 值域文案改 `--alias`。
- **域概念词保留**：`recovery`（记录/现场）、identity（身份三态域模型）、`DeclaredIdentity` 等内部名不动——它们是概念不是面词；残留扫描只对面词 token。

### 第 2 步：命令实现层随动

- `decide/recover.rs` → 拆 `decide/{undo,replay,release}.rs`；`execute/recover/` → 拆 `execute/{undo,replay}.rs` + 记录文件操作归 `execute/release.rs`（release 不写目标字节）；
- observe：`observe/table.rs`（info 实现）+ inspect 的分区明细部分合入 `observe/layout.rs`；inspect 的记录现场呈现并入 `observe/records.rs`（`records show`）；`observe/new.rs` 不动；add 预览输出通道加在 add 的 observe/decide 路径；
- resize 合一：`decide/resize.rs` 吸收 `decide/resizefs.rs`（fs_only 分支 + 挂载点在线分支），`execute/resize.rs` 同步；`--no-fs` 与 `--fs-only` 互斥校验进 decide；
- 所有用户可见消息里的旧词替换：`diskedit recover --release` → `diskedit release`、`diskedit inspect` → `diskedit records show`、`diskedit info` → `diskedit layout`（refusal-map 引用这些串的行同步，见第 5 步）。

### 第 3 步：docsgen + 文档

- `docsgen.rs` 随新 COMMANDS 再生（reference.md / capability-matrix / exit-codes）；`tests/generated_docs.rs` 逐字比对会强制再生，不允许手改生成物；
- **交叉 lint 扩展**：docsgen 增加 prose 校验——生成文档中每个命令的一句话描述必须来自 `declare.rs` summary（不得在别处手写第二份）；reference.md:25 的既有漂移随本次修复；
- docs/ 双语手写文档（如有）全量扫旧词。

### 第 4 步：守卫测试与仪器

- [tests/residue.rs](file:///workspace/rebuild/tests/residue.rs) 扩展禁词表：`recover`（命令名 token）、`--replay-table-only`、`--release`（旗标形态）、`resizefs`、`records scan`、`--identity`、`--grow`（info 形态）、`\"inspect\"`（命令名）、`diskedit info`——精确 token 匹配，避开 recovery/identity 域词；
- [tests/contract_surface.rs](file:///workspace/rebuild/tests/contract_surface.rs) 面枚举更新为新命令全集（这同时就是将来负向声明仪器 #12/#15 的白名单）；
- case_runner/refusal_claims 中引用旧命令名的测试名与断言更新（如 `recover_refuses_a_target_without_a_record` → `undo_refuses_...`）；
- [audit.py](file:///workspace/rebuild/plan/instruments/audit.py) 新增 `check-naming` 子命令，三条命名法机检：
  - ①动词按意图命名：人工条款，audit 输出命令表供核对（不自动判）；
  - ②一概念一动词：静态检查 COMMANDS 两两 summary 相似度超阈值即红（近义词闸，防 info/inspect 复发）；
  - ③行为输入显式随调用：扫 `src/` 的 `env::var`，白名单仅限 state 目录定位（identity.rs 的 STATE_DIR_ENV/LOCALAPPDATA/XDG_STATE_HOME）——出现其他 env 读取即红。

### 第 5 步：账本与 D 登记

- [plan/divergences.md](file:///workspace/rebuild/plan/divergences.md) 登记 D20–D26（每条含理由 + 撤回条件=首个外部用户出现即锁定面）：
  - D20 info+inspect→layout 合一；D21 inspect 记录现场→records show；D22 recover→undo/replay/release 三动词；D23 resize+resizefs 合一（fs 轴）；D24 records scan 并入 list --root；D25 --identity→--alias；D26 info --grow→add --dry-run；
- refusal-map.tsv：只改新列（本实现消息引用与构造点路径随第 2 步变），旧列（旧二进制原文）一字不动；`audit.py check` 校验映射一致性；
- [plan/06-防漂移执行手册.md](file:///workspace/rebuild/plan/06-防漂移执行手册.md) 补三条命名法条款与 check-naming 门（挂进现有门序列，新门编号顺延）。

## Verification

1. `cargo test --all-targets` 全绿（含 generated_docs 逐字比对、residue、contract_surface、case_runner、refusal_map/refusal_claims）；
2. `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check`；
3. `python3 plan/instruments/audit.py check`（49 类）+ `check-disposition`（12,679 行不重不漏）+ 新 `check-naming` 通过；
4. gate.py 重录 G11–G14 证据 + 新命名门；
5. 残留终检：`grep -rn` 旧词 token（`diskedit recover`、`--replay-table-only`、`--identity`、`resizefs`、`records scan`、`diskedit inspect`、`diskedit info`）在 src/ + docs/ + plan 引用本实现消息处零命中（旧二进制历史引文除外）；
6. wsl 用例如本机不可跑（root/loop），登记为真机待跑项，不阻塞提交。

## Assumptions & Decisions

- `--discard-replay` 旗标名**保留不改**（已定案，非笔误）；replay 升格为动词后 `--replay-table-only` 自然消亡；
- `new` 不加 dry-run（无落位决策可预览）；`add --dry-run` 复用现有落位判定；
- 域概念词（recovery/identity）保留，面词零残留——残留扫描按 token 精确匹配而非全文禁词；
- 分支纪律：全部工作在本分支，提交后推送 test 线；不触碰其他分支。
