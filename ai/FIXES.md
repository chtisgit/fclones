# Fixes and changes on branch `consolidate-upstream`

59 commits on top of upstream `a74f90d`. Finding IDs (C/H/M/L) refer to `CLAUDE_REVIEW.md`. The
full record of every decision, check and test is in `GOAL.LOG`.

Totals:
- 41 of the 42 review findings fixed. Only L8 is left, deliberately; see `REMAINING.md`.
- 12 upstream PRs applied, plus 5 follow-up commits.
- 4 further GitHub issue fixes, closing 6 issues.
- 57 new tests: 46 written here, 11 that came with the PRs.

## Applied upstream pull requests

Author kept in each commit, and the PR is named in the commit message.

| PR | Change | Commit | Issues | Notes |
|---|---|---|---|---|
| #365 | Fix crash on non-UTF-8 file names on Windows | eed7415, f686978 | #364, #298 (crash) | Windows part of L7; f686978 fixes the Windows test build |
| #299 | Reflink dedupe via FIDEDUPERANGE instead of FICLONE | 64b4d23, 936f6bb, 8ababe6 | #293, #113 (partly) | M12; 936f6bb fixes the PR's wrong ioctl request code (the call always failed with ENOTTY) and its error handling; 8ababe6 fixes a race between tests |
| #359 | Canonicalize input paths so `--isolate` works with `.` / `../x` | cfb5d1b | #351 | |
| #349 | Scanned file count and size in the report | baf6228, cb976a0 | #347 | cb976a0 adapts #359's test to the new API |
| #334 | `--ignore-xattr-errors` for dedupe (macOS) | 5b910ae, 113aef9 | #333 | 113aef9 adds a Linux test |
| #348 | `io::Error::other` instead of `io::Error::new(Other, ..)` | c3798ee | | |
| #357 | README: fix multi-pattern `--name` / `--exclude` examples | 89d0c00 | #310, #332 | |
| #352 | README: fix repeated `--threads` example | c5348c4 | | |
| #350 | README: refresh the report example | 8225010 | | |
| #291, #318, #331 | README: Alpine, Arch and czkawka links | bbb33e6, bcf60ea, 5f5ea7f | | |

## Review findings (`CLAUDE_REVIEW.md`)

