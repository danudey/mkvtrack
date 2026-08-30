# mkvtrack

A terminal interface for looking at the audio and subtitle tracks in Matroska
files and changing their flags: which track is the default, which subtitle
track is forced, the track name, the language, and the accessibility flags.

It reads and writes Matroska itself. There is no dependency on mkvtoolnix,
ffmpeg or any other binary at runtime - only the Rust standard library,
`ratatui` and `crossterm`.

```
┌ Files (2) ─────────────────────┐┌ Tracks - A Film (2019).mkv ────────────────────────┐
│* A Film (2019).mkv             ││  ID  Type     Codec       Lang  Name         Flags │
│  Another.mkv                   ││  1   video    H.265       und                d     │
│                                ││> 2   audio    TrueHD      eng   English      D     │
│                                ││  3   audio    E-AC-3      jpn   Japanese           │
│                                ││  4   audio    AC-3        eng   Commentary   Com   │
│                                ││  5   subtitle ASS         eng   Full         d     │
│                                ││  6   subtitle ASS         eng   Signs        D F   │
└────────────────────────────────┘└────────────────────────────────────────────────────┘
```

## Build

```
cargo build --release
```

The binary is `target/release/mkvtrack`.

Rust 1.88 or later is needed. The floor comes from the proc-macro crates
`ratatui` depends on, not from the 2024 edition, which asks only for 1.85.

### A portable static binary

