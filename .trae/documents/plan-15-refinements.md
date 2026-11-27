# 15 处修改精化版：执行规划

## Summary

对已收敛的 15 处改动清单采纳全部五条精化，重排执行顺序：**#6 定案程序提前到所有代码项之前**（先跑旧二进制取真值，认领真→补构造点、假→改账本，禁止拖）；#7 走既有 postcondition 回读模式第 3 实例（出现重复才抽公共件）；#11-15 合成一台负向声明仪器（声明进数据文件 + 两个检查器，第 16 条加一行即可）；合账宣告文本必须引用最终四态分布数字。

## 对五条精化的裁定（全部采纳，附依据）

| # | 精化 | 裁定 | 依据 |
| --- | --- | --- | --- |
| 1 | #6 二选一钉成程序 | 采纳，补一个前置：真值来源 = 按 sources.tsv 拿 e2fsprogs 1.47.4 构建旧版 resize2fs；`resize2fs -P` 对普通 ext 镜像文件即可运行，不需 root/真盘 | R0211/R0382/R0416 的措辞都是"resize2fs -P makes the final decision"——账本已把决定权外包给旧工具，判定必须由旧工具实跑产生，不许拍脑袋 |
| 2 | #7 指定走 postcondition 回读模式 | 采纳 | `src/app/execute/postcondition.rs`（29 行）已是通用 verify(&channel)，table.rs:115 与 relocate.rs:232 是现成两个实例，NTFS 作第 3 实例零抽象成本 |
| 3 | #11-15 合成负向声明仪器 | 采纳，落点对齐现有仪器结构：数据文件 `plan/baseline/negative-declarations.tsv` + 静态检查器 + 行为检查器，注册进 gate.py（新门 G15） | instruments 目录先例：census/tsv 基线 + audit.py 检查；5 条声明全部已有 SA 行（SA1-006/010/016/017/020，措辞已在 incident-classes.tsv），数据文件直接引用 |
| 4 | #6 提到所有代码项之前 | 采纳 | 结果可能改账本（R0211 族三行 + 可能波及 SA2 行），账本未定案就动码会造成返工 |
| 5 | 宣告写数字 | 采纳 | 宣告文本加进 00-root-causes.md，引用四态分布（当前：认领 1,549 / 拒绝 3,154 / 差异 23 / 裁剪 7,953）与工作项清零状态 |

## Current State

- 分支 test（a0a983c），账本 12,679 行 0 待判，四门 G11-G14 全绿；
- #6 相关：refusal-map R0211（decide 续跑判定）、R0382（dry_run 分支）、R0416（MBR 分支）三行引用 `resize2fs -P` 消息原文；sources.tsv 已登记 e2fsprogs 1.47.4（URL+sha256）；
- #7 相关：`src/app/execute/postcondition.rs::verify(&channel)`，两个调用点 table.rs / relocate.rs；NTFS 当前由 `FsKind::Ntfs` 分支绕过回读（缺闸）；
- #11-15 相关：SA1-006（discard）、SA1-010（JSON 导入）、SA1-016（老盘标）、SA1-017（未登记 fs）、SA1-020（备份菜单）措辞齐备；
- 仪器结构先例：`plan/instruments/gate.py run --name Gxx --kind lint` 记录 evidence。

## 执行顺序与具体改动

### 第 0 步：#6 定案（改账本或改码之前必须完成）

1. 按 sources.tsv 下载并校验 sha256 的 e2fsprogs 1.47.4，`./configure && make -C resize2fs` 得旧版 resize2fs（记录版本与 sha256 到证据文件 `plan/evidence/G6-truth.log`）；
2. 构造缩容场景镜像（ext4，含 `-P` 能回答的 min size），跑旧 resize2fs -P 取真值输出；
3. 判定：旧工具确实按消息语义给出 min size → **认领真**：在 `src/app/decide/resize.rs` 三个分支（R0211/R0382/R0416 对应处）补构造点（写前调用只读探针，失败即拒绝）+ 单测；旧工具行为与账本措辞不符 → **认领假**：改 refusal-map 三行 + 差异表登记（新 D20），不动码；
4. 用 gate.py 记 `G6` 证据。两条路都必须出结果，不许既不补码也不改账。

