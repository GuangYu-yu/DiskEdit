#!/usr/bin/env python3
"""SA 账本审计：事故类与隐式行为必须逐行有归属、有证据。

账本列固定为：
  class_id  source  source_loc  category  description  behavior
  evidence  state  reason

状态只有四种：已认领 / 差异 / 范围裁剪 / 已登记缺口。
- 已认领：evidence 里每个逗号分隔的标记都能落到真实用例或差异登记。
- 差异：evidence 指向 plan/divergences.md 里存在的 D 编号。
- 已登记缺口：evidence 形如 `owner:SA2`，且 06 附录 B 有该批。
- 范围裁剪：reason 必须给出六段（用 `;` 分隔）：不学什么、规范依据、
  影响面、用户可见后果、替代路径、撤回条件。

另外核对 06 附录 A 的 M4 表与账本逐行同集、状态一致；M4 少一行或状态
不一致都算欠账。

防御面对称 census（`census` 子命令）的计数口径固定在 `count_defenses`：
按文件累计断言（`PED_ASSERT`/`assert`/`ASSERT`）、异常抛出
（`ped_exception_throw`/`throw`）、失败路径 `goto`（目标名以
fail/error/err/out/done/cleanup 开头）。结果写 `plan/baseline/defense-census.tsv`，
账本用 `census:<tool>` 引用；数字为 0 也如实记录。
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import pathlib
import re
import subprocess
import sys

COLUMNS = [
    "class_id",
    "source",
    "source_loc",
    "category",
    "description",
    "behavior",
    "evidence",
    "state",
    "reason",
]
STATES = {"已认领", "差异", "范围裁剪", "已登记缺口"}
R = re.compile(r"^R\d{4}$")
D = re.compile(r"^D\d+$")
OWNER = re.compile(r"^owner:(SA\d+)$")
CASE = re.compile(r"^(?P<file>[^\s:]+\.rs)::(?P<name>[A-Za-z0-9_]+)$")
CENSUS = re.compile(r"^census:(?P<tool>[a-z0-9-]+)$")
CENSUS_COLUMNS = ["tool", "version", "source", "files", "asserts", "exceptions", "goto_fail", "total"]


def count_defenses(root: pathlib.Path) -> tuple[int, int, int, int]:
    """数三类防御点：断言、异常抛出、失败路径 goto。口径见模块说明。"""
    files = asserts = exceptions = gotos = 0
    for path in sorted(root.rglob("*")):
        if path.suffix not in {".c", ".h", ".cc", ".cpp"}:
            continue
        files += 1
        text = path.read_text(encoding="utf-8", errors="ignore")
        asserts += len(re.findall(r"\b(?:PED_ASSERT|assert|ASSERT)\s*\(", text))
        exceptions += len(re.findall(r"\b(?:ped_exception_throw|throw)\b", text))
        gotos += len(re.findall(r"\bgoto\s+(?:fail|error|err|out|done|cleanup)\w*\s*;", text))
    return files, asserts, exceptions, gotos


def load_census(path: pathlib.Path) -> dict[str, dict[str, str]]:
    if not path.is_file():
        return {}
    rows: dict[str, dict[str, str]] = {}
    lines = path.read_text(encoding="utf-8").splitlines()
    if not lines or lines[0].split("\t") != CENSUS_COLUMNS:
        return {}
    for line in lines[1:]:
        if not line.strip():
            continue
        cells = line.split("\t")
        if len(cells) == len(CENSUS_COLUMNS):
            rows[cells[0]] = dict(zip(CENSUS_COLUMNS, cells))
    return rows


def write_census(path: pathlib.Path, row: dict[str, str]) -> None:
    rows = load_census(path)
    rows[row["tool"]] = row
    lines = ["\t".join(CENSUS_COLUMNS)]
    for tool in sorted(rows):
        lines.append("\t".join(rows[tool][column] for column in CENSUS_COLUMNS))
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def load_ledger(path: pathlib.Path) -> tuple[list[dict[str, str]], list[str]]:
    errors: list[str] = []
    rows: list[dict[str, str]] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    if not lines or lines[0].lstrip("# ").split("\t") != COLUMNS:
        errors.append(f"{path}: 表头必须恰为 {COLUMNS}")
        return rows, errors
    seen: set[str] = set()
    for number, line in enumerate(lines[1:], start=2):
        if not line.strip() or line.startswith("#"):
            continue
        cells = line.split("\t")
        if len(cells) != len(COLUMNS):
            errors.append(f"{path}:{number}: 需要 {len(COLUMNS)} 列，实际 {len(cells)}")
            continue
        row = dict(zip(COLUMNS, cells))
        cid = row["class_id"]
        if not re.fullmatch(r"SA\d-\d{3}", cid):
            errors.append(f"{path}:{number}: class_id 形如 SA1-001，得到 {cid!r}")
        if cid in seen:
            errors.append(f"{path}:{number}: class_id 重复 {cid}")
        seen.add(cid)
        if row["state"] not in STATES:
            errors.append(f"{path}:{number}: 非法状态 {row['state']!r}")
        rows.append(row)
    return rows, errors


def resolve_case(token: str, tests: pathlib.Path, root: pathlib.Path) -> bool:
    match = CASE.fullmatch(token)
    if not match:
        return False
    candidates = [tests / match["file"], root / match["file"]]
    for file in candidates:
        if file.is_file() and re.search(
            rf"\bfn\s+{re.escape(match['name'])}\s*\(", file.read_text(encoding="utf-8")
        ):
            return True
    return False


def load_ids(path: pathlib.Path, pattern: re.Pattern[str], prefix: str) -> set[str]:
    if not path.is_file():
        return set()
    if prefix == "R":
        return {
            line.split("\t", 1)[0].strip()
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.split("\t", 1)[0].strip().startswith("R")
        }
    return {
        found.group(0)
        for found in (pattern.search(line) for line in path.read_text(encoding="utf-8").splitlines())
        if found
    }


def load_m4(path: pathlib.Path) -> tuple[dict[str, str], list[str]]:
    errors: list[str] = []
    rows: dict[str, str] = {}
    text = path.read_text(encoding="utf-8")
    if "### M4" not in text:
        return rows, [f"{path}: 缺少 附录 A 的 M4 表"]
    section = text.split("### M4", 1)[1].split("\n## ", 1)[0]
    for line in section.splitlines():
        if not line.startswith("| SA"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if len(cells) < 2:
            errors.append(f"{path}: M4 行不完整：{line}")
            continue
        rows[cells[0]] = cells[-1]
    if not rows:
        errors.append(f"{path}: M4 表没有任何 SA 行")
    return rows, errors


def validate(
    rows: list[dict[str, str]],
    m4: dict[str, str],
    known_r: set[str],
    known_d: set[str],
    batches: set[str],
    tests: pathlib.Path,
    root: pathlib.Path,
    census: dict[str, dict[str, str]] | None = None,
) -> list[str]:
    errors: list[str] = []
    census = census or {}
    for row in rows:
        cid = row["class_id"]
        state = row["state"]
        if state not in STATES:
            errors.append(f"{cid}: 非法状态 {state!r}")
            continue
        evidence = [part.strip() for part in row["evidence"].split(",") if part.strip()]
        if state == "已认领":
            if not evidence:
                errors.append(f"{cid}: 已认领但没有证据")
            for token in evidence:
                if R.fullmatch(token):
                    if token not in known_r:
                        errors.append(f"{cid}: 证据 {token} 不在 refusal-map.tsv")
                elif D.fullmatch(token):
                    if token not in known_d:
                        errors.append(f"{cid}: 证据 {token} 不在 divergences.md")
                elif CENSUS.fullmatch(token):
                    tool = CENSUS.fullmatch(token).group("tool")
                    if tool not in census or not census[tool].get("version"):
                        errors.append(f"{cid}: {token} 在 defense-census.tsv 没有记录")
                elif not resolve_case(token, tests, root):
                    errors.append(f"{cid}: 证据 {token} 找不到对应的用例函数")
        elif state == "差异":
            if not evidence or any(token not in known_d for token in evidence if D.fullmatch(token)):
                errors.append(f"{cid}: 差异状态要指向存在的 D 编号")
        elif state == "已登记缺口":
            match = OWNER.fullmatch(row["evidence"].strip())
            if not match or match.group(1) not in batches:
                errors.append(f"{cid}: 缺口证据应形如 owner:SA2，且该批在 06 附录 B 中")
        elif state == "范围裁剪":
            parts = [part.strip() for part in row["reason"].split(";")]
            if len(parts) != 6 or any(not part for part in parts):
                errors.append(f"{cid}: 裁剪理由必须六段（; 分隔）")
        if cid not in m4:
            errors.append(f"{cid}: 06 附录 A 的 M4 缺这一行")
        elif m4[cid] != state:
            errors.append(f"{cid}: M4 状态 {m4[cid]!r} 与账本 {state!r} 不一致")
    for cid in m4:
        if cid not in {row["class_id"] for row in rows}:
            errors.append(f"{cid}: M4 有账本没有的行")
    return errors


def check(args: argparse.Namespace) -> int:
    rows, errors = load_ledger(args.ledger)
    m4, m4_errors = load_m4(args.m4)
    errors.extend(m4_errors)
    known_r = load_ids(args.refusal_map, R, "R")
    known_d = load_ids(args.divergences, D, "D")
    batches = {
        found.group(1)
        for found in re.finditer(r"^\|\s*(SA\d)\s*\|", args.appendix.read_text(encoding="utf-8"), re.M)
    }
    errors.extend(
        validate(
            rows,
            m4,
            known_r,
            known_d,
            batches,
            args.tests,
            args.root,
            load_census(args.census),
        )
    )
    if errors:
        for error in errors:
            print(f"audit: {error}", file=sys.stderr)
        return 1
    print(f"audit: {len(rows)} 类全部有归属与证据")
    return 0


DISPOSITION_STATES = {"认领", "拒绝", "差异", "裁剪", "待判"}


def reference_ids(ref_dir: pathlib.Path) -> dict[str, str]:
    """返回 reference 账本全部行 id -> 所属表。"""
    result: dict[str, str] = {}
    tables = [
        ("sites", ref_dir / "sites"),
        ("docs", ref_dir / "docs.tsv"),
        ("incidents", ref_dir / "incidents.tsv"),
        ("implicit", ref_dir / "implicit.tsv"),
        ("specs", ref_dir / "specs.tsv"),
    ]
    for ledger, path in tables:
        if path.is_dir():
            for member in sorted(path.glob("*.tsv")):
                with member.open(newline="", encoding="utf-8") as f:
                    for row in csv.DictReader(f, delimiter="\t"):
                        result[row["site_id"]] = ledger
        else:
            with path.open(newline="", encoding="utf-8") as f:
                key = {
                    "docs": "doc_id",
                    "incidents": "incident_id",
                    "implicit": "implicit_id",
                    "specs": "spec_id",
                }[ledger]
                for row in csv.DictReader(f, delimiter="\t"):
                    result[row[key]] = ledger
    return result


def check_disposition(args: argparse.Namespace) -> int:
    """处置门：批次覆盖不重不漏，状态合法，裁剪/差异链证据。"""
    errors: list[str] = []
    known = reference_ids(args.ref)
    if not known:
        errors.append(f"{args.ref}: 未读到任何 reference 行")
    queue_path = args.disp / "queue.tsv"
    seen: dict[str, str] = {}
    batch_count = 0
    judged = 0
    with queue_path.open(newline="", encoding="utf-8") as f:
        for line_number, row in enumerate(csv.DictReader(f, delimiter="\t"), start=2):
            batch_count += 1
            batch_path = args.disp / f"{row['batch']}.tsv"
            md_rel = row["evidence_md"].strip()
            md_path = args.disp / md_rel if md_rel else None
            if not batch_path.exists():
                errors.append(f"queue.tsv:{line_number}: 批次文件缺失 {batch_path}")
                continue
            declared = int(row["rows"])
            with batch_path.open(newline="", encoding="utf-8") as bf:
                members = list(csv.DictReader(bf, delimiter="\t"))
            if len(members) != declared:
                errors.append(f"{batch_path}: 声明 {declared} 行，实际 {len(members)}")
            for member in members:
                rid = member["row_id"]
                if rid not in known:
                    errors.append(f"{batch_path}: {rid} 不在 reference 账本中")
                elif known[rid] != member["ledger"]:
                    errors.append(f"{batch_path}: {rid} 表属不一致 {member['ledger']}≠{known[rid]}")
                elif rid in seen:
                    errors.append(f"{batch_path}: {rid} 与 {seen[rid]} 重复登记")
                else:
                    seen[rid] = str(batch_path)
                if member["state"] not in DISPOSITION_STATES:
                    errors.append(f"{batch_path}: {rid} 非法状态 {member['state']!r}")
                elif member["state"] != "待判":
                    judged += 1
                    if member["state"] in {"裁剪", "差异"}:
                        if md_path is None or not md_path.exists():
                            errors.append(
                                f"{batch_path}: {rid} 裁剪/差异须在 queue.tsv 填 evidence_md 指向依据文件"
                            )
                        elif member["state"] == "裁剪":
                            text = md_path.read_text(encoding="utf-8")
                            if "撤回条件" not in text or "（裁剪/差异必填）" in text:
                                errors.append(f"{md_path}: 裁剪批次须含撤回条件且不得为模板占位")
                    if member["state"] == "差异":
                        if not re.search(r"D\d+", member["note"]):
                            errors.append(f"{batch_path}: {rid} 差异须在 note 链 D 编号")
    missing = set(known) - set(seen)
    for rid in sorted(missing)[:20]:
        errors.append(f"未处置: {rid}")
    if missing:
        errors.append(f"共 {len(missing)} 行未入任何批次")
    if errors:
        for error in errors:
            print(f"audit: {error}", file=sys.stderr)
        return 1
    print(
        f"disposition: {batch_count} 批覆盖 {len(seen)} 行不重不漏，已判 {judged}，待判 {len(seen) - judged}"
    )
    return 0


SITE_KINDS = ("assert", "exception", "goto")
SITE_RES = {
    "assert": re.compile(r"\b(?:PED_ASSERT|assert|ASSERT)\s*\("),
    "exception": re.compile(r"\b(?:ped_exception_throw|throw)\b"),
    "goto": re.compile(r"\bgoto\s+(?:fail|error|err|out|done|cleanup)\w*\s*;"),
}


def enumerate_sites(root: pathlib.Path) -> list[tuple[str, int, str]]:
    """与 count_defenses 同口径逐站点枚举：(相对路径, 行号, 类别)，按序稳定。"""
    sites: list[tuple[str, int, str]] = []
    for path in sorted(root.rglob("*")):
        if path.suffix not in {".c", ".h", ".cc", ".cpp"}:
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        rel = str(path.relative_to(root))
        for kind, regex in SITE_RES.items():
            for match in regex.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                sites.append((rel, line, kind))
    sites.sort()
    return sites


def census_per_site(args: argparse.Namespace) -> int:
    """逐站点 census：枚举防御站点 → 映射类 ID → 要求 100% 覆盖。

    口径与 count_defenses 完全一致，因此本命令同时是三重红灯：
    1. 源码树与 defense-census.tsv 计数不符（census 过期）；
    2. 枚举出的站点在 site-map.tsv 缺行（漏一个站点即红）；
    3. site-map 引用了不存在的类 ID 或已失效站点行（映射过期）。
    """
    errors: list[str] = []
    census = load_census(args.census)
    if not census:
        errors.append(f"{args.census}: 无法读取 census")
    sources_ok = True
    enumerated: dict[str, tuple[str, str, int, str]] = {}
    per_tool: dict[str, dict[str, int]] = {}
    for tool, row in sorted(census.items()):
        root = pathlib.Path(row["source"])
        if not root.is_dir():
            errors.append(f"{tool}: 源码树缺失 {root}（按 sources.tsv 取回后重跑）")
            sources_ok = False
            continue
        sites = enumerate_sites(root)
        counts = {
            "assert": sum(1 for s in sites if s[2] == "assert"),
            "exception": sum(1 for s in sites if s[2] == "exception"),
            "goto": sum(1 for s in sites if s[2] == "goto"),
        }
        expected = {
            "assert": int(row["asserts"]),
            "exception": int(row["exceptions"]),
            "goto": int(row["goto_fail"]),
        }
        for kind in SITE_RES:
            if counts[kind] != expected[kind]:
                errors.append(
                    f"{tool}: {kind} 站点数 {counts[kind]} ≠ census {expected[kind]}（census 过期或源不符）"
                )
        per_tool[tool] = counts | {"files": int(row["files"])}
        for rel, line, kind in sites:
            enumerated[f"{tool}|{rel}|{line}|{kind}"] = (tool, rel, line, kind)
    if args.out:
        out = pathlib.Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        with out.open("w", encoding="utf-8", newline="") as f:
            f.write("site_key\ttool\tfile\tline\tkind\n")
            for key in sorted(enumerated):
                tool, rel, line, kind = enumerated[key]
                f.write(f"{key}\t{tool}\t{rel}\t{line}\t{kind}\n")
    # site-map
    mapping: dict[str, str] = {}
    map_path = pathlib.Path(args.map)
    if map_path.exists():
        with map_path.open(encoding="utf-8") as f:
            reader = csv.DictReader(f, delimiter="\t")
            for n, row in enumerate(reader, start=2):
                key = row.get("site_key", "")
                cls = row.get("class_id", "")
                if key in mapping:
                    errors.append(f"{map_path}:{n}: site_key 重复 {key}")
                mapping[key] = cls
    known_classes = {
        row["class_id"]
        for row in csv.DictReader(args.ledger.open(encoding="utf-8"), delimiter="\t")
        if row.get("class_id")
    } if args.ledger.exists() else set()
    if not args.ledger.exists():
        errors.append(f"{args.ledger}: 无法读取 SA 账本")
    defaults = dict(part.split("=", 1) for part in args.default_class.split(",") if part)
    missing = sorted(set(enumerated) - set(mapping))
    if missing and args.init_map and sources_ok:
        with map_path.open("a", encoding="utf-8", newline="") as f:
            writer = csv.writer(f, delimiter="\t", lineterminator="\n")
            if not map_path.exists():
                f.write("site_key\tclass_id\tnote\n")
            for key in missing:
                tool = key.split("|", 1)[0]
                cls = defaults.get(tool, "")
                writer.writerow([key, cls, "默认归属工具 census 类；细化映射在此行改写"])
        mapping.update({k: defaults.get(k.split("|", 1)[0], "") for k in missing})
        missing = []
    for key in missing:
        errors.append(f"站点未映射: {key}")
    stale = sorted(set(mapping) - set(enumerated))
    for key in stale[:20]:
        errors.append(f"映射失效（站点已不存在）: {key}")
    if stale:
        errors.append(f"共 {len(stale)} 条失效映射")
    bad_class = sorted({k for k, v in mapping.items() if v and v not in known_classes})
    for key in bad_class[:20]:
        errors.append(f"{key}: 类 ID {mapping[key]} 不在 SA 账本")
    if errors:
        for error in errors:
            print(f"audit: {error}", file=sys.stderr)
        return 1
    total = len(enumerated)
    print(f"census-per-site: {len(census)} 工具 {total} 站点 100% 映射，census 计数一致")
    return 0


def drift_record(args: argparse.Namespace) -> int:
    """跨版本漂移检测仪·记录端：钉死版本的探针输出摘要入库。"""
    out_dir = pathlib.Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    recorded = skipped = 0
    for row in csv.DictReader(args.defs.open(encoding="utf-8"), delimiter="\t"):
        tool, probe = row["tool"], row["probe"]
        try:
            proc = subprocess.run(["sh", "-c", probe], capture_output=True, text=True,
                                  errors="replace", timeout=30)
        except (OSError, subprocess.SubprocessError) as exc:
            print(f"drift: {tool} 探针无法执行 {exc}", file=sys.stderr)
            return 1
        if proc.returncode == 127 or "not found" in proc.stderr:
            payload = {"tool": tool, "version": row["version"], "probe": probe,
                       "status": "missing", "at": datetime_now()}
            skipped += 1
        else:
            output = (proc.stdout + proc.stderr).strip()
            payload = {"tool": tool, "version": row["version"], "probe": probe,
                       "status": "ok", "output": output,
                       "sha256": hashlib.sha256(output.encode("utf-8")).hexdigest(),
                       "at": datetime_now()}
            recorded += 1
        (out_dir / f"{tool}.json").write_text(
            json.dumps(payload, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"drift-record: 记录 {recorded}，缺二进制记 missing {skipped}（升级后 drift-check 对照）")
    return 0


def drift_check(args: argparse.Namespace) -> int:
    """跨版本漂移检测仪·对照端：探针输出与入库摘要不符即红。"""
    errors: list[str] = []
    checked = missing = 0
    for row in csv.DictReader(args.defs.open(encoding="utf-8"), delimiter="\t"):
        tool, probe = row["tool"], row["probe"]
        baseline_path = pathlib.Path(args.out) / f"{tool}.json"
        if not baseline_path.exists():
            errors.append(f"{tool}: 无入库摘要，先跑 drift-record")
            continue
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
        if baseline.get("status") == "missing":
            missing += 1
            continue
        try:
            proc = subprocess.run(["sh", "-c", probe], capture_output=True, text=True,
                                  errors="replace", timeout=30)
        except (OSError, subprocess.SubprocessError) as exc:
            errors.append(f"{tool}: 探针无法执行 {exc}")
            continue
        output = (proc.stdout + proc.stderr).strip()
        sha = hashlib.sha256(output.encode("utf-8")).hexdigest()
        if sha != baseline.get("sha256"):
            errors.append(
                f"{tool}: 探针输出漂移（pinned {baseline.get('version')}，"
                f"旧 sha {str(baseline.get('sha256'))[:12]}，新 sha {sha[:12]}）——"
                f"先人工确认行为再 drift-record 重新入库"
            )
        else:
            checked += 1
    if errors:
        for error in errors:
            print(f"drift: {error}", file=sys.stderr)
        return 1
    print(f"drift-check: {checked} 一致，missing 跳过 {missing}（升级时必红必查）")
    return 0


def datetime_now() -> str:
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def selftest() -> int:
    good = {
        "class_id": "SA1-001",
        "source": "parted",
        "source_loc": "NEWS:107",
        "category": "修复",
        "description": "x",
        "behavior": "y",
        "evidence": "tests/ok.rs::a_case",
        "state": "已认领",
        "reason": "",
    }
    cases = [
        ({**good, "state": "已认领", "evidence": "tests/ok.rs::missing"}, True),
        ({**good, "state": "已认领", "evidence": "R9999"}, True),
        ({**good, "state": "已登记缺口", "evidence": "owner:SA9"}, True),
        ({**good, "state": "范围裁剪", "evidence": "", "reason": "一;二;三"}, True),
        ({**good, "state": "已完成", "evidence": ""}, True),
        (good, False),
    ]
    failures = 0
    with __import__("tempfile").TemporaryDirectory() as temporary:
        root = pathlib.Path(temporary)
        (root / "tests").mkdir()
        (root / "tests" / "ok.rs").write_text("fn a_case() {}\n", encoding="utf-8")
        for row, expected in cases:
            errors = validate(
                [row],
                {row["class_id"]: row["state"]},
                {"R0602"},
                {"D8"},
                {"SA1", "SA2"},
                root / "tests",
                root,
                {},
            )
            if bool(errors) != expected:
                print(f"selftest: 期望失败={expected} 得到 {errors}", file=sys.stderr)
                failures += 1
        source = root / "src"
        source.mkdir()
        (source / "a.c").write_text(
            "void f(void) { if (x) goto fail; PED_ASSERT(1); ped_exception_throw(1); }\n",
            encoding="utf-8",
        )
        if count_defenses(source) != (1, 1, 1, 1):
            print("selftest: 防御点计数口径不对", file=sys.stderr)
            failures += 1
        census_row = {column: "0" for column in CENSUS_COLUMNS}
        census_row["tool"] = "parted"
        census_row["total"] = "3"
        errors = validate(
            [{**good, "evidence": "census:parted"}],
            {"SA1-001": "已认领"},
            set(),
            set(),
            {"SA1"},
            root / "tests",
            root,
            {"parted": census_row},
        )
        if errors:
            print(f"selftest: census 证据未被接受：{errors}", file=sys.stderr)
            failures += 1
    print(f"selftest: {'ok' if not failures else str(failures) + ' 项失败'}")
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description="SA 事故与隐式行为账本审计")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("selftest")
    run = sub.add_parser("check")
    run.add_argument("--ledger", type=pathlib.Path, required=True)
    run.add_argument("--m4", type=pathlib.Path, required=True)
    run.add_argument("--appendix", type=pathlib.Path, required=True)
    run.add_argument("--divergences", type=pathlib.Path, required=True)
    run.add_argument("--refusal-map", type=pathlib.Path, required=True)
    run.add_argument("--tests", type=pathlib.Path, required=True)
    run.add_argument("--census", type=pathlib.Path, required=True)
    run.add_argument("--root", type=pathlib.Path, default=pathlib.Path("."))
    count = sub.add_parser("census")
    count.add_argument("--source", type=pathlib.Path, required=True)
    count.add_argument("--tool", required=True)
    count.add_argument("--version", required=True)
    count.add_argument("--out", type=pathlib.Path, required=True)
    disp = sub.add_parser("check-disposition")
    disp.add_argument("--ref", type=pathlib.Path, required=True)
    disp.add_argument("--disp", type=pathlib.Path, required=True)
    per = sub.add_parser("census-per-site")
    per.add_argument("--census", type=pathlib.Path, required=True)
    per.add_argument("--ledger", type=pathlib.Path, required=True)
    per.add_argument("--map", type=pathlib.Path, required=True)
    per.add_argument("--out", type=pathlib.Path)
    per.add_argument("--init-map", action="store_true")
    per.add_argument("--default-class", default="parted=SA1-021,libfdisk=SA1-022,gdisk=SA1-023")
    drec = sub.add_parser("drift-record")
    drec.add_argument("--defs", type=pathlib.Path, required=True)
    drec.add_argument("--out", type=pathlib.Path, required=True)
    dchk = sub.add_parser("drift-check")
    dchk.add_argument("--defs", type=pathlib.Path, required=True)
    dchk.add_argument("--out", type=pathlib.Path, required=True)
    args = parser.parse_args()
    if args.command == "selftest":
        return selftest()
    if args.command == "census":
        files, asserts, exceptions, gotos = count_defenses(args.source)
        total = asserts + exceptions + gotos
        write_census(
            args.out,
            {
                "tool": args.tool,
                "version": args.version,
                "source": str(args.source),
                "files": str(files),
                "asserts": str(asserts),
                "exceptions": str(exceptions),
                "goto_fail": str(gotos),
                "total": str(total),
            },
        )
        print(f"census: {args.tool} {args.version} files={files} total={total}")
        return 0
    if args.command == "check-disposition":
        return check_disposition(args)
    if args.command == "census-per-site":
        return census_per_site(args)
    if args.command == "drift-record":
        return drift_record(args)
    if args.command == "drift-check":
        return drift_check(args)
    return check(args)


if __name__ == "__main__":
    raise SystemExit(main())
