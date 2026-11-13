# diskedit capability matrix

Generated from `CommandDef.capabilities` and `CapabilitySnapshot::design(platform, media)` by `src/docs.rs`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --bin DiskEdit docs_are_generated_in_sync`;
CI regenerates it and fails when the working tree differs. Do not edit by hand.

## Capability states

| capability | linux / block | linux / image | windows / block | windows / image | other / block | other / image |
|---|---|---|---|---|---|---|
| ReadTarget | supported | supported | unsupported | supported | unsupported | supported |
| WriteTable | supported | supported | unsupported | unsupported | unsupported | unsupported |
| SectorIo | supported | supported | unsupported | unsupported | unsupported | unsupported |
| ExternalTool | supported | supported | unsupported | unsupported | unsupported | unsupported |
| KernelResync | supported | not applicable | unsupported | not applicable | unsupported | not applicable |
| ExclusiveLock | supported | supported | design limit | design limit | unsupported | unsupported |
| AtomicReplace | supported | supported | unsupported | unsupported | unsupported | unsupported |
| DirPersist | supported | supported | design limit | design limit | unsupported | unsupported |
| StableIdentity | supported | supported | unsupported | supported | unsupported | supported |

## Commands

| command | linux / block | linux / image | windows / block | windows / image | other / block | other / image |
|---|---|---|---|---|---|---|
| `info` | admitted | admitted | blocked: ReadTarget (unsupported) | admitted | blocked: ReadTarget (unsupported) | admitted |
| `inspect` | admitted | admitted | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) |
| `records list` | admitted | admitted | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) |
| `records verify` | admitted | admitted | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) |
| `records scan` | admitted | admitted | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) | blocked: ReadTarget (unsupported) | blocked: AtomicReplace (unsupported) |
| `resize` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `move` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `copy` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `delete` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `set` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `check` | admitted | admitted | blocked: ReadTarget (unsupported) | blocked: ExternalTool (unsupported) | blocked: ReadTarget (unsupported) | blocked: ExternalTool (unsupported) |
| `mkfs` | admitted | admitted | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) |
| `resizefs` | admitted | admitted | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) |
| `undo` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `abandon` | admitted | admitted | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) | blocked: AtomicReplace (unsupported) |
| `new` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `add` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `resize-part` | admitted | admitted | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) | blocked: WriteTable (unsupported) |
| `plan` | admitted | admitted | blocked: ReadTarget (unsupported) | admitted | blocked: ReadTarget (unsupported) | admitted |

## Runtime gate

The gate evaluates only the target shapes the build can open. This table lists the 2 target shapes it evaluates on Linux (`HostPlatform::openable_media`): linux / block, linux / image. Other shapes do not enter the gate's decision — `cmd::capability_gap` iterates only the openable set.
The table is generated from the Linux capability set; a non-Linux build's openable set is decided at run time by `HostPlatform::openable_media`, and this document does not copy it.

| command | linux / block | linux / image |
|---|---|---|
| `info` | admitted | admitted |
| `inspect` | admitted | admitted |
| `records list` | admitted | admitted |
| `records verify` | admitted | admitted |
| `records scan` | admitted | admitted |
| `resize` | admitted | admitted |
| `move` | admitted | admitted |
| `copy` | admitted | admitted |
| `delete` | admitted | admitted |
| `set` | admitted | admitted |
| `check` | admitted | admitted |
| `mkfs` | admitted | admitted |
| `resizefs` | admitted | admitted |
| `undo` | admitted | admitted |
| `abandon` | admitted | admitted |
| `new` | admitted | admitted |
| `add` | admitted | admitted |
| `resize-part` | admitted | admitted |
| `plan` | admitted | admitted |

The process-level exit prints the cause sentence of a failure. Exit 3 and exit 4 are followed by a second line, `— this run changed no persistent state`; exit 6 prints `unsupported: <cause>` and appends no such line.