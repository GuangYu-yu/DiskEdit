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
"""

from __future__ import annotations

import argparse
import pathlib
import re
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
) -> list[str]:
    errors: list[str] = []
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
        validate(rows, m4, known_r, known_d, batches, args.tests, args.root)
    )
    if errors:
        for error in errors:
            print(f"audit: {error}", file=sys.stderr)
        return 1
    print(f"audit: {len(rows)} 类全部有归属与证据")
    return 0


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
            )
            if bool(errors) != expected:
                print(f"selftest: 期望失败={expected} 得到 {errors}", file=sys.stderr)
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
    run.add_argument("--root", type=pathlib.Path, default=pathlib.Path("."))
    args = parser.parse_args()
    return selftest() if args.command == "selftest" else check(args)


if __name__ == "__main__":
    raise SystemExit(main())
