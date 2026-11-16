# fclones bug review

Reviewed revision: `a74f90d` (version 0.35.0), all of `fclones/src/`.

**How this was checked.** No Rust toolchain is installed on this machine, so I couldn't build or run the test suite.
I read all the source and then reproduced as many findings as possible with the installed
`/usr/bin/fclones`, which reports version **0.35.0**, the same as HEAD. Each finding is marked:

- **[Reproduced]**: I observed the wrong behavior with the 0.35.0 binary. The command is included.
- **[Code]**: found by reading the code only; it was not reproduced.

Line numbers refer to files under `fclones/src/`.

---

## Summary

| # | Severity | Area | Finding | Status |
|---|----------|------|---------|--------|
| C1 | Critical | transform | `group --transform … --in-place --no-copy` **deletes every scanned file** | Reproduced |
| C2 | Critical | dedupe | `group -S -H` + `remove` deletes the real file and leaves a dangling symlink | Reproduced |
| C3 | Critical | report | Report reader trims paths, so a file named `"a "` becomes `a` and `remove` deletes **a different file** | Reproduced |
| C4 | Critical | transform | Transformed output is hashed only up to the *input* length, so different files are reported as duplicates | Reproduced |
| C5 | High | dedupe | File locking (`--no-lock` off) does nothing: the lock is released right after it is taken | Reproduced (strace) |
| H1 | High | CLI | `-n/--rf-over` on `remove/link/move/dedupe` always panics | Reproduced |
| H2 | High | transform | Transform whose output is larger than the input + pipe buffer **deadlocks** | Reproduced |
| H3 | High | group | `--max-suffix-size` larger than a file causes an underflow, and duplicates are silently missed | Reproduced |
| H4 | High | dedupe | `--priority top/bottom` is inverted or ignored when combined with other priorities | Reproduced |
| H5 | High | report | The header command parser mixes char and byte indices; non-ASCII args corrupt `--isolate` roots | Reproduced |
| H6 | High | group | With `--transform`, unique files are reported as groups of 1 | Reproduced |
| H7 | High | dedupe | Read-only duplicates can't be removed or linked with default settings (the lock needs write access) | Reproduced |
| H8 | High | pattern | `--regex` alternation isn't anchored, so `a\|b` means "starts with a OR ends with b" | Reproduced |
| H9 | High | selector | `--exclude '**/foo'` also excludes `foobar/` (prefix match on directories) | Reproduced |
| M1 | Medium | walk | `--depth 1` descends one level deeper than documented | Reproduced |
| M2 | Medium | walk | An explicitly named hidden input path (or a hidden cwd with `.`) is silently skipped | Reproduced |
| M3 | Medium | walk | `.fdignore` is ignored when a `.gitignore` exists; `!negation` across nested ignore files doesn't work | Reproduced / Code |
| M4 | Medium | pattern | The documented `!(a\|b)` extglob doesn't compile | Reproduced |
| M5 | Medium | transform | The "is the program runnable" check drops the program's directory, so absolute paths are rejected; it also runs the program | Reproduced |
| M6 | Medium | transform | The transform command is silently truncated at a `$` that isn't `$IN`/`$OUT` | Reproduced |
| M7 | Medium | stats | Hard links are counted as redundant/reclaimable space (overstated 3×) | Reproduced |
| M8 | Medium | dry-run | The `move --dry-run` script has no `mkdir -p`; the Windows `mklink` arguments are reversed | Reproduced / Code |
| M9 | Medium | regex | `get_fixed_prefix` never resets its escape state and mixes bytes with chars, so directories can be wrongly pruned | Code |
| M10 | Medium | device | `get_mount_point` panics when no disks are detected; its fallback is the wrong mount | Code |
| M11 | Medium | cache | Cache validity is only `(inode, mtime ms, len)`, so content changed with a preserved mtime returns a stale hash | Code |
| M12 | Medium | reflink | `dedupe` after `--transform` overwrites files that are only equal *after* transform; FICLONE with unequal sizes | Code |
| M13 | Medium | report | Malformed text report (odd-length hash, huge numbers) panics instead of returning an error | Code |
| L1–L15 | Low | various | See the Low section | Code |

