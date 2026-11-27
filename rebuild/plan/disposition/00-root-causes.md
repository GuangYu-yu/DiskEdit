# 全局根因汇总（全部 11,918 行处置完成后的收敛方案）

> 2026-11-27 换版重放：参考工具全部升至最新稳定版（见 sources.tsv），旧账 11,331 行判断按内容映射迁移，1,348 行因上游改写回待判。下表为重放后口径。

## 1. 处置总览（门验证：263 批 12,679 行不重不漏，待判 0）

> 换版重放新增 1,348 行已全部判断（认领 70 / 拒绝 204 / 裁剪 1,074 / 工作项 0），换版未暴露任何结构性缺口。

| 态 | 行数 | 占比 | 含义 |
| --- | --- | --- | --- |
| 认领 | 1,549 | 13% | 本实现已有（引 SA/R/代码/测试锚）+ 10 项待实现工作项 |
| 拒绝 | 3,154 | 26% | C 防御断言/i18n/崩溃诊断等老古董细节，架构差异吸收 |
| 差异 | 23 | <1% | 归并为 D1–D19，全部含撤回条件登记于 divergences.md |
| 裁剪 | 7,953 | 61% | 交互面、工具内部子命令、老盘标、内容级识别——特性不存在 |

## 2. 全局根因分层（不是"之前改的那批"，而是全部参考行为的结构归因）

| 层 | 根因 | 收敛硬约束 | 状态 |
| --- | --- | --- | --- |
| 范围外（7,329 裁剪） | 参考行为绑定不存在的特性：交互 prompt/TUI、老盘标、菜单式恢复、fs 修复器本体、未消费子命令 | **档案表单点**（Support 表）+ 范围裁剪六段式（不学什么/规范依据/影响面/可见后果/替代路径/撤回条件），每批 md 已含撤回条件 | 已收敛——扩展只改档案表一处 |
| 实现细节（3,054 拒绝） | C 手工防御（assert/BUG_ON/fail-goto 清理链/内存管理）、i18n、stdout 刷新 | **Rust 架构吸收**：所有权/checked 运算/Result 单点替代；census 仪器（SA1-021/022/023）把"防御面对称"变成可验证计数 | 已收敛 |
| 行为面（1,512 认领） | 真实用户可见行为：校验、次序、回读、退出码、拒绝面 | 已实现的引 SA/R/测试锚；未实现的 9 项见 §3 | 收敛中 |
| 刻意差异（23 行） | 本实现更保守/更精确：占位 CHS、读侧严格、卷标前置拒绝、结构化输出 | D1–D19 登记（理由+撤回条件+测试位置） | 已收敛 |

## 3. 全部待实现工作项（开工清单，按根因族）

**缺校验族（6 项）**——共同根因：参考工具的逐点边界校验粒度高于本实现 parse/policy 层现有断言。收敛：全部落在 parse/policy 单点 + 单测锚，每项两门全绿翻态：

