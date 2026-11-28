Generated from `app/cli/declare.rs`, `app/report/exit.rs` and `diskedit_core::fs::catalog` by `docsgen`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`.
CI compares the working tree with the generator output. Do not edit by hand.

# DiskEdit exit status

Seven values; the mapping lives in `app/report/exit.rs`.

| code | meaning |
|---|---|
| 0 | success: nothing written and no record created, or the record was cleaned up and the kernel view is in sync, or a full rollback completed |
| 1 | internal error or a broken invariant before any target byte was written |
| 2 | usage or parse error |
| 3 | request refused, record damaged, or evidence points at a foreign or damaged object |
| 4 | I/O or environment failure after execution began, with no target bytes changed |
| 5 | partial completion, a record awaits recovery, or persistent state is reached while the kernel view is stale |
| 6 | capability unsupported |