---

## Critical

### C1. `--in-place --no-copy` transform deletes all original files [Reproduced]

`transform.rs:255-257` sets `Output::InPlace(input_conf.input_path().clone())`. With `--no-copy`, `copy == false`, so
`input_path()` is the **original file** (`Input::Named`). When each `Execution` ends, `Output`'s `Drop`
(`transform.rs:93-101`) runs `std::fs::remove_file(target)` on it.

`fclones group` is documented as "safe and does not modify the filesystem". Here it deletes every file it hashes,
even with a no-op transform:

```sh
mkdir d; echo one > d/a; echo two > d/b; echo one > d/c
fclones group --transform 'touch $IN' --in-place --no-copy d
ls d        # empty: all three files are gone
```

The `--no-copy` help text warns that the transform command may modify the original. It does not say that fclones
itself will delete it.
**Fix:** `Output::InPlace` should only delete the file when it's the temporary copy (`Input::Copied`), never the original.

### C2. `-S -H` reports cause `remove` to delete the target of a symlink [Reproduced]

With `--symbolic-links --match-links`, both a symlink `a -> b` and its target `b` are reported in one group.
In `dedupe.rs:814-824`, `partition` tries to skip symlinks ("We don't want to remove dirs or symlinks"), but the
metadata comes from `FileMetadata::new` → `fs::metadata` (`file.rs:302`), which **follows** symlinks. So
`is_file()` is true for the link. Because `match_links` disables grouping by inode, `a` and `b` become separate
subgroups and one of them is chosen for removal. When the link sorts first, the real file is removed:

```sh
mkdir l; echo "real data" > l/b; ln -s b l/a
fclones group -S -H l > rep.txt
fclones remove < rep.txt
cat l/a     # No such file or directory: the data is gone, a dangling symlink remains
```

**Fix:** use `symlink_metadata` for the `is_file` check (or record the link-ness), and never drop a file whose
`FileId` equals a retained file's `FileId`.

### C3. Text report reader trims whitespace from paths, so the wrong file gets deleted [Reproduced]

The writer emits paths as `"    " + to_escaped_string()` (`report.rs:167`). STFU-8 does not escape spaces.
The reader does `Path::from_escaped_string(path_str.trim())` (`report.rs:411-417`), which strips trailing spaces
from the file name. If a different file with the stripped name and the same size exists, the size check in
`partition` passes and that file is deleted:

```sh
mkdir d; echo dupdata > "d/a "; echo dupdata > d/b; echo 'OTHER!!' > d/a   # all 8 bytes
fclones group d > rep.txt
fclones remove --priority top < rep.txt
# removed d/a ("OTHER!!", which was not a duplicate); "d/a " and d/b both remain
```

