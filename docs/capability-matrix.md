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
| UsageQuery | supported | not applicable | unsupported | not applicable | unsupported | not applicable |
| StableIdentity | supported | supported | unsupported | supported | unsupported | supported |
| OnlineTableEdit | supported | not applicable | unsupported | not applicable | unsupported | not applicable |
| KernelSectorSize | supported | not applicable | unsupported | not applicable | unsupported | not applicable |

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

A cell records whether that platform/media pair has the primitives the command needs.
The runtime gate reads the same matrix, restricted to the media the host can actually open
(`HostPlatform::openable_media`): Linux opens block devices and image files; other hosts open
image files only. A read-only command therefore runs on a non-Linux image file, while any
command needing a write-side or evidence-side primitive answers exit 6.