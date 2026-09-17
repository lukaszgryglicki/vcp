# vcp

`vcp` — **v**isible **cp**. A FreeBSD [cp(1)](https://man.freebsd.org/cgi/man.cgi?cp(1))-compatible
copy program that shows Midnight-Commander-like progress bars while copying.
Written in Rust, plain ASCII output (no curses) — works in text mode and over ssh.

## Progress display

Progress is drawn on **stderr**, only when stderr is a terminal, and never
interferes with `-v` output, prompts or warnings (bars are cleared, the
message is printed, bars are redrawn).

Copying a single file — one full-width bar plus the speed and ETA lines:

```
|=================== (bigfile.iso    34.01%) =====>                            |
| speed: current    4.213 M/s    average    3.987 M/s                          |
| eta:   current          1h 32m 16s    average          1h 40m 02s            |
```

Copying multiple files/directories — three labeled bars (overall count,
overall bytes, current file) plus the speed and ETA lines:

```
count: |=========== ( 12/345     3.48%) =>                                    |
bytes: |======== ( 259.31M/2.03T    12.34%) ==>                               |
file:  |========== (...ath/to/current/file.c    45.00%) ====>                 |
speed: | current    4.213 M/s    average    3.987 M/s                         |
eta:   | current          1h 32m 16s    average          1h 40m 02s           |
```

The speed and ETA lines are always present: *current* is the transfer rate
over the last 3 seconds, *average* the rate since copying started, and the
ETA line shows the time remaining at each of the two rates. Speeds are
fixed-width fields (`XXXX.XXX U/s`, unit auto-scaled B/K/M/G/T); ETAs are
fixed-width too, human readable with seconds precision and weeks as the
largest unit (`1w 3d 1h 32m 16s`, `59m 07s`, `12s`), showing `unknown`
while there is no rate yet (or the copy has stalled), `>999w` for hopeless
cases and `done` at the end. Nothing moves while the numbers change. Only
bytes actually transferred count (skipped or failed files do not inflate
the speed).

The `(...)` status text in the middle is **not** part of the gauge: the
fill track is the cells left of `(` plus the cells right of `)`. Reaching
`|=====(` means about half done; on the next tick the arrow re-appears
after `)` and continues to the right edge (`===(...)>`, `===(...)=>`, ...).

Numeric fields have a constant width for the whole copy: the bytes bar
starts at `(   0.00B/1.01G     0.00%)` and counts up to
`(1010.50M/1.01G    99.90%)` with the *done* value right-aligned in a
fixed-width field, and the file counter is padded the same way
(` 12/345`), so `(`, `/`, `%` and `)` never move while copying. The
current file name field is likewise constant-width for each file.

Long file names are shortened with an ASCII ellipsis whose position
*slides* back and forth across the name on each tick (`...ng/path/file.c`
→ `lo...path/file.c` → `long/pa...file.c` → `long/path/fi...` → back), so
much more of the name can be read over a few seconds while the line width
stays constant.

Bar width: set `COLUMNS` explicitly to force a width (`COLUMNS=100 vcp -R
src dst`); otherwise the terminal width is used. Bars are never drawn
narrower than 80 columns — smaller `COLUMNS` values are ignored, a
narrower terminal is treated as 80 wide, and 80 is the fallback when the
width cannot be determined. Redrawing
uses `\r` and minimal ANSI cursor-up/erase sequences — no curses, ASCII bar
content only.

On FreeBSD, `SIGINFO` (Ctrl+T) additionally prints a `src -> dst NN%` status
line, exactly like the system cp.

## Usage

```
usage: vcp [-R [-H | -L | -P]] [-f | -i | -n] [-alpsvx] source_file target_file
       vcp [-R [-H | -L | -P]] [-f | -i | -n] [-alpsvx] source_file ... target_directory
```

Options are compatible with FreeBSD 15 cp(1), including the long variants:

| Option | Long | Meaning |
|--------|------|---------|
| `-R`   | `--recursive` | copy hierarchies recursively |
| `-H`   | | with `-R`: follow symlinks on the command line |
| `-L`   | `--dereference` | with `-R`: follow all symlinks |
| `-P`   | `--no-dereference` | never follow symlinks (default with `-R`) |
| `-a`   | `--archive` | archive mode, same as `-RpP` |
| `-f`   | `--force` | remove existing destinations, no prompting |
| `-i`   | `--interactive` | prompt before overwrite (`y`/`Y` confirms) |
| `-n`   | `--no-clobber` | never overwrite existing files |
| `-l`   | `--link` | hard-link regular files instead of copying |
| `-s`   | `--symbolic-link` | symlink files instead of copying |
| `-p`   | | preserve mode, ownership, timestamps, flags, ACLs |
| `-N`   | | with `-p`: do not copy file flags |
| `-v`   | `--verbose` | print `src -> dst` for each copied file |
| `-x`   | `--one-file-system` | do not traverse mount points |
| `-r`   | | historic; same as `-RL` (may not be combined with `-R`) |
|        | `--sort` | sort directory entries while traversing |
|        | `--copy-new` | with `-R`: re-scan after copying, copy what appeared/changed |
|        | `--remove-gone` | with `-R`: re-scan after copying, delete what we copied that is gone |
|        | `--reconcile` | both `--copy-new` and `--remove-gone` |
|        | `--passes=N` | at most N reconcile passes (0 = until nothing changes, default) |
|        | `--settle=SEC` | with `--copy-new`: wait for files modified less than SEC ago |

The vcp-only long options above are matched by their exact name only, so
cp's abbreviations (`--rec` for `--recursive`, ...) keep working unchanged.
Without them vcp behaves exactly like cp with progress bars.

Semantics mirror FreeBSD cp: BSD getopt behavior (option parsing stops at the
first non-option argument), `-f`/`-i`/`-n` and `-H`/`-L`/`-P` override one
another, `cp -R src/. dst` copies contents, identical-file detection,
`is a directory (not copied)`, sockets are skipped with a warning, fifos and
device nodes are re-created with `-R`, directory attributes are corrected
post-order, exit status 1 on any failure, 64 on usage errors, messages match
cp's wording. Like cp, recursive copies do all destination-side work relative
to a file descriptor of the target directory with `RESOLVE_BENEATH`
resolution, so a symlink already present in the destination cannot redirect
writes outside of it (refused with `Permission denied`).

Copying uses `copy_file_range(2)` (Linux, FreeBSD) with a read/write
fallback (always used on macOS). Request sizes
adapt to the observed throughput (64K–4M: doubling while requests complete
quickly, halving when they drag), so the bars and the speed readout keep
updating a few times per second even on slow links, while fast local copies
quickly settle on large requests.

## Reconcile mode

Like cp, vcp works from a snapshot of the source taken before the copy
starts: a file deleted before its turn is an error (`No such file or
directory`, exit 1), a file deleted while it is being copied is copied
anyway (the open descriptor keeps it alive), a file rewritten in place
during the copy is copied as it was at that moment, and files created after
the start are never seen. This is what `cp -R` does too. For a source that
is being modified while a long copy runs (e.g. videos being transcoded into
new files and the originals removed), the reconcile options make vcp
re-scan the source after the copy and act on the differences:

- `--copy-new` — entries that are new or changed since the previous pass
  are copied (files, symlinks, fifos, devices, directories). "Changed" means
  a different type, inode, size or modification time (with `-p` also mode,
  owner, group or flags). A directory is only compared by type. A file that
  vanished before its turn is silently skipped in the copy phase and the
  re-scan decides whether it is gone or has a replacement.
- `--remove-gone` — entries that vcp itself created (or overwrote) in this
  run and that are no longer in the source are deleted from the
  destination, children before parents. Destination entries vcp did not
  create or overwrite in this run are never touched (so pre-existing extra
  files and directories stay), directories are only removed when they are
  empty, the command-line operands themselves are never removed
  (a vanished operand stays an error), and every removal first verifies
  that the destination entry is still the very inode vcp created. Removals
  are done before the copies of the same pass to free space first. With
  `-v` each removal prints `deleting <dst>`.
- `--reconcile` — both of the above.
- `--passes=N` — maximum number of reconcile passes. `0` (the default)
  means: repeat until a re-scan finds no differences. With `N > 0`, if the
  re-scan after pass N still finds differences, vcp prints
  `source still changing after N reconcile pass(es); destination may be out
  of sync` and exits 1. Note that a file being continuously appended to
  keeps `--passes=0` running until it stops changing — by design.
- `--settle=SEC` — a file whose modification time is less than SEC seconds
  old is considered still being written and is held back (also during the
  first pass); when only such files remain, vcp waits for them
  (`waiting for N unsettled file(s)`) and re-scans. Requires `--copy-new`
  or `--reconcile`.

Every pass is a copy of its own: the bars, `speed:` and `eta:` lines start
fresh, since the size of a pass is known only after its re-scan (there is
no way to know how many more passes will follow). Each pass with work is
announced on stderr (when it is a terminal or with `-v`) as
`vcp: reconcile pass K: X new, Y changed, Z gone[, U unsettled]`. Re-copies
follow the normal cp rules (`-i` prompts again, `-n` skips and exits 1,
`-p` re-applies directory times). The options require `-R`; without them
nothing of this is active and vcp behaves exactly like before.

Not detected: renames (a renamed file is "gone" plus "new", so it is
copied again), and content changes that keep both size and modification
time.

## Portability

Builds and runs on FreeBSD, Linux and macOS (selected at compile time):

| Feature | FreeBSD | macOS | Linux |
|---------|---------|-------|-------|
| `RESOLVE_BENEATH` destination containment | yes | no (no kernel support) | no |
| `-p` file flags (`chflags`) | yes | yes (not on symlinks) | n/a |
| `-p` NFSv4/POSIX.1e ACLs | yes | no | no |
| `copy_file_range(2)` fast path | yes | no (read/write) | yes |
| `SIGINFO` (Ctrl+T) status line | yes | yes | n/a |

## Build

```
cargo build --release
# binary in target/release/vcp
```

## Tests

- `cargo test` — unit tests for the bar renderer and copy loop: fill
  excludes the `(...)` text, arrow-head monotonicity, constant line
  width, pinned `(`/`/`/`%`/`)` positions, humanized field padding,
  `COLUMNS` override, sliding-ellipsis behavior, speed formatting and
  layout, current-window vs. average speed math, ETA formatting
  (`1w 3d 1h 32m 16s`, `done`/`unknown`/`>999w`), ETA line layout and
  ETA tracking of remaining bytes, adaptive chunk sizing; reconcile
  diffing: new/changed/gone/type-changed detection, deepest-first removal
  order, `-p` attribute comparison, `--settle` hold-back and wait time,
  ancestor directories kept for new entries, ownership bookkeeping.
- `sh tests/vcp-suite.sh` (FreeBSD, after `cargo build --release`) —
  187-check compatibility suite that runs `vcp` and `/bin/cp` side by
  side and compares messages, exit codes and resulting trees (mtree),
  including `RESOLVE_BENEATH` escape refusal, plus pty checks
  (`script(1)`) of the rendered bars: forced `COLUMNS` width, constant
  line length, `/` and `%` never moving mid-copy, text region never
  overwritten by the fill, speed and ETA lines present under the bars in
  every frame with pinned fields, a non-zero final average and a final
  `done` ETA. The reconcile section (57 checks) modifies the source while
  vcp is blocked at an `-i` prompt or in a `--settle` wait and checks:
  option validation and that cp abbreviations are unaffected, unchanged
  behavior without the options, `--copy-new`/`--remove-gone` alone and
  combined, changed files re-copied, directory replaced by a file (both
  before and after its contents were copied), `--passes=1` warning and
  exit status, `--settle` hold-back with `-p` directory times, safety
  (pre-existing destination directories kept, operands never removed),
  and per-pass bar totals on a pty.

## Why

`cp` gives no feedback on long copies; `SIGINFO` helps but is manual. `vcp`
is `cp` with mc-style progress — nothing more, nothing less.
