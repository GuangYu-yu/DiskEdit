# Fallbacks and refusals

A fallback is not a way to make a command succeed. It is a decision about whether to continue when a fact is missing. This tool is responsible for the records it creates, the changes it starts, and the recovery of its own interrupted work. It is not responsible for repairing disks, reconstructing foreign metadata, or rescuing data. When the facts are not available, the safe default is to refuse.

The first question is whether the fact can be known. Sector size, partition geometry, device identity, probe results, and whether an external step completed are not values to guess. A wrong guess in a write, shrink, delete, or rebuild is not a failed command; it is damaged data.

The second question is whether a failure is visible. A record that cannot be read, a parse error, a permission problem, or an I/O error must travel upward as an error. It must not become "no record", an empty set, or a default value. The only exception is a failure that is explicitly modelled as unknown, with every later branch treating unknown as a reason not to continue.

The third question is who owns the state. A record left by an earlier run of this tool, a table this tool wrote halfway, or an external step this tool started is inside our responsibility. Another tool's record, a damaged backup header, or a partition table of unknown origin is not. Such state is preserved and refused: it is not repaired, adopted, or deleted.

Before continuing, check whether continuing is safe under every interpretation. If the same observation can mean two things and one of them would destroy data, do not continue. A degraded path is allowed only when none of the possible meanings can cause an irreversible result, and it must leave something observable behind. Silent degradation is not allowed.

If a degraded step fails halfway, the state it leaves must be recognizable by the next run. A half-written, half-changed, or half-completed operation must not look successful. Retries are allowed only after the write fact is recorded and only when repeating the step cannot duplicate a side effect. Retry, compatibility, and repair are different things and cannot substitute for one another.

Every fallback that remains must justify itself. It needs a test that fails when the fallback is removed; an empty branch with no such test is either given one or deleted. Compatibility with older records, retrying after the kernel view lags, and reporting an unsupported platform are legitimate categories, but each has a boundary: old formats only for records this tool created with a known schema, retries only after the write fact is set, and unsupported platforms only refused, never emulated.

Finally, a fallback must not expand the tool's responsibility. Supporting a format, platform, or repair scenario that was never promised, or bypassing permission, ownership, or identity checks to make progress, is not allowed. A fallback must not quietly change what the user sees. If it changes the result, the user has to know.

In one sentence: continue only when the fact is known; refuse when it is not; recover our own state, leave other state alone; and any continuation must be observable, recoverable, and testable.