| ID | Fix | Commit | Issues |
|---|---|---|---|
| C1 | `--transform --in-place --no-copy` no longer deletes the original files | 830680d | |
| C2 | Never remove or replace the target of a kept symlink (`-S -H` reports) | 1325d3f | |
| C3 | Text report reader keeps whitespace in paths; the base dir is unescaped | 0a11031 | #275 |
| C4, H2 | Transform output is hashed to EOF: no truncation, no deadlock | 05b70d1 | |
| C5, H7 | `--lock` holds the lock while acting; read-only files get a shared lock | 006a6eb | #113 (partly) |
| H1 | `-n/--rf-over` on remove/link/move/dedupe no longer panics | 1d0fdcf | #302 |
| H3 | Suffix length is clamped to the file length | 56430b6 | #303 |
| H4 | `--priority top/bottom` combine with other priorities | 1e65bd7 | |
| H5 | Report header parser uses byte offsets (non-ASCII text) | 7d2f84f | |
| H6 | With `--transform`, the final report applies the replication filter | d5e7081 | #320, likely #345 |
| H8 | `--regex` with top-level `\|` is anchored correctly | 5f608d9 | |
| H9 | `--exclude` matches whole directory paths only | b8dcc0a | |
| M1 | `--depth` off by one | fbab8bc | |
| M2 | Explicitly given hidden paths are scanned | 46bbdcc | |
| M3 | Both `.gitignore` and `.fdignore` honored; nested ignore files win | b653959 | |
| M4 | Unsupported `!(…)` globs give a clear error | 04f84b9 | |
| M5, M6 | Transform program lookup and command parsing | 2ab4e36 | #288 |
| M7 | Hard-linked data counted once in redundant / reclaimed sizes | a596772 | #268, #257 (stats part) |
| M8 | Dry-run scripts: create move target dirs; fix `mklink` argument order | 2f0090b | #367 |
| M9 | Fixed-prefix extraction for `--path` directory pruning | 03c3742 | |
| M10 | `get_mount_point` no longer panics without mount points | 003547f | |
| M11 | Hash cache detects changes that keep the mtime (ctime check; cache moved to `hash_db_v2`) | 46f62e8 | #84 |
| M12 | Refuse to reflink files of different sizes (the rest is in PR #299) | aac8ef6 | #293 |
| M13 | Malformed group headers give an error instead of a panic | 3a6acc7 | |
| L1, L11, L15 | Inverted assertion, `current_exe` unwrap, wording/typos | 793b9a2 | |
| L2, L3 | `--threads removable:N` works; more disk device names recognized | bd76f19 | |
| L4 | Shell quoting covers `~` and `!` | 7d2668a | #366 |
| L5 | `try_sort_by_key` computes keys once; failed keys sort last | 5db34e4 | |
| L6 | `Path::strip_prefix` with a longer base returns `None` | 3bc7bae | |
| L7 | Report paths that decode to NUL give an error (Unix part; Windows part = PR #365) | 0017e6b | |
| L9 | `--isolate` works with `--stdin` | be849aa | |
| L10 | Directory entry errors are logged | 3378e9c | |
| L12 | Unlimited `RLIMIT_NOFILE` no longer wraps to -1 | ed6f223 | |
| L13 | `--max-prefix-size` below 4 KiB is honored | 2e35e75 | |
| L14 | `Transform` is no longer `Clone` (Drop deletes the temp dir) | c614bb3 | |

## Other GitHub issues fixed

| Issue | Fix | Commit |
|---|---|---|
| #353 (and the error reported in #324) | Temp file names stay within 255 bytes; fallback `.fclones-tmp.<random>` | e817f53 |
| #309, #336 | RPM no longer owns system dirs (`/usr/bin`, …); still built on buster for the glibc 2.28 baseline | 8694181 |
| #323, #337 | On Linux, mounts are read from `/proc/mounts` without stat'ing each one (no automount triggers) | 247268b |
| #298 | Windows files with the Hidden attribute count as hidden | f93eed8 |

Issue #164 was already fixed before this branch (c0865f5) and can be closed.

## Other changes

- c70ec76: `Path::file_name` no longer returns the Windows root as a file name. Found through a
  failing Windows test.
- 7e8d02d: the test helper canonicalizes paths with `dunce`, as fclones does. This fixes 7 Windows
  test failures that were only differences in path form.
- c9b6a7e: fixes a clippy warning in a test.
- d30854e: fixes the musl release builds, which PR #299 broke (`libc::ioctl` takes a `c_int`
  request on musl). Found while building v0.36.0.

## Release v0.36.0

Built with `packaging/package-internal.sh`, run directly in this container. The Docker image
(`packaging/Dockerfile`, Debian trixie) installs the same tools, but it has not been built,
because there is no Docker here.
- **glibc build:** `cargo zigbuild --target x86_64-unknown-linux-gnu.2.28` (Zig 0.13.0). The
  binary needs at most GLIBC_2.28, the same as v0.35.0, and the script checks this.
- **musl builds:** `cargo zigbuild` for x86_64 and i686.
- **Windows build:** MinGW.
- **Maintainer:** the deb Maintainer and RPM Packager are Christian Fiedler
  (`[package.metadata.deb]`); the copyright file still names the author of fclones.
- **Checks:** every binary passed a quick check (`group` + `link`; Windows under Wine).
- **musl tests:** can't be built, because the dev-only dependency `reflink` 0.1 doesn't
  compile for musl (pre-existing).

## Test status

- Linux: 224 lib, 4 bin and 2 doc tests pass; `cargo fmt` is clean.
- Windows, cross-compiled and run under Wine: 181 pass and 4 fail. The 4 failures are
  pre-existing; see `REMAINING.md`.

## Check on real systems before release

Nothing is pushed yet. These changes could only be partly tested here; details are in
`REMAINING.md` under "Not verified here".

- **Reflink dedupe** (PR #299 + 936f6bb): the path where the dedupe succeeds has never run (only
  ZFS and tmpfs here). Run `dedupe` once on btrfs or XFS.
- **RPM packaging** (8694181): build the Docker packaging image and install the RPM on
  Fedora or Rocky Linux 9.
- **Windows** (f93eed8, PR #365, c70ec76): tested only under Wine. Run the tests once on real
  Windows with NTFS.
- **Automount fix** (247268b): check that startup no longer triggers automounts on a system
  with autofs.
- **Release notes for M11** (46f62e8): existing hash caches are not reused (`hash_db_v2`), and
  changing a file's permissions or owner now causes a re-hash.

Upstreaming: the branch can be split into separate PRs, for example one per review fix or a
few grouped by area.