### 第 1 步：parse/policy 层批量（#1-#5）

| 项 | 文件 | 改动 | 测试锚 |
| --- | --- | --- | --- |
| EBR 签名 | `src/app/table/mbr/extended.rs::walk_extended` | 链上每个 EBR 先验 0x55AA 再推进，失败按分类回退（不猜） | 环镜像单测：链中段坏签名 |
| 条目下限 | `src/app/table/gpt/parse.rs` | 条目数组字节总量 <16384B 拒绝 | 构造短数组镜像 |
| revision 上限 | `src/app/table/gpt/header.rs` | rev >0x00010200 拒绝 | 构造高 rev 头 |
| boot 扩展容器 | `src/app/decide/edit.rs` | set flag boot 目标为扩展容器记录时拒绝 | case_runner 用例 |
| vfat 标签 | 决策层 content_property | 标签 11 字符/字符集前置校验（移除对工具报错的依赖） | 长标签/非法字符用例 |

### 第 2 步：#7 NTFS 回读闸（postcondition 第 3 实例）

- `src/app/execute/`（NTFS 分支落点）：写后回读 `ntfsresize --info --force`，`grep "Current volume size"`，缺失/不匹配即拒绝；模式与 table.rs:115 完全一致，**不抽公共件**（若第 3 实例出现重复再议）；
- 测试：qemu_suite 的 ntfs 场景（现有 `Current volume size` 探针在 tests/qemu_suite.rs:347）+ 本地负样本单测。

### 第 3 步：#8/#9 提示面

- `src/app/`（diagnose 输出处）：check 非零退出区分"拒绝（判定层）"与"问题（fsck 报告）"两类消息；
- inspect 诊断输出补 GPT 头 revision 越界与 reserved 字段损伤告警。

### 第 4 步：#11-15 负向声明仪器（G15）

1. 数据文件 `plan/baseline/negative-declarations.tsv`：列 = decl_id | SA id | 声明一句话 | 检查器(static/behavioral) | 检查对象；5 行初值：
   - N1=SA1-006 源码禁 discard/TRIM → static
   - N2=SA1-010 CLI 子命令白名单不含导入 → behavioral
   - N3=SA1-016 老盘标输入统一拒绝 → behavioral
   - N4=SA1-017 未登记 fs 签名按未知拒绝 → behavioral
   - N5=SA1-020 CLI 白名单不含菜单式备份 → behavioral
2. 检查器 A（static）：`plan/instruments/negative_static.py`——禁词扫源码（residue 式），命中即红；
3. 检查器 B（behavioral）：`tests/negative_declarations.rs`——负样本用例逐条驱动（CLI 白名单枚举 + 老盘标/未登记 fs 镜像）；
4. gate.py 注册 G15 跑两个检查器；第 16 条声明 = 数据文件加一行。

### 第 5 步：#10 账面 + 合账宣告

- SA2 账本补 btrfs/xfs mkfs/check 消费面定案行；
- `plan/disposition/00-root-causes.md` 宣告文本重写：引用最终四态分布数字（以当次 `audit.py check-disposition` 输出为准）+ 工作项清零 + G11-G15 门状态。

## Verification

1. `cargo test --all-targets` 全绿（含新增单测与 negative_declarations）；
2. `gate.py` 记录 G6 + G11 + G12 + G13 + G14 + G15 全部通过；
3. `audit.py check-disposition`：行数、四态分布与宣告文本数字一致；
4. #6 若走"改账"路线：G11（49 类）与 G14 仍绿；
5. footprint/commits 纪律守卫通过（本地跑 instruments 对应脚本）。

## Assumptions & Decisions

- 旧版 resize2fs 真值在本地容器构建（-P 不需 root）；若构建失败则改用 drift 机制登记到真机环境，#6 在此期间保持"未定案"，第 1 步及其后不允许开始——这是精化 4 的严格执行；
- #7 严格不抽公共件；第 4 实例出现时才抽；
- 全程不动 main 分支，只提交 test；CI yml 不在本次范围。
