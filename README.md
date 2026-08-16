# Roam

A cross-backend desktop file browser. Local disk, S3, GCS, Azure Blob, WebDAV and
SFTP through one window, built on [OpenDAL] and [GPUI].

```
cargo run                 # browse $HOME
cargo run -- /some/path   # browse somewhere else
```

Saved connections live in `profiles.toml` under the platform config directory
(`ROAM_CONFIG` overrides the path). Credentials are **not** kept there — they go
to the system keychain, and the config file records only which keys to look up.

## Layout

```
crates/roam-core/   No gpui dependency; testable without a window.
crates/roam-ui/     GPUI views, plus the icon assets the app must supply itself.
crates/roam/        The binary.
docs/DESIGN.md      Design, decisions, and the things that turned out to be wrong.
scripts/            Local servers for the integration tests.
```

`roam-core` owns everything that talks to a backend. The one rule that matters:
**an `Operator` is never awaited on a GPUI thread** — OpenDAL's network services
need a tokio reactor, and GPUI runs its own executor. Everything crosses that
boundary through `roam_core::rt::Rt::spawn`. See `docs/DESIGN.md` §2.

## Tests

```
cargo test --workspace          # 303 pass; 258 are real, 45 skip (see below)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

258 of those need nothing but a compiler: 163 in `roam-core`, 88 views, 7 scale.
The other 45 are the backend integration tests, which report success by skipping
when no server is configured — `scripts/test-backends.sh` is what makes them run
for real.

The offline suite covers the whole app, including the views: UI tests drive real
GPUI views with `TestAppContext` and dispatch real keystrokes. It also rasterises
every icon with the same `resvg` gpui uses and asserts each one puts ink on the
page — `gpui-component` names its icons but ships none of them, and both layers
below fail silently, so a missing asset source renders every icon in the app as
empty space. See `docs/DESIGN.md`.

Two things it cannot cover, so both have their own path:

**Real protocols.** Everything above runs against OpenDAL's local `fs` service,
which never touches the network. The integration tests run against real servers
in Docker:

```
scripts/test-backends.sh test all     # start the servers, run 47 integration tests
scripts/test-backends.sh test s3      # or just one: s3 | azblob | gcs | webdav | sftp
scripts/test-backends.sh down         # stop and clean up
```

They skip with a printed note when their endpoint variable is unset, so
`cargo test` stays green without Docker. Every one of them earned its place —
`docs/DESIGN.md` §14 lists the bugs they found, including uploads over 8 MB
failing on WebDAV and resumed downloads breaking on Apache's weak etags.

**Scale.** The design claims a 100k-entry directory stays usable because sorting
and filtering build an index view rather than touching the rows. That is measured
rather than asserted — release build, 100k entries: an 83 ms name sort, a 6.6 ms
filter, and a 12.3× cost ratio against 10k where *n* log *n* predicts ~12×. On a
real 100k-file directory the first batch of rows arrives in **6.9 ms** while the
full scan takes 1.13 s. Numbers and thresholds live in
`crates/roam-core/tests/scale.rs`; the disk-backed part is opt-in:

```
ROAM_SCALE_FS=1 cargo test --release -p roam-core --test scale -- --nocapture
```

Opening a 60k-file directory in the release build settles at 0.4% CPU and 148 MB
RSS — idle CPU being the interesting figure, since the transfer panel polls at
10 Hz and either that or the render loop failing to stop would show up there.

**Real rendering.** GPUI's `TestPlatform` has a stub text system, so layout and
image decoding never happen in tests. The examples render for real; if one fails
to lay out, the process aborts instead of staying up:

```
cargo run -p roam-ui --example connection_dialog
cargo run -p roam-ui --example name_dialog
cargo run -p roam-ui --example transfer_panel
cargo run -p roam-ui --example preview_panel [file]
cargo run -p roam-ui --example versions_dialog
```

## What works

Browsing with a virtualized table, streamed listings and per-session caching ·
multiple tabs, each on its own backend · connection profiles with keychain
credentials · capability-driven context menus · new folder, rename, duplicate,
delete with confirmation · uploads, downloads, directory moves and cross-backend
copies through a transfer engine with progress and cancellation · resumable
downloads · drag-in from the Finder · preview for text, Markdown and images ·
a lazily-loaded directory tree · object version history · keyboard navigation and
filtering · light and dark themes.

## CI

`.github/workflows/ci.yml` splits along the same seam the code does:

- **core (Linux)** — `roam-core` has no gpui dependency, so it builds on a plain
  runner with no graphics stack. That makes it the natural home for the backend
  integration tests too, since every server they need is a Linux container.
- **ui (macOS)** — the view layer needs gpui, so it runs where the app ships.

The `core` job has been **executed on a real Linux runner** with `act`, and it
passes end to end: fmt, clippy, 163 + 22 + 23 + 7 offline tests, all five backend
servers, 23 S3 and 22 azblob/gcs/webdav/sftp integration tests, the 100k-entry
scale test, and the `if: always()` teardown. That is also what verifies the claim
this split rests on — `roam-core` really does build and test on Linux with no
graphics stack.

Running it there found three bugs in `scripts/test-backends.sh` that were
invisible locally, all one root cause: `docker run -v` resolves the source path in
the **daemon's** filesystem, not the step's. Those are the same path only when the
step runs directly on the host. Inside a container the mount silently delivers an
empty directory — so the sftp public key, the MinIO bucket and the fake-gcs bucket
all have to be created through an API rather than by writing to a mounted path.

```
act -j core -P ubuntu-latest=catthehacker/ubuntu:act-latest
```

## What is not verified

- **The real cloud services.** MinIO, Azurite, fake-gcs-server and Apache are
  compatible, not identical: SigV4 regional details, throttling and eventual
  consistency need actual accounts.
- **GCS large objects.** OpenDAL's concurrent GCS writer uses the XML multipart
  endpoint, which `fake-gcs-server` answers with 404.
- **Screenshots.** `screencapture` is blocked on the development machine, so the
  UI is verified by running it, not by looking at it.
- **GitHub's own runners.** The `core` job ran under `act` on arm64 Linux
  containers, not on a hosted x86_64 `ubuntu-latest`, and `rust-cache` no-ops
  locally. The `ui` job cannot be checked this way at all: `act` has no macOS
  image, and substituting Linux would pass gpui tests that could never run there.
  Its commands are run directly on macOS instead — the same OS as the runner.

`sftp` is enabled but stands apart from the rest: it drives the system `ssh`
binary, so it needs one in `PATH` and its key must be a file on disk rather than a
keychain entry.

[OpenDAL]: https://opendal.apache.org
[GPUI]: https://gpui.rs
