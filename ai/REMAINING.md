# What remains open on branch `consolidate-upstream`

This covers what was not applied or fixed, and what could not be verified here. What was fixed
is in `FIXES.md`; the full reasoning is in `GOAL.LOG`.

## Upstream pull requests not applied (6 of 18)

| PR | What it does | Why not applied |
|---|---|---|
| #339, #340, #341 (stacked) | Before acting, re-reads every file and checks it against the report hash; #341 adds a JSON dry-run | Built and compared with this branch. Two bugs: (1) `link`/`dedupe` stops after the first device of a group that spans devices, silently skipping the rest; (2) the hash check assumes a full-file hash, which is wrong for `--skip-content-hash`, `--transform` and hard-link-only groups, so those groups are rejected and nothing is processed. It also doubles the I/O by default, and #341 takes the dry-run format from the group run's `--format`. The idea (#84) is good; the stale-file part is covered by M11 (46f62e8). |
| #335 | Removes files that are already reflinked copies of each other from the groups | Changes default behavior with no flag: `remove`/`move` users stop seeing those duplicates, and replica counts for `--isolate`, `--rf-under` and `--unique` change. Can't be tested here (ZFS has no FIEMAP; no btrfs/XFS). Acceptable as an opt-in flag. |
| #317 | Major dependency upgrades, edition 2024, minimum Rust 1.74 → 1.89, 27-file clippy cleanup | Fixes no bug. bytesize 2 changes the sizes printed in reports, and byte-unit 5 changes how size arguments are parsed. Touches every file, so it conflicts with all fixes here. Upgrade dependencies one at a time instead. |
| #259 | README shell aliases | The `dedupe` alias shadows `fclones dedupe`, `--priority oldest` deletes the oldest copies, and `2>/dev/null` hides errors. The upstream reviewer questioned it too. |

## Review finding not fixed

- **L8**: groups made only of hard links (with `-H`) are reported with hash `000…0`. Left
  deliberately: a real hash would mean reading data only to display it.

## GitHub issues not fixed

**Can't reproduce here, or not a bug in fclones:**
- #322: hard-link limit (EMLINK; NTFS allows 1024 links). Needs a filesystem with a low limit and
  a design change: switch the link target when the limit is hit. A failing link is already rolled
  back safely.
- #250: hard links reported as duplicates on ZFS. Not reproducible; most likely real copies were
  present, as in #268. Needs the reporter's data.
- #326: grouping large files is slow on exFAT/macOS. Not reproducible without that setup.
- #174: PowerShell re-encodes piped output. This is PowerShell behavior; workarounds are in the
  issue.
- #270: `--path '.\'` finds nothing on Windows. Path patterns must match whole paths, so this is
  expected behavior.
- #249: Android/termux link creation fails with EACCES. An Android storage restriction; `ln` fails
  too.
- #324: "flag to ignore errors". fclones already continues after a failed file; the reported error
  is fixed by #353 (e817f53).

**Need a decision by the maintainer:**
- #253, #258: `move` keeps at least one replica per group by design, so a `--unique` report moves
  nothing.
- #360: `--unique` with `--isolate` counts replicas per root, by design.

**Feature requests:** #31 #51 #94 #133 #145 #146 #156 #157 #159 #170 #195 #197 #254 #255 #263
#272 #279 #284 #285 #286 #287 #292 #300 #301 #305 #307 #311 #313 #314 #315 #316 #319 #321 #325
#327 #330 #354 #355 #356 #358 #368.

**Questions, documentation, or packaging outside this repo:** #192 #230 #251 #262 #265 #266 #294
#296 #297 #308 #361 #362 #363.

## Not verified here

- **FIDEDUPERANGE success path (PR #299 + 936f6bb):** only the "not supported" path ran (ZFS,
  tmpfs). Needs btrfs or XFS.
- **RPM fix (8694181):** the script steps were tested on the real `.deb` with buster-style
  `%dir` lines injected. The Docker packaging image was not built, and running buster's
  alien 8.95 was blocked by the permission policy.
- **Automount fix (247268b):** checked with strace (13 → 0 statfs calls at startup). The hang
  itself was not tested, because there's no autofs here.
- **PR #334 on macOS:** only a Linux equivalent test was run.
- **M11 trade-off:** the cache now also checks ctime, so `chmod`, `chown` and other metadata
  changes cause a re-hash. The old cache file is not reused (`hash_db_v2`).

## Known test failures and warnings

- 4 Windows tests fail under Wine. They already failed before this branch and are not code bugs:
  `test_soft_link_command_replaces_file_with_a_link` (Wine's symlink support), and
  `encode_decode_stfu8`, pattern `add` and `input_escaping` (they hard-code Unix separators or the
  Unix escape character).
- Remaining clippy warnings are in pre-existing code or in PR #299's code, left as authored.
- `fclones/target/` holds test scratch directories created by the existing tests.