Related: the `# Base dir:` header is written with `to_escaped_string()` (`report.rs:135`) but read back with plain
`Path::from(...)` and no unescaping (`report.rs:561-562`). A base dir containing `\` or non-UTF-8 bytes is
corrupted. That base dir is used to rebuild `--isolate` roots.
**Fix:** only strip the 4-space indent and the line terminator (`\n`/`\r\n`), never `trim()`. Decode the base dir with `from_escaped_string`.

### C4. Transform output is truncated to the input file's length, causing false duplicates [Reproduced]

`hasher.rs:332-344` calls `stream_hash(stream, chunk.len, …)`. `chunk.len` is the **original** file size, so
only that many bytes of the transformed output are hashed. Any transform that expands data (decompression,
format conversion, `exiftool` dumps, …) hashes only a prefix of the output. Files whose transformed output
differs after that point are grouped as identical:

```sh
python3 -c "import gzip; c=b'A'*2000
open('g/x.gz','wb').write(gzip.compress(c+b'tail-one'*10))
open('g/y.gz','wb').write(gzip.compress(c+b'tail-two'*10))"
fclones group --transform 'gzip -dc' g     # reports x.gz and y.gz as duplicates
```

`run_dedupe` sets `no_check_size` automatically after `--transform` (`main.rs:166`), so `fclones remove` deletes one of them.
**Fix:** hash the whole output stream (`FileLen::MAX`), not `chunk.len`.

### C5. `--lock` does nothing [Reproduced via strace]

`dedupe.rs:315, 320, 325, 330, 339` all use `let _ = Self::maybe_lock(...)?;`. In Rust, `let _ = <temporary>`
drops the value immediately, so the `FileLock` is unlocked and closed **before** the remove/link runs:

```
openat("d/b", O_WRONLY) = 3
fcntl(3, F_SETLK, F_WRLCK) = 0
fcntl(3, F_SETLK, F_UNLCK) = 0      <- released right away
unlink("d/b") = 0
```

The `Move` arm (`dedupe.rs:339`) also lacks `?`, so lock errors are silently ignored there.
**Fix:** `let _lock = Self::maybe_lock(...)?;`. Clippy's `let_underscore_lock`/`let_underscore_must_use` lints would catch this.

---

## High

### H1. `-n/--rf-over` on `remove`/`link`/`move`/`dedupe` always panics [Reproduced]

`config.rs:610-614`: the field is `Option<usize>` but the parser is `value_parser!(u64).range(1..)`. clap
can't downcast the value:

```sh
fclones remove --dry-run -n 2 < rep.txt
# thread 'main' panicked at clap_builder-4.5.31/src/parser/error.rs:32:9:
# Mismatch between definition and access of `rf_over`.
```

**Fix:** `value_parser = clap::value_parser!(usize).range(1..)` (or make the field `u64`). Add a CLI test that passes `-n`.

### H2. Transform deadlocks when its output exceeds the input length plus the pipe buffer [Reproduced]

This has the same root cause as C4. After reading `chunk.len` bytes, `hash_transformed` calls
`child.wait()` (`hasher.rs:349`) **before** the rest of stdout is drained (the drain only happens later in
`Execution::drop`, `transform.rs:279-285`). A child with more than about 64 KiB of output left blocks on `write()`
while fclones blocks on `wait()`:

```sh
# 246-byte .gz files that decompress to ~200 KB
timeout 20 fclones group --transform 'gzip -dc' g    # killed by timeout, never finishes
```

**Fix:** read the stream to EOF (see C4) before calling `wait()`.

### H3. `--max-suffix-size` larger than a file underflows; duplicates are silently lost [Reproduced]

`group.rs:1103` computes `fi.len.as_pos() - suffix_len`. The pre-filter only requires
`file_len >= suffix_threshold` (64 KiB on SSDs, `device.rs:115-121`), not `file_len >= suffix_len`. A user-provided
`--max-suffix-size` can be larger than that. `FilePos - FileLen` is unchecked (`file.rs:82-87`): in debug builds
it panics, and in release builds it wraps to a huge offset and `seek` fails with EINVAL.

```sh
head -c 300000 /dev/urandom > s/x; cp s/x s/y
fclones group --max-suffix-size 1MiB s
# warn: Failed to compute hash of file …/s/y: Invalid argument (os error 22)
# Found 0 (0 B) redundant files
```

**Fix:** `let suffix_len = min(suffix_len, fi.len)` and start at `fi.len.as_pos() - suffix_len`; use `checked_sub`/`saturating_sub` in the `Sub` impls.

### H4. `--priority top`/`bottom` break when combined with other priorities [Reproduced]

Priorities are applied in reverse with the expectation that each one is a *stable sort* (`dedupe.rs:855-861`).
`Top` is `files.reverse()` and `Bottom` is a no-op (`dedupe.rs:684-688`). Neither sorts by input position:

- `--priority top --priority most-recently-modified` sorts by mtime, then **reverses the whole list**. The result
  is "remove the bottom file", the opposite of `top`.
- `--priority bottom --priority <x>`: `bottom` is ignored and `<x>` alone decides.

```sh
# a (newest), b, c (oldest) listed in this order
fclones remove --dry-run --priority top                               < rep  # rm b, rm a   (correct)
fclones remove --dry-run --priority top --priority most-recently-modified < rep  # rm b, rm c   (wrong)
fclones remove --dry-run --priority bottom                            < rep  # rm b, rm c   (correct)
fclones remove --dry-run --priority bottom --priority most-recently-modified < rep # rm b, rm a (wrong)
```

**Fix:** keep the original index in each subgroup and implement `Top`/`Bottom` as stable sorts by that index.

### H5. Report header command parser corrupts arguments after non-ASCII text [Reproduced]

In `arg::split`, `pos` counts **chars** (`arg.rs:371`) but is used as a **byte** index to slice `$'…'` quoted words
(`arg.rs:343, 352`). Any multi-byte character before a `$'…'` word shifts the slice. That gives a silently wrong
argument, a decode error, or a panic when the index lands inside a UTF-8 sequence. `quote()` emits `$'…'` for any path
containing `'` or control characters, so this happens easily, for example a folder named `Peter's` after one
named `Ü`.

The parsed command is used to rebuild `--isolate` roots for `remove`/`link`/…. Renaming an empty, unrelated root
from `U` to `Ü` changes which files get deleted:

```sh
# roots: U|Ü (empty), "Peter's" (a, b), Zed (a)
fclones group --isolate U   "Peter's" Zed | fclones remove --dry-run   # rm Zed/a                  (correct)
fclones group --isolate Ü   "Peter's" Zed | fclones remove --dry-run   # rm Peter's/a, Peter's/b  (wrong roots)
```

**Fix:** iterate with `char_indices()` and use byte offsets.

### H6. With `--transform`, unique files are reported as duplicate groups of size 1 [Reproduced]

`GroupConfig::rf_over()` returns `0` whenever a transform is set (`config.rs:458-469`). That's intended to stop
early pruning, but `group_filter()` (`config.rs:440-456`) uses the same value for the **final** report filter
(`matches_strictly`: `count > 0`). So every file is reported, and the header counts them as "redundant":

```sh
echo one > u/a; echo two > u/b
fclones group --transform cat u     # two groups "* 1", one per file
```

**Fix:** use the user's `rf_over` (default 1) for the final strict filter and `0` only for intermediate pruning.

### H7. Read-only files can't be deduplicated with default settings [Reproduced]

`FileLock::new` opens the file `O_WRONLY` (`lock.rs:269-279`). Removing, linking, or moving a file needs write
permission on the *directory*, not on the file. With the default (locking on), every read-only duplicate fails:

```sh
chmod 444 d/a d/b
fclones remove < rep.txt          # warn: Failed to open file d/b for write: Permission denied
fclones remove --no-lock < rep.txt  # works
```

This interacts with C5: the lock also gives no protection. **Fix:** open read-only and use `F_RDLCK`/`flock`, or
treat `PermissionDenied` like `Unsupported` (skip the lock).

### H8. `--regex` patterns with top-level `|` aren't anchored [Reproduced]

`Pattern::regex_with` builds `"^" + pattern + "$"` (`pattern.rs:86`) without a group, so `a|b` becomes `^a|b$`.
That means "starts with a OR ends with b", not "matches a or b fully":

```sh
fclones group -x --name '.*\.png|.*\.txt' d    # also matches x.png.bak and y.png.old
```

With `--exclude`, this excludes much more than intended. **Fix:** `"^(?:" + pattern + ")$"` (and the same for `prefix_regex`).
Also, `trim_end_matches('$')` (`pattern.rs:83`) strips an *escaped* `\$`, which turns `foo\$` into an
unanchored `^foo\$` regex.

### H9. `--exclude` prefix-matches directories, so `**/foo` also excludes `foobar/` [Reproduced]

`PathSelector::matches_dir` rejects a directory if `p.matches_prefix(&path)` for **any** exclude pattern
(`selector.rs:84`). `prefix_regex` is the unanchored-at-end regex `^.*/foo`, which matches `/x/foobar/`. The doc comment
(`selector.rs:72`) says this should only apply to exclude patterns ending in `**`.

```sh
mkdir foo foobar; echo s > foo/1; echo s > foobar/1; echo s > foobar/2
fclones group --exclude '**/foo' .     # reports nothing: foobar/ was skipped entirely
```

**Fix:** for directories, only use prefix matching for exclude patterns that end in `/**`. Otherwise use a full match on the directory path.

---

## Medium

### M1. `--depth` is off by one [Reproduced]
The docs say "1 descends into directories specified explicitly as input paths, but does not descend into
subdirectories". `visit_dir` checks `level > self.depth` (`walk.rs:357`). The root is level 0 and its child directories are
level 1, so with `--depth 1` the children are still read. `fclones group --depth 1 x` reports `x/sub/b`.
Should be `level >= self.depth` for subdirectories, or the root handled at level 1.

### M2. Explicitly given hidden paths are silently ignored [Reproduced]
The hidden check (`walk.rs:279-285`) also applies to the root entries. `fclones group .hid` or
`fclones group ~/.config` finds 0 files with no warning. Running `fclones group .` while the cwd is
hidden does the same, because the root is canonicalized to its real name. Roots should be exempt from the hidden filter.

### M3. Ignore-file handling [Reproduced / Code]
- `IgnoreStack::push` (`walk.rs:87-95`) loads `.fdignore` **only if no `.gitignore` exists**, so with both present
  the `.fdignore` rules are ignored (reproduced: a file listed in `.fdignore` was still reported).
- `matches` (`walk.rs:117-120`) checks each ignore file independently with `any(is_ignore)`, so a `!pattern`
  whitelist in a nested `.gitignore` can't re-include something a parent ignored. [Code]

### M4. Documented `!(…)` extglob doesn't compile [Reproduced]
`pattern.rs:217-219` translates `!(a|b)` to the look-ahead `(?!a|b)`. The `regex` crate doesn't support that:
```
fclones group --name '*.!(jpg)' d
error: Invalid pattern: … regex parse error: ^[^/]*\.(?!jpg)$ … look-around … not supported
```
Even with look-around support, a bare zero-width `(?!…)` wouldn't consume characters. The feature needs a different
translation or should be removed from the docs (`pattern.rs:129`).

### M5. Transform pre-flight check is wrong and has side effects [Reproduced]
`Transform::new` (`transform.rs:157-181`) keeps only the **file name** of the program and runs
`Command::new(&program).spawn()`:
- An absolute path to a program that isn't on `PATH` is rejected even though the real run would work:
  `--transform /opt/x/mycat` → `Cannot launch mycat: No such file or directory`.
- It really executes the program with no arguments and inherited stdin/stdout/stderr. The program can print into the
  report on stdout or consume stdin (`--stdin` input lists). `kill()` is called but the child is never `wait()`ed.
- `p.into_string().unwrap()` panics on a non-UTF-8 program name.

### M6. Transform command parsing silently truncates [Reproduced]
`parse_command` (`transform.rs:431-440`) ignores unparsed remaining input (`.1` of the `IResult`). A `$` not
followed by `[A-Za-z0-9_]` stops parsing:
`--transform 'sed -e s/one$/two/'` runs `sed -e s/one` ("unterminated `s' command"). Also, `$FOO` for any
name other than `IN`/`OUT` is replaced by the literal `FOO` (the `$` is dropped). Quoting isn't supported, so arguments
can't contain spaces. The parser should fail loudly on leftover input, and should probably reuse `arg::split`.

### M7. Hard links inflate "redundant" and "reclaimed" sizes [Reproduced]
`redundant_count`'s "fast path" (`group.rs:404-406`) claims to be equivalent to the subgroup path but ignores
`group_by_id`. `FsCommand::space_to_reclaim`/`execute` return `metadata.len()` even when `nlink > 1`.
With three hard links plus one copy (1000 B each), the header says `Redundant: 3000 B in 3 files` and
`remove --dry-run` says "reclaim 3.0 KB", but only 1000 B would be freed.

### M8. Dry-run scripts aren't executable as printed [Reproduced / Code]
- `move --dry-run` emits `mv src target/full/path/src` (`dedupe.rs:412-425`) without `mkdir -p` for the target's
  parent, while the real execution does create it (`dedupe.rs:240,249`). Pasting the script fails. [Reproduced]
- Windows: `mklink {target} {link}` (`dedupe.rs:443,451`). The syntax is `mklink [/H] <link> <target>`, so the order is
  reversed. Running it would try to create a link at the *kept* file's path. [Code]

### M9. `Regex::get_fixed_prefix` / `is_partial_match` can prune directories wrongly [Code]
`regex.rs:54-88`:
- `escape` is set to `true` after the first `\` and **never reset**, so every later magic character (`[`, `*`,
  `(`, …) is copied into the "fixed prefix" verbatim. Example: base dir `/srv/1.0`, `--path '*/x/*'` gives the regex
  `^/srv/1\.0/[^/]*/x/…`. The prefix becomes `/srv/1.0/[^/]*/`, `matches_dir("/srv/1.0/sub/")` returns false, and the subtree is
  skipped. (It didn't reproduce in the scratch directory only because the scratch path contains `-`, which
  ends the prefix early.)
- `result.chars().take(result.len() - 1)` mixes the byte length with a char count. With multi-byte characters it doesn't
  drop the last char, and if `result` is empty it underflows `usize`.
- `is_partial_match` (`regex.rs:42-43`) truncates `s` to `min(s.len(), prefix.len())` **chars** using **byte**
  lengths. With non-ASCII paths, a directory that matches the prefix exactly can be rejected.

### M10. `DiskDevices::get_mount_point` can panic [Code]
`device.rs:281`: `.unwrap_or(&self.mount_points[0].0)` panics if `sysinfo` reports no disks (for example in some
containers or sandboxes). `fclones move` calls this for every file (`dedupe.rs:733`). Also, after the sort by
descending depth (`device.rs:268-270`), `mount_points[0]` is the *deepest* mount, not a sensible default.

### M11. Hash cache can return stale hashes [Code]
`HashCache::get` (`cache.rs:141-152`) treats an entry as valid when `(device, inode, chunk)` matches and
`mtime (ms)` + `len` are unchanged. Content changes that preserve the mtime (`touch -r`, `rsync --inplace -t`,
tools that keep timestamps, editors with "preserve mtime") or inode reuse after delete+recreate with a preserved mtime
return the old hash. Wrong hashes lead to false duplicates and then deletions. Including `ctime` (which users can't set)
in the validity check would close most of this.

### M12. `dedupe` (reflink) after `--transform` silently changes file contents [Code]
After a `--transform` grouping, files are only "equal after transform", and `no_check_size` is forced
(`main.rs:166`). `fclones dedupe` then FICLONEs the kept file over the others (`reflink.rs:109`).
This **replaces** their real contents, for example images grouped by pixel data lose their own EXIF. The
"same size" assumption in `reflink_overwrite` (`reflink.rs:136`, `truncate(false)`) also no longer holds. If the
destination is larger, the result depends on filesystem FICLONE semantics. `dedupe`/`link` should refuse
transformed reports, or at least warn.

### M13. Malformed text reports panic instead of erroring [Code]
`report.rs:393-395`: `FileHash::from_str(..).unwrap()` panics on an odd-length hex hash (the regex accepts
`[a-f0-9]+`). The `parse::<u64>().unwrap()`/`parse::<usize>().unwrap()` calls panic on numbers that overflow.
A hand-edited report should give an `InvalidData` error.

---

## Low

- **L1** `DiskDevices::is_empty` asserts `self.devices.is_empty()` and returns `false` (`device.rs:308-312`). The
  assertion is inverted, so any call panics. It's unused today, but `DiskDevices` is part of the public API (`lib.rs`).
- **L2** The `removable` thread-pool name is accepted by `check_pool_config` (`device.rs:321`) but never looked up in
  `get_parallelism` (`device.rs:165-169`), so `--threads removable:N` is silently ignored.
- **L3** `physical_device_name` regex `^/dev/([fhs]d[a-z]|nvme[0-9]+)` (`device.rs:210`) maps `/dev/sdaa1` to `/dev/sda`
  and doesn't handle `vd*`, `xvd*`, `mmcblk*`. Partitions of one HDD then get separate thread pools and
  parallel seeks.
- **L4** `arg::quote` special chars (`arg.rs:129-132`) contain `'˜'` (U+02DC) instead of `'~'`, and miss `!`
  (history expansion when pasting the dry-run script into interactive bash).
- **L5** `util::try_sort_by_key` (`util.rs:18-28`): the doc says items whose key fails are sorted *last*, but
  `Option`'s order puts `None` *first*. The key function runs O(n log n) times, so the same error is pushed many times.
- **L6** `Path::strip_prefix` (`path.rs:150-161`) returns `Some(".")` when `base` is *longer* than `self`
  (e.g. `/a`.strip_prefix(`/a/b`)). It should return `None`.
- **L7** Panics on odd paths: Unix `os_to_c_str` unwraps `CString::new` (`path.rs:393`), so a report path that decodes to
  a NUL byte panics. Windows `c_to_os_str`/`os_to_c_str` unwrap `to_str()` (`path.rs:396-404`) and panic on
  file names with unpaired surrogates, which NTFS allows.
- **L8** With `-H`, groups made only of hard links skip hashing (`pre_filter` is `unique_count() > 1`,
  `group.rs:1025, 1126`) and are reported with hash `00000000000000000000000000000000`.
- **L9** `--isolate` together with `--stdin` is always rejected: `validate` counts `self.paths.len()`, which is 0
  with `--stdin` (`config.rs:378`).
- **L10** `Walk::sorted_entries` silently drops `read_dir` entry errors (`walk.rs:406, 411`). Unreadable entries
  produce no warning, unlike every other error in the walker.
- **L11** `StdLog::new` unwraps `current_exe()` (`log.rs:75-80`) and panics if `/proc` isn't available.
- **L12** `rlimit_nofile` (`rlimit.rs:45-48`): when the limit is `RLIM_INFINITY`, `as isize` gives -1, so the
  open-file semaphore falls back to 64. One permit is taken per transform task, but a transform child uses about 3 fds.
- **L13** `--max-prefix-size` below 4 KiB isn't honored: files larger than the prefix length always use
  `min_prefix_len()` = 4 KiB (`group.rs:1040-1044`). The option is effectively "max(4 KiB, value)".
- **L14** `Transform` derives `Clone` but its `Drop` deletes `tmp_dir` (`transform.rs:107, 264-268`). Dropping
  any clone would delete the directory under the original. It isn't cloned today, so this is latent.
- **L15** Cosmetic: phase name "Fetching extends" (`phase.rs`); `update_file_locations` uses a hard-coded label
  instead of `phases.format(...)` (`group.rs:882`), so phase numbering skips it; `"Count not create hash database"`
  (`cache.rs:63`); `Pattern::literal`/`Pattern::glob` docs say "case insensitive" but they're case-sensitive by default.

---

## Suggested regression tests

Most of the Critical and High items can be covered with small integration tests in the style of the existing
`with_dir` tests:

1. `--in-place --no-copy` transform keeps all input files (C1).
2. `-S -H` group + `remove` never removes a file that another reported path links to (C2).
3. Report round-trip with file names that have leading or trailing spaces and tabs (C3).
4. Transform that expands data (`gzip -dc`) with a shared prefix → no group; large expansion → no hang (C4, H2).
5. `FsCommand::execute` holds the lock during the operation; read-only files can be removed (C5, H7).
6. `Config::try_parse_from(["fclones","remove","-n","2"])` parses (H1).
7. `--max-suffix-size` larger than the file size (H3).
8. Every pair of `--priority` values, compared against an explicit expected order (H4).
9. `arg::split(arg::join(args)) == args` property test with non-ASCII and quote characters (H5).
10. Regex/glob anchoring tests with `|`, `\$`, and `**/foo` vs `foobar` (H8, H9).
