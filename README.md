# mylocate

[繁體中文](README.md) | [简体中文](README.zh-CN.md) | **English**

[![CI](https://github.com/iamjlintw/mylocate/actions/workflows/ci.yml/badge.svg)](https://github.com/iamjlintw/mylocate/actions/workflows/ci.yml)

Instant file search for macOS, built to feel like [Everything](https://www.voidtools.com/) on Windows: **results as you type, with an index that is never stale**.

Written in Rust, with no dependency other than `memchr` (SIMD substring search).

## Measured (M2 Pro, 3.27M files under `$HOME`)

| | Index build | Single query | Coverage |
|---|---|---|---|
| **mylocate** | 16 s (first run only) | **6.6 ms** (P95 7.0) | 100% |
| Spotlight `mdfind` | background daemon | 50 ms – 3.9 s | **42%** |
| `searchfs` (filesystem catalog) | not needed | 46.8 s per query | 100% |
| `find` | not needed | 10.7 s per query | 100% |
| `locate` (needs `updatedb`) | one full scan | hundreds of ms – seconds | weekly refresh |

Coverage was verified entry by entry against `find`: `webpack.config`, `README`, `package-lock.json`, single-character queries, multi-keyword AND, and directory searches all matched 100%.

> Spotlight reaches only 42% because it deliberately skips `node_modules`, `.git`, and any directory containing `.noindex` — which leaves it half-blind on a development machine.

## Requirements

- macOS (Apple Silicon and Intel)
- [Rust toolchain](https://rustup.rs/), to build
- Xcode Command Line Tools (`xcode-select --install`)
- [fzf](https://github.com/junegunn/fzf): optional, only used by interactive mode `ml -i`

Building the initial index and watching for changes requires reading every file under `$HOME`. If the scan finds noticeably fewer files than expected, add your terminal (or `ml`) under System Settings → Privacy & Security → Full Disk Access.

## Installation

```sh
./install.sh
```

The script builds the binary, installs `ml` into `~/.local/bin`, creates the first index, and asks whether to register a launchd agent so the daemon starts at login.

## Usage

```sh
ml webpack.config        # search (case-insensitive)
ml webpack config        # multiple keywords are ANDed
ml -n 0 README           # no result limit (default 50)
ml -i                    # interactive mode, filters as you type (needs fzf)
ml stats                 # index and daemon status
ml index [path]          # rebuild the index (defaults to $HOME)
ml daemon [path]         # run the resident service in the foreground
```

If the daemon is running, queries go through it automatically; otherwise `ml` reads the index file directly (slower, around 50 ms).

## Uninstall

```sh
launchctl unload -w ~/Library/LaunchAgents/com.mylocate.daemon.plist   # if the agent was registered
rm -f ~/Library/LaunchAgents/com.mylocate.daemon.plist
rm -f ~/.local/bin/ml
rm -rf ~/Library/Caches/mylocate                                        # the index file
```

## How it works

```
                  ┌─ Full scan (first run only, 16 s) ─┐
   APFS ─────────►│ getattrlistbulk + openat recursion │──► Index file (105 MB)
                  └────────────────────────────────────┘         │
                                                                 │ mmap (no parsing)
   File change ──► FSEvents ──► re-list dir + diff ──► delta ────┴──► Query 6.6 ms
```

Three design decisions carry the whole thing:

**1. The index is persistent, so the full scan is paid for exactly once.**
Each node is a 12-byte `repr(C)` struct and the index file is that array verbatim, so loading it is an `mmap` that hands back a usable slice — there is no deserialization step. After a reboot, FSEvents replays offline changes via `sinceWhen` to catch up.

**2. Queries never wait on a lock.**
The multi-hundred-MB base is shared through an `Arc`; an update copies only the thin delta layer on top of it, then atomically swaps in the new snapshot. A query grabs the `Arc` and lets go immediately, so it can never be blocked by an update.

Getting here took two wrong turns. The updater originally called `list_dir` while holding the write lock, and since cache activity from various apps under `$HOME` essentially never stops, query latency degraded to 3.6 s. Moving the I/O outside the lock brought the median back to 7 ms, but occasionally colliding with the write lock still spiked to 100 ms. Only after switching to copy-on-write did the maximum drop to 7.4 ms.

**3. Search is a linear scan, not an inverted index.**
Everything's own description is "optimized multi-threaded strstr on every single filename" — it uses no index structure either. We hand the entire lowercased filename arena to a vectorized `memmem` as one large haystack, then binary-search back to the record that was hit. Memory access is fully sequential and the comparison runs on SIMD.

## Why not the approaches that look better on paper

Each of these was measured, not dismissed on intuition:

| Approach | Why not |
|---|---|
| **SQLite for metadata** | Index building would not get faster (it still has to walk the APFS B-tree once), `LIKE '%x%'` cannot use an index and degrades into a full table scan, and updates are slower than mutating an in-memory array. Everything doesn't use SQL either. |
| **Reading the raw device, like Everything does** | Everything is fast because the NTFS MFT is one contiguous region — a sequential read yields all metadata at once. APFS has no equivalent, and with FileVault enabled the raw device reads back as ciphertext. |
| **`searchfs(2)` filesystem catalog search** | Measured at 46.8 s for one full-disk pass, slower than `find`. The "100× faster than find" figure dates from HFS+; APFS changed the structure and the advantage is gone. |
| **`updatedb` / `locate`** | Internally a full `find` scan, scheduled weekly, and queries have to linearly decompress and scan an entire text file. It loses on all three counts. |
| **Inverted index / trigram** | Memory would grow several times over and updates get complicated, while a linear scan already takes only 6.6 ms. |

### The 16-second full scan is a physical floor

`kern.maxvnodes` is 247,308 — enough for 7.5% of all files — so the vast majority of directory entries must be read cold from the B-tree on SSD. This is I/O bound, not CPU bound. Measured `sys` time is 70–90 s while wall time stays fixed at 13–16 s, effective parallelism plateaus at 5 (the NVMe queue depth limit), and `user` time is only 0.6 s.

Directions that were tried and **confirmed ineffective**: trimming the attributes requested from `getattrlistbulk` (sys time dropped 22%, wall time got worse), switching to `openat` to avoid re-resolving absolute paths (no change), and adding threads (no gain past 8).

The one optimization that worked was fixing a thundering herd: the scanner used to `notify_all()` after finishing each directory, waking every thread 445,000 times to contend for the same lock. Removing it took 12 threads from 20.3 s down to 11.8 s.

## Known limitations

- The index covers a single volume (recursion across volume boundaries is blocked during the scan), so external drives are not included.
- File contents are not indexed, only filenames — use `rg` to search contents.
- File size and modification time are not stored: both fields live in the inode record, and requesting them costs the kernel an extra B-tree lookup per file. Results are usually only scanned a few dozen at a time, so running `stat` on just those few when needed is the better trade.
- Event overflow only triggers a rescan of the affected subtree; a full rebuild happens only if the root is moved away or the event id wraps. A rebuild also runs once the delta exceeds 200,000 entries, and queries continue to be served throughout.
- The index file lives inside the watched tree, so the daemon ignores its own events — otherwise the hundreds of MB written during a rebuild would trigger another rebuild, producing a rebuild→event→rebuild loop (measured: 7 rebuilds within 60 seconds).

## License

MIT — see [LICENSE](LICENSE).
