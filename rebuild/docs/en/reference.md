Generated from `app/cli/declare.rs`, `app/report/exit.rs` and `diskedit_core::fs::catalog` by `docsgen`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`.
CI compares the working tree with the generator output. Do not edit by hand.

# diskedit command reference

Write commands on a block device without a WWN, EUI or repeatable serial
(nbd, rbd, dm, md) need `--identity NAME`; images and devices with an
identity of their own reject the flag.

## Commands

### `info`

show partition table and per-partition filesystem identification

```text
diskedit info <TARGET> [--sector-size N]
diskedit info <TARGET>:N --grow [--sector-size N]
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--grow`, `--sector-size` |

### `inspect`

show per-partition extents and content

```text
diskedit inspect <TARGET|PATH>
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--identity` |

### `records list`

list recovery record directory entries

```text
diskedit records list [TARGET]
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--identity` |

### `records verify`

verify one recovery record

```text
diskedit records verify <TARGET> <ID>
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--identity` |

### `records scan`

scan a recovery record container

```text
diskedit records scan <ROOT>
```

| property | value | options |
|---|---|---|
| with or without a suffix | none | — |

### `resize`

resize a partition and its filesystem

```text
diskedit resize <TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes
diskedit resize <TARGET>:N --end LBA [--align A] [--no-fs] --yes
diskedit resize <TARGET>:N --grow-to-end [--align A] [--no-fs] --yes
```

| property | value | options |
|---|---|---|
| with or without a suffix | required (`--yes`) | `--align`, `--allow-move`, `--chunk`, `--dry-run`, `--end`, `--grow-lv`, `--grow-to-end`, `--identity`, `--lv`, `--no-fs`, `--sector-size`, `--yes` |

### `move`

relocate a partition

```text
diskedit move <TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--align`, `--chunk`, `--identity`, `--sector-size`, `--start`, `--yes` |

### `copy`

copy a partition to a new location and add its entry

```text
diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--align`, `--chunk`, `--identity`, `--keep-partuuid`, `--name`, `--sector-size`, `--start`, `--yes` |

### `delete`

delete a partition entry

```text
diskedit delete <TARGET>:N --yes
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--identity`, `--sector-size`, `--yes` |

### `set`

set a partition or filesystem attribute

```text
diskedit set <TARGET>:N name S --yes
diskedit set <TARGET>:N label S --yes
diskedit set <TARGET>:N uuid [U] --yes [--random]
diskedit set <TARGET>:N flag F on|off --yes
diskedit set <TARGET>:N type GUID|0xXX --yes
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--identity`, `--random`, `--sector-size`, `--yes` |

### `check`

check the filesystem in a partition

```text
diskedit check <TARGET>:N
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | none | `--sector-size` |

### `mkfs`

create a filesystem in a partition

```text
diskedit mkfs <TARGET>:N <FS> --yes [--label S]
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--identity`, `--label`, `--sector-size`, `--yes` |

### `resizefs`

resize the filesystem inside a partition

```text
diskedit resizefs <TARGET>:N [BYTES] --yes
diskedit resizefs <MOUNTPOINT> [BYTES] --yes
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--identity`, `--sector-size`, `--yes` |

### `recover`

roll back, replay or release a recovery record

```text
diskedit recover <TARGET> --yes
diskedit recover <TARGET> --replay-table-only --yes
diskedit recover <TARGET> --release [--discard-replay] --yes
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--discard-replay`, `--identity`, `--release`, `--replay-table-only`, `--sector-size`, `--yes` |

## Low-level

### `new`

create a fresh partition table

```text
diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--identity`, `--overwrite-data`, `--sector-size`, `--table`, `--yes` |

### `add`

add a partition

```text
diskedit add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--align`, `--identity`, `--name`, `--part-number`, `--sector-size`, `--start`, `--type-guid`, `--yes` |

## options

| option | value | description |
|---|---|---|
| `--align` | `A` | placement alignment: mib|none |
| `--allow-move` | — | relocate blocking partitions after printing the plan |
| `--chunk` | `MiB` | resume granularity in MiB, 1..=64; default 4 for new jobs |
| `--discard-replay` | — | confirm giving up a range `recover` could still replay |
| `--dry-run` | — | print the plan and stop before the first write; no --yes needed |
| `--end` | `LBA` | new end LBA for in-place resizing |
| `--grow` | — | append a relocation preview for partition :N |
| `--grow-lv` | — | also grow the LV: the one named by --lv, else the only LV |
| `--grow-to-end` | — | pin the end at the last usable LBA (fill semantics) |
| `--identity` | `NAME` | name a target without WWN, EUI or repeatable serial (nbd, rbd, dm, md) |
| `--keep-partuuid` | — | reuse the source PARTUUID instead of generating a new one |
| `--label` | `S` | filesystem label (omitted means an empty label, not a default) |
| `--lv` | `NAME` | the LV to grow (implies --grow-lv) |
| `--name` | `S` | entry name for the partition this command creates |
| `--no-fs` | — | skip the filesystem step of the grown partition |
| `--overwrite-data` | — | accept overwriting bytes an existing partition holds |
| `--part-number` | `N` | 1-based entry slot; it must be empty |
| `--random` | — | ask the target to generate a new value |
| `--release` | — | drop the recovery files, keeping the writes they describe |
| `--replay-table-only` | — | replay only the recorded table regions of a completed move |
| `--sector-size` | `N` | logical sector size override for raw images (power of two, at most 65536) |
| `--start` | `LBA|end|align` | new start: an LBA, `end` for tail packing, or `align` for the next 1 MiB boundary |
| `--table` | `gpt|mbr` | partition table type: gpt (default) or mbr |
| `--type-guid` | `GUID|0xNN` | GPT type GUID text, or an MBR type byte 0xNN |
| `--yes` | — | acknowledge a write before it starts |