```
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The result is `target/x86_64-unknown-linux-musl/release/mkvtrack`: a static-pie
executable with no dynamic dependencies at all, so it runs on any x86-64 Linux
regardless of the glibc version. No musl C toolchain is needed, because nothing
here links against C code; the Rust musl standard library ships the C runtime
objects it needs.

## Use

```
mkvtrack [OPTIONS] [PATH]...
```

`PATH` is a Matroska file or a directory of them; with no path the current
directory is used. Recognised extensions are `.mkv`, `.mka`, `.mks`, `.mk3d`
and `.webm`.

| Option | Meaning |
| --- | --- |
| `-r`, `--recursive` | descend into subdirectories |
| `-b`, `--backup` | copy each file to `<name>.bak` before its first write |
| `-l`, `--list` | print the tracks and exit, no interface |
| `-h`, `--help` | usage |

### Keys

| Key | Action |
| --- | --- |
| `up`/`down`, `j`/`k` | move within the focused pane |
| `Tab` | switch between the file list and the track list |
| `[` / `]` | previous / next file |
| `d` | make this the default track of its type, clearing the flag on its siblings |
| `D` | clear the default flag on this track |
| `f` | toggle forced |
| `e` | toggle enabled |
| `h` / `v` / `t` | toggle hearing impaired / visual impaired / text descriptions |
| `o` / `c` | toggle original language / commentary |
| `n` | edit the track name (an empty value removes it) |
| `l` | edit the language |
| `s` / `S` | save this file / every changed file |
| `u` | discard the changes to this file and reload it |
| `?` | help |
| `q` | quit, with a prompt if anything is unsaved |

Nothing is written until you press `s` or `S`. Changed files are marked with
`*` in the file list.

In the Flags column, an upper case letter means the file states the flag
explicitly; a lower case letter means the value is the specification default
and the file says nothing. `D` default, `F` forced, `HI`/`VI` hearing or
visually impaired, `TD` text descriptions, `Orig` original, `Com` commentary,
`off` disabled.

## How reading works

Opening a file walks the Segment's children only as far as the first cluster,
which is everything a track list needs. A full walk costs one seek per cluster,
so it used to get slower the longer the film was; now it is a fixed handful of
reads whatever the size. Measured with `strace` on this build: 29 read and seek
calls for a 200 KB file and the same 29 for a 4 MB one with several hundred
clusters, where the earlier version took 823 for the latter. The rest of the
Segment is only walked when a file actually has to be rewritten, so that cost
falls on the one file being written rather than on every file the cursor passes
over.

On top of that, the whole directory is read on background worker threads as
soon as the program starts, so moving through the file list never waits on the
disk. The first file is read up front so the opening frame is not empty, and
the file pane title shows `Files (37/210 read)` until the sweep finishes. A
file the cursor reaches before the background threads get to it is read there
and then, and a result arriving late never overwrites a file that is already
loaded or has unsaved edits.

## How writing works

Only the Tracks element is ever rewritten. Two paths:

* **In place.** The new Tracks element fits in the space the old one occupies
  plus any Void padding that follows it, so it is written there and any
  leftover bytes become a Void. Nothing else in the file moves and no other
  byte is touched. This covers nearly every real edit, because flags are one
  byte values and most muxers leave padding after Tracks. A gap of exactly one
  byte is absorbed by encoding the element's size VINT one byte wider, which
  EBML permits.
* **Rewrite.** If the new Tracks element needs more room than that - typically
  when flags have to be added to a file that never wrote them, or a long name
  is set - the file is copied to a temporary file alongside it with everything
  after Tracks shifted. Every stored position is corrected: SeekHead
  `SeekPosition`, `CueClusterPosition` in the cue index, and the deprecated
  `Position` element in each cluster (widened if the new value no longer fits).
  The layout is solved to a fixed point, because those positions are
  themselves part of the elements whose size they change. The temporary file
  is only renamed over the original once it is fully written and flushed.

Other details worth knowing:

* Every element the tool does not understand is carried through byte for byte,
  so codec private data, content encodings, colour metadata and anything else
  in a track entry survive unchanged.
* A CRC-32 inside a rewritten master element is recalculated. Void padding
  inside Tracks is dropped, which frees a few bytes.
* Setting a flag to the value the specification already implies does not add
  an element to a file that does not have one; changing a flag away from the
  default always writes it out explicitly.
* Setting a language writes `Language` (ISO 639-2) and removes `LanguageBCP47`
  if the file had one, since a BCP-47 tag overrides `Language` and leaving both
  would be ambiguous. The status line says so when it happens.
* A file that changed on disk after it was read is refused rather than written
  with stale offsets.
* Files that use unknown-size clusters, which is rare outside live streams, can
  still be edited in place but cannot be rewritten; the tool says so instead of
  guessing.

## Layout

| File | Contents |
| --- | --- |
| `src/ebml.rs` | VINTs, element headers, CRC-32, and a lossless element tree |
| `src/mkv.rs` | locating the Segment, its children and Tracks; the track view model |
| `src/edit.rs` | the in place and rewrite save paths |
| `src/scan.rs` | reading the directory on background threads |
| `src/app.rs` | interface state and the edits it applies |
| `src/ui.rs` | rendering |
| `src/main.rs` | arguments, the terminal, the event loop |

## Tests

```
cargo test
```

The tests build Matroska files, edit them through the same code the interface
uses, and then check every cross reference in the result: that the Segment
covers the file, that its children tile it exactly, that each SeekHead entry
lands on the element it names, that each cue lands on a cluster, and that each
cluster's recorded position matches where it actually is. Both save paths and
the cluster-position widening branch are covered, as is the CRC-32
recalculation.

To run the same edits over a real file:

```
MKVTRACK_TEST_MKV=/path/to/file.mkv cargo test a_real_file -- --nocapture
```

It copies the file to a temporary directory first and prints where the result
was left, so it can be checked with `mkvinfo`, `ffprobe` or a player.

## Releases

Releases are made by [release-please]. Write commit messages in the
[conventional commits] form:

```
feat: add a keybinding to clear the forced flag
fix: keep the CRC-32 when a cue is widened
feat!: drop the --list flag
```

`feat` bumps the minor version, `fix` the patch version, and a `!` or a
`BREAKING CHANGE:` footer the major one. `docs`, `test`, `chore` and `ci`
commits do not cause a release on their own.

On every push to `main` the `release` workflow keeps a release pull request
open with the next version number and the changelog entries earned so far.
Merging that pull request bumps `Cargo.toml` and `Cargo.lock`, updates
`CHANGELOG.md`, tags the commit `vX.Y.Z`, and creates the GitHub release. The
same workflow then builds the binaries and attaches them to it:

| Asset | Target |
| --- | --- |
| `mkvtrack-<version>-macos-universal.tar.gz` | `aarch64` and `x86_64` macOS in one binary |
| `mkvtrack-<version>-x86_64-pc-windows-msvc.zip` | Windows on x86-64 |
| `mkvtrack-<version>-aarch64-pc-windows-msvc.zip` | Windows on ARM64 |
| `mkvtrack-<version>-x86_64-unknown-linux-musl.tar.gz` | Linux on x86-64, static |
| `mkvtrack-<version>-aarch64-unknown-linux-musl.tar.gz` | Linux on ARM64, static |
| `mkvtrack-<version>-riscv64gc-unknown-linux-musl.tar.gz` | Linux on RISC-V 64, static |

Each asset has a `.sha256` file beside it.

The Linux binaries link against musl statically, so they run on any
distribution. Nothing in the crate or its dependencies is written in C, so
`rust-lld` links all three architectures from the same runner; there is no
cross compiler or container in the build.

To reproduce a release build locally, add the target as in
[A portable static binary](#a-portable-static-binary) above. The workflow also
passes `RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static"`, which
the `aarch64` and `riscv64gc` targets need: unlike `x86_64`, they do not link
statically by default and `rust-lld` is not their default linker.

[release-please]: https://github.com/googleapis/release-please
[conventional commits]: https://www.conventionalcommits.org/