1. `EBR-003`：walk_extended 逐级校验 0x55AA 签名（现只按链接记录推进）→ [chunk-A](file:///workspace/rebuild/plan/disposition/reports/chunk-A-specs-inc-docs.md)
2. `testdisk-00012`：GPT 条目数组字节总量 ≥16384B 下限 → [chunk-B](file:///workspace/rebuild/plan/disposition/reports/chunk-B-imp-td-gd.md)
3. `parted-01412`：GPT 头 revision >0x00010200 拒绝识别 → [chunk-C](file:///workspace/rebuild/plan/disposition/reports/chunk-C-parted.md)
4. `libfdisk-00356`：set flag boot 前拒扩展容器记录 → [chunk-E](file:///workspace/rebuild/plan/disposition/reports/chunk-E-lfd-dos-f2fs.md)
5. `dosfstools-00113/00118`：vfat 标签 11 字符/字符集决策层前置 → [chunk-E](file:///workspace/rebuild/plan/disposition/reports/chunk-E-lfd-dos-f2fs.md)
6. 观察项核对：ext 收缩 `resize2fs -P` 最小尺寸写前闸——R0211/R0382/R0416 认领指向 decide/resize.rs，但 `BelowMinimumSize` 无构造点；核对 refusal-map 三行或登记差异 → [chunk-D](file:///workspace/rebuild/plan/disposition/reports/chunk-D-e2fsprogs.md)

**缺回读证实族（1 项）**——共同根因：NTFS 是唯一无产品侧对称回读的可收缩族（ext/btrfs/xfs/lvm 均已有闸）：

7. `ntfs-3g-00063`：NTFS 收缩后 `ntfsresize --info` 回读，字段缺失即拒绝不猜 → [chunk-G](file:///workspace/rebuild/plan/disposition/reports/chunk-G-lvm-ntfs.md)

**提示面族（2 项）**：

8. check 工具"运行前拒绝"被统一映射为 `Problems`（btrfs-progs-00975/00977 类）——区分拒绝/问题两类消息 → [chunk-F](file:///workspace/rebuild/plan/disposition/reports/chunk-F-btrfs-xfs.md)
9. GPT 头 rev/reserved 损伤加入 inspect 诊断输出（testdisk-00008/00009）→ [chunk-B](file:///workspace/rebuild/plan/disposition/reports/chunk-B-imp-td-gd.md)

**账面族（1 项）**：

10. SA2-012 枚举不含 btrfs/xfs 的 mkfs/check 固定参数消费面 → 补一条 SA2 行入账 → [chunk-F](file:///workspace/rebuild/plan/disposition/reports/chunk-F-btrfs-xfs.md)

## 3.5 全部 15 处改动（三档视图）与五条精化钉死

**第一档：10 项工作项（同 §3，按类型汇总）**

| # | 修改处 | 类型 | 规模 |
| --- | --- | --- | --- |
| 1 | `mbr/extended.rs`：walk_extended 逐级校验 0x55AA 签名 | 代码+单测 | 小 |
| 2 | `gpt/parse.rs`：条目数组字节总量 ≥16384B 下限 | 代码+单测 | 小 |
| 3 | `gpt/header.rs`/parse：revision >0x00010200 拒绝 | 代码+单测 | 小 |
| 4 | `edit.rs`：set flag boot 前拒扩展容器记录 | 代码+单测 | 小 |
| 5 | 决策层 content_property：vfat 标签 11 字符/字符集前置 | 代码+单测 | 小 |
| 6 | ext 收缩 `resize2fs -P` 写前闸（程序见下） | 核对→程序化二选一 | 小 |
| 7 | `filesystem.rs`：FsKind::Ntfs 对称回读闸 | 代码+测试 | 中 |
| 8 | diagnose：check 非零退出区分"拒绝/问题"两类消息 | 代码 | 小 |
| 9 | inspect 诊断输出补 GPT 头 rev/reserved 损伤告警 | 代码 | 小 |
| 10 | SA2 账本补 btrfs/xfs mkfs/check 消费面定案行 | 纯账面 | 0 代码 |

**第二档：5 条负向声明锚（"从不做 X"类，测行为测不了的那批）**

| # | 声明 | 落点 | 类型 |
| --- | --- | --- | --- |
| 11 | 从不 discard/TRIM（SA1-006） | 静态禁词仪器（residue 式） | 仪器 |
| 12 | 不提供 JSON/sfdisk 导入（SA1-010） | CLI 面枚举断言 | 测试 |
| 13 | 老盘标统一拒绝（SA1-016） | 负样本单测 | 测试 |
| 14 | 未登记 fs 识别拒绝（SA1-017） | 负样本单测 | 测试 |
| 15 | 不提供菜单式备份回写（SA1-020） | CLI 面枚举断言 | 测试 |

**结构（合成一台仪器，不散装五件）**：声明进数据文件 `plan/baseline/negative-declarations.tsv`（列：decl_id | SA id | 声明一句话 | 检查器 static/behavioral | 检查对象），两个检查器——A `plan/instruments/negative_static.py`（禁词扫源码）、B `tests/negative_declarations.rs`（CLI 白名单枚举 + 老盘标/未登记 fs 负样本镜像）——注册进 gate.py 为 **G15**；第 16 条声明 = 数据文件加一行。

**五条精化（钉成程序）**：

1. **#6 二选一是程序不是说法**：先按 sources.tsv 构建旧版 resize2fs（1.47.4，sha256 校验）取真值（`resize2fs -P` 对普通 ext 镜像即可跑，不需 root）→ 认领真则三处（R0211/R0382/R0416）补构造点+单测，认领假则改 refusal-map 三行并登记差异——禁止既不补码也不改账地拖着；
2. **#7 指定实现路径**：走既有 postcondition 回读模式（`src/app/execute/postcondition.rs::verify`，table.rs/relocate.rs 为前两实例），NTFS 作第 3 实例，出现重复才抽公共件——防过早抽象也防特例散布；
3. **#11-15 合成一台负向声明仪器**（结构见上），不散装五件；
4. **#6 提到所有代码项之前**：它的结果可能改账本，先定案再动码；未定案期间第 1 步及其后不允许开始；
5. **宣告写数字**：合账宣告文本必须引用最终四态分布（当次 `audit.py check-disposition` 输出为准）与工作项清零状态，不许写"应该没有了"。

**汇总口径**：真代码改动 7 处（#1-5、7、8，#9 可选提示面），测试/仪器 7 处（含各项自带测试锚），1 项程序化核对 + 1 项纯账面。做完这 15 处并 G11-G15 全绿，静态部分"没有可借鉴的了"即可宣告（黑盒 SA3 对账仍是最终合账前提）。

## 4. 硬约束收敛映射（每项落点）

- **架构**：1–5 归 parse/policy 校验单点（表结构校验不散落调用点）；标签类归决策层 `content_property` 前置；7 归"完成=回读证实"架构的 FsKind::Ntfs 对称补点。
- **闸门**：10 项每项完成以"测试锚 + audit 两门全绿"翻态；账本行 note 从待实现改为认领。
- **仪器**：负向声明锚 5 条（G-1 从不 discard / G-2 JSON 导入 / G-3 老盘标 / G-4 未登记 fs / G-5 备份菜单）落成 §3.5 第二档的单台仪器（数据文件 + 双检查器 + G15 门）。
- **测试**：各报告已给出锚建议（缺签名 EBR 镜像、数组下限、revision 上限、容器记录拒绝、NTFS 回读不足落待恢复）。
- **已知薄弱点（不新增工作）**：D4 修复路径故障注入缺夹具（旧档已记），随 6 的核对一并覆盖。

## 5. "之前的改动是否从根上解决"——全局回答

是。10,383 行（裁剪+拒绝）无需任何代码即闭合，证明两大结构性约束（档案表单点、Rust 架构吸收 C 防御）已经吃掉了参考工具防御实践的主体；1,512 认领中 1,503 行直接锚到既有实现。未收敛面只剩 §3 的 10 项，全部是**校验粒度**问题而非结构性缺陷——即参考工具 40 年实践里没有再暴露出任何"我们的架构写不出来的东西"。

## 6. 遗留对照

- README §7 的 9 条盲区不变（黑盒 SA3、跨版本矩阵等）仍是合账最终前提；
- 差异 D4/D10 的"未验证"标注不变；
- 本档 §3 清单开工后逐项销账，销完即可宣告"没有可借鉴的了"的静态部分完成。
