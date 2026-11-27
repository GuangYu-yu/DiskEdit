# sites-parted-frontend-assert-libparted 处置判断

## 判断

- 本批 241 行（parted 3.6 防御面站点账本）。四态计数：认领 0 / 拒绝 136 / 裁剪 105 / 差异 0。
- 断言/异常/失败 goto 的防御类站点：C 空指针与生命周期断言、分配失败清理 goto 由 Rust 所有权/类型/Result 承担，判拒绝（架构差异-多虑）；防御面计数引 SA1-021（census:parted）。
- 错误提示/提示面类站点：refusal-map 已有对应拒绝面的判认领并引 R 编号或 SA；绑定交互 prompt 的判裁剪（本实现恒非交互）。
- 老古董 C 实现细节（i18n/iconv/gettext、stdout 刷新、崩溃诊断脚手架、进度 timer）：判拒绝，理由 Rust 架构差异。
- 涉及真实盘面校验且本实现缺失的判认领+待实现（本批如有，见 note 中「待实现」标记）。

## 裁剪依据

- **SA1-016**：老盘标（mac/sun/dvh/amiga/bsd/atari/pc98/dasd/fdasd/vtoc/aix/loop）不识别，统一按未知拒绝并提示。
- **SA1-017**：逐文件系统探测与识别精度层裁剪：识别只按档案表与只读探针（R0134），未登记族按未知拒绝。
- **SA1-028**：BIOS CHS 几何：不读几何、不写真实 CHS，MBR 一律 LBA 占位（SA1-027）。
- **SA2-014**：FAT（vfat）扩缩容：档案表按不支持拒绝，不接 fatresize。
- **坏块扫描**：ped_geometry_check/timer 坏块扫描面不入范围。
- **非 Linux 平台**：BeOS/GNU Hurd 平台后端不入范围：本实现仅 src/platform/linux。

## 依据

- 权威依据：plan/baseline/incident-classes.tsv（SA1-016/017/020/021/025/027/028、SA2-012/014、SA3-003）、plan/baseline/refusal-map.tsv（R 行）、plan/06-防漂移执行手册.md（不猜边界）。
- 参考行原文：plan/baseline/reference/sites/parted.tsv（site_id 索引）。
- 新实现事实源：crates/core/src/table（mbr/gpt）、crates/core/src/fs/catalog.rs（档案表）、crates/core/src/geometry、src/app/observe（R0131/R0132/R0134/R0143）、src/app/execute（filesystem.rs/write.rs/transaction.rs）、src/app/report/exit.rs（SA1-009）、src/platform/linux（SA1-013/SA1-025）。

## 撤回条件

- SA1-016（老盘标）：出现必须识别 mac/sun/dvh/amiga/bsd/atari/pc98/dasd/fdasd/vtoc/aix/loop 的外部契约时，先补探测与写回用例与拒绝矩阵，再撤销对应裁剪行。
- SA1-017（逐 fs 探测/识别精度）：档案表登记新族并配只读探针与拒绝矩阵后，对应 FAT/HFS 逐字段校验行可转为认领。
- SA1-028（CHS）：出现必须写真实 CHS 的引导器契约且几何来源可探测时撤销；届时一并复核 LBA 旗标行。
- SA2-014（FAT 扩缩容）：上游 fatresize 修复分区设备视图断言、能证明 -s 语义安全后重新评估。
- 坏块扫描：规范要求读写面坏块检查时再评估；当前由外部工具承担。
- 非 Linux 平台：出现 BeOS/Hurd 交付目标时重开；本实现仅承诺 Linux。

## 差异与拆分

- 本批无差异（D-建议）行；结论混合时拆分建议见 plan/disposition/reports/chunk-C-parted.md。

## 换版重放补充（2026-11-27：parted 3.6→3.8）

- 本批新增/改写 7 行：裁剪 6 行（00761/00762/00776/00777 fs/r/fat 断言——parted fs 操作面不入范围，SA1-017/SA2-014；00910/00911 fs/r/hfs cache 断言——SA1-017）；拒绝 1 行（01792 ped_malloc 参数断言——C 库内不变量，Rust 所有权/类型承担，SA1-021）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：SA1-016/017 重开（出现必须识别对应老盘标/登记新文件系统族的外部契约，须先补探测、写回用例与拒绝矩阵）或 SA2-014 重开（上游 fatresize 修复分区设备视图断言且 -s 语义安全）时，对应行转为认领并重审。01792 撤回条件：Rust 架构承担失效（如引入等价 C 分配层）时重评。
