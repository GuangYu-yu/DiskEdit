#!/usr/bin/env python3
"""Build-side worker integrity check for the VM carrier bundle.

`tools/vm/build.sh` packs the guest worker binary into the initramfs (a
gzip-compressed newc cpio archive). Host-side admission only compares the
kernel/initramfs files on disk against the manifest; the worker digest is
registered there as an audit fact. This script closes that gap at build time:
it decompresses the archive, seeks the single cpio entry holding the guest
worker (`bin/diskedit`), and compares that entry's SHA-256 against the digest
recorded for the worker.

Only the worker entry is read out; every other entry is skipped by offset
arithmetic. No external `cpio` tool is used — the newc header layout mirrors
the writer in `build.sh` (magic `070701`, 13 eight-hex fields, name + NUL,
then a 4-byte padded name region and a 4-byte padded payload).

Usage:
    verify_worker.py --check <initramfs> --entry <cpio name> --expect <sha256 hex>
    verify_worker.py --self-test
"""

import argparse
import gzip
import hashlib
import sys

MAGIC = b"070701"
HEADER_LEN = 110  # 6-byte magic + 13 fields x 8 hex chars
TRAILER = "TRAILER!!!"


class EntryError(Exception):
    """The archive is malformed or the requested entry is absent."""


def _align4(size):
    return (size + 3) & ~3


def newc_entry(name, mode, data):
    """Mirror the writer in `build.sh`; keep the two in sync.

    Fields: ino, mode, uid, gid, nlink, mtime, filesize, devmajor,
    devminor, rdevmajor, rdevminor, namesize, check.
    """
    fields = [0, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name) + 1, 0]
    header = MAGIC.decode() + "".join(f"{f:08x}" for f in fields)
    blob = header.encode() + name.encode() + b"\0"
    blob += b"\0" * ((4 - len(blob) % 4) % 4)
    blob += data
    blob += b"\0" * ((4 - len(data) % 4) % 4)
    return blob


def _decompress(path):
    with open(path, "rb") as f:
        raw = f.read()
    try:
        return gzip.decompress(raw)
    except (OSError, EOFError):
        # Tolerate an uncompressed newc archive; the build always gzips.
        return raw


def find_entry(archive, target):
    """Return the payload of the cpio entry named `target`.

    Headers are walked by offset; non-target entries are skipped without
    materializing their payload. Raises `EntryError` on a malformed archive
    or a missing entry.
    """
    offset = 0
    while offset + HEADER_LEN <= len(archive):
        magic = archive[offset:offset + 6]
        if magic != MAGIC:
            raise EntryError(f"not a newc archive at offset {offset}: bad magic {magic!r}")
        fields = []
        for i in range(13):
            chunk = archive[offset + 6 + i * 8:offset + 14 + i * 8]
            fields.append(int(chunk, 16))
        filesize = fields[6]
        namesize = fields[11]
        name_start = offset + HEADER_LEN
        name_region = archive[name_start:name_start + namesize]
        name = name_region.rstrip(b"\0").decode("utf-8", errors="replace")
        data_start = _align4(name_start + namesize)
        data_end = data_start + filesize
        if name == target:
            return archive[data_start:data_end]
        if name == TRAILER:
            break
        offset = _align4(data_end)
    raise EntryError(f"cpio entry `{target}` not found in initramfs")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def verify_bytes(archive, entry, expected_hex):
    """Return (ok, message); the message explains a failure."""
    try:
        data = find_entry(archive, entry)
    except EntryError as exc:
        return False, f"worker digest mismatch: {exc}"
    actual = sha256_hex(data)
    if actual != expected_hex.lower():
        return False, (
            "worker digest mismatch: initramfs 里的 worker 与 manifest 的 "
            f"worker 摘要不一致 (entry `{entry}`, expected {expected_hex}, "
            f"archive {actual})"
        )
    return True, f"worker digest matches entry `{entry}` ({actual})"


def verify_file(path, entry, expected_hex):
    return verify_bytes(_decompress(path), entry, expected_hex)


def _self_test():
    worker = b"\x7fELF-synthetic-worker\n" + b"payload" * 16
    other = b"\x7fELF-tampered-worker\n" + b"payload" * 16
    expected = sha256_hex(worker)

    def pack(worker_bytes):
        parts = [
            newc_entry(".", 0o040755, b""),
            newc_entry("bin/diskedit", 0o100755, worker_bytes),
            newc_entry("bin/sh", 0o100755, b"#!/bin/sh\n"),
            newc_entry(TRAILER, 0, b""),
        ]
        return gzip.compress(b"".join(parts), compresslevel=9, mtime=0)

    cases = [
        ("matching worker", pack(worker), expected, True),
        ("tampered worker", pack(other), expected, False),
    ]
    failures = 0
    for label, archive, digest, should_pass in cases:
        ok, message = verify_bytes(gzip.decompress(archive), "bin/diskedit", digest)
        print(f"[{'ok' if ok else 'rejected'}] {label}: {message}")
        if ok != should_pass:
            print(f"  FAIL: expected {'pass' if should_pass else 'reject'}", file=sys.stderr)
            failures += 1
        elif not ok and "mismatch" not in message:
            print("  FAIL: rejection message lacks 'mismatch'", file=sys.stderr)
            failures += 1

    parts = [newc_entry(".", 0o040755, b""), newc_entry(TRAILER, 0, b"")]
    ok, message = verify_bytes(b"".join(parts), "bin/diskedit", expected)
    print(f"[{'ok' if ok else 'rejected'}] missing entry: {message}")
    if ok:
        failures += 1

    if failures:
        print(f"self-test: {failures} failure(s)", file=sys.stderr)
        return 1
    print("self-test: all checks passed")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", metavar="INITRAMFS", help="initramfs archive to verify")
    parser.add_argument("--entry", help="cpio entry holding the worker (e.g. bin/diskedit)")
    parser.add_argument("--expect", help="expected worker SHA-256 hex (manifest worker.sha256)")
    parser.add_argument("--self-test", action="store_true", help="run the synthetic archive self-test")
    args = parser.parse_args(argv)

    if args.self_test:
        return _self_test()

    missing = [
        flag for flag, value in (("--check", args.check), ("--entry", args.entry), ("--expect", args.expect))
        if not value
    ]
    if missing:
        parser.error(f"missing arguments: {', '.join(missing)}")

    ok, message = verify_file(args.check, args.entry, args.expect)
    if ok:
        print(message)
        return 0
    print(f"ERROR: {message}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
