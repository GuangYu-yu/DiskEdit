Generated from `app/cli/declare.rs`, `app/report/exit.rs` and `diskedit_core::fs::catalog` by `docsgen`.
Regenerate with `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs`.
CI compares the working tree with the generator output. Do not edit by hand.

# diskedit command reference

Write commands on a block device without a WWN, EUI or repeatable serial
(nbd, rbd, dm, md) need `--alias NAME`; images and devices with an
identity of their own reject the flag.

## Commands

### `layout`

show the partition table and per-partition layout

```text
diskedit layout <TARGET> [--sector-size N] [--json]
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--json`, `--sector-size` |

### `records list`

list recovery record directory entries

```text
diskedit records list [TARGET] [--root ROOT]
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--alias`, `--root` |

### `records verify`

verify one recovery record

```text
diskedit records verify <TARGET> <ID>
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--alias` |

### `records show`

show the recovery record site of a target

```text
diskedit records show <TARGET>
```

| property | value | options |
|---|---|---|
| no partition suffix | none | `--alias` |

### `undo`

roll back the recorded write and remove the recovery record

```text
diskedit undo <TARGET> --yes
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--alias`, `--sector-size`, `--yes` |

### `replay`

replay the recorded table regions of a completed move

```text
diskedit replay <TARGET> --yes
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--alias`, `--sector-size`, `--yes` |

### `release`

give up a recovery record and keep the writes it describes

```text
diskedit release <TARGET> [--discard-replay] --yes
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--alias`, `--discard-replay`, `--sector-size`, `--yes` |

### `resize`

resize a partition and its filesystem

```text
diskedit resize <TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes
diskedit resize <TARGET>:N --end LBA [--align A] [--no-fs] --yes
diskedit resize <TARGET>:N --grow-to-end [--align A] [--no-fs] --yes
diskedit resize <TARGET>:N --fs-only --yes
diskedit resize <MOUNTPOINT> [BYTES] --fs-only --yes
```

| property | value | options |
|---|---|---|
| with or without a suffix | required (`--yes`) | `--alias`, `--align`, `--allow-move`, `--chunk`, `--dry-run`, `--end`, `--fs-only`, `--grow-lv`, `--grow-to-end`, `--lv`, `--no-fs`, `--sector-size`, `--yes` |

### `move`

relocate a partition

```text
diskedit move <TARGET>:N --start <LBA|end|align> [--chunk N] --yes [--align A]
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--alias`, `--align`, `--chunk`, `--sector-size`, `--start`, `--yes` |

### `copy`

copy a partition to a new location and add its entry

```text
diskedit copy <TARGET>:N --start <LBA|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--alias`, `--align`, `--chunk`, `--keep-partuuid`, `--name`, `--sector-size`, `--start`, `--yes` |

### `delete`

delete a partition entry

```text
diskedit delete <TARGET>:N --yes
```

| property | value | options |
|---|---|---|
| `:N` or `:last` required | required (`--yes`) | `--alias`, `--sector-size`, `--yes` |

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
| `:N` or `:last` required | required (`--yes`) | `--alias`, `--random`, `--sector-size`, `--yes` |

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
| `:N` or `:last` required | required (`--yes`) | `--alias`, `--label`, `--sector-size`, `--yes` |

## Low-level

### `new`

create a fresh partition table

```text
diskedit new <TARGET> [--table gpt|mbr] --yes [--overwrite-data]
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--alias`, `--overwrite-data`, `--sector-size`, `--table`, `--yes` |

### `add`

add a partition

```text
diskedit add <TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] [--dry-run] --yes
```

| property | value | options |
|---|---|---|
| no partition suffix | required (`--yes`) | `--alias`, `--align`, `--dry-run`, `--name`, `--part-number`, `--sector-size`, `--start`, `--type-guid`, `--yes` |

## options

| option | value | description |
|---|---|---|
| `--alias` | `NAME` | name a target without WWN, EUI or repeatable serial (nbd, rbd, dm, md) |
| `--align` | `A` | placement alignment: mib|none |
| `--allow-move` | — | relocate blocking partitions after printing the plan |
| `--chunk` | `MiB` | resume granularity in MiB, 1..=64; default 4 for new jobs |
| `--discard-replay` | — | confirm giving up a range `diskedit replay` could still replay |
| `--dry-run` | — | print the plan and stop before the first write; no --yes needed |
| `--end` | `LBA` | new end LBA for in-place resizing |
| `--fs-only` | — | resize only the filesystem: grow it into its partition, or through a mountpoint |
| `--grow-lv` | — | also grow the LV: the one named by --lv, else the only LV |
| `--grow-to-end` | — | pin the end at the last usable LBA (fill semantics) |
| `--json` | — | print the structured per-partition detail instead of the text layout |
| `--keep-partuuid` | — | reuse the source PARTUUID instead of generating a new one |
| `--label` | `S` | filesystem label (omitted means an empty label, not a default) |
| `--lv` | `NAME` | the LV to grow (implies --grow-lv) |
| `--name` | `S` | entry name for the partition this command creates |
| `--no-fs` | — | skip the filesystem step of the grown partition |
| `--overwrite-data` | — | accept overwriting bytes an existing partition holds |
| `--part-number` | `N` | 1-based entry slot; it must be empty |
| `--root` | `ROOT` | scan a record container directory instead of listing a target's record |
| `--random` | — | ask the target to generate a new value |
| `--sector-size` | `N` | logical sector size override for raw images (power of two, at most 65536) |
| `--start` | `LBA|end|align` | new start: an LBA, `end` for tail packing, or `align` for the next 1 MiB boundary |
| `--table` | `gpt|mbr` | partition table type: gpt (default) or mbr |
| `--type-guid` | `GUID|0xNN` | GPT type GUID text, or an MBR type byte 0xNN |
| `--yes` | — | acknowledge a write before it starts |
