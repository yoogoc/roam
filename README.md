# Roam

A cross-backend desktop file browser. Local disk, S3, GCS, Azure Blob and WebDAV
through one window, built on [OpenDAL] and [GPUI Kit].

```
cargo run                 # browse $HOME
cargo run -- /some/path   # browse somewhere else
```

Saved connections live in `profiles.toml` under the platform config directory
(`ROAM_CONFIG` overrides the path). **Credentials are stored there too, in
plaintext**, at file mode `0600`. That is deliberate — the system keychain asked
for permission on every launch — but it means the file is not safe to sync,
commit, or paste into an issue, and anyone who can read it has the credentials.

Connections are entered through a form generated per backend: pick S3, GCS, Azure
Blob, WebDAV or local disk and it asks for that service's fields, masks the
secret ones, and composes the OpenDAL URI itself. The picker lists what the build
supports, with the same five connection types on every platform.

SFTP has been removed. Existing SFTP profiles remain readable and removable, but
cannot connect or be edited. Other saved connections continue to work.

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
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
```

`roam-core` carries 199 unit tests, 40 backend integration tests and 12 scale tests.
Protocol tests skip when no server is configured; `scripts/test-backends.sh`
runs them against local servers. The UI suite has 112 tests (including two that
require MinIO), and macOS CI now enforces their results.

GUI dependencies come from `gpui-kit 0.6.1`, with exact resolved versions recorded
in `Cargo.lock`. Test harnesses enable `test-support` and permit real Tokio IO
wakeups. The old macOS `Root` initialization failure is resolved with this stack.

The view tests drive real GPUI views with `TestAppContext` and
dispatch real keystrokes. They also rasterise every icon the views name, with the
same `resvg` gpui uses, and assert each one puts ink on the page. Upstream ships
the icons now (`gpui_kit::assets::Assets`), but an application that registers no
asset source still gets *nothing drawn*, silently, so the check stays.

Two things it cannot cover, so both have their own path:

**Real protocols.** Everything above runs against OpenDAL's local `fs` service,
which never touches the network. The integration tests run against real servers
in Docker:

```
scripts/test-backends.sh test all     # start the servers, run backend integration tests
scripts/test-backends.sh test s3      # or just one: s3 | azblob | gcs | webdav
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

The `core` job runs offline tests and tests against four local protocol servers:
MinIO, Azurite, fake-gcs-server and Apache WebDAV. The server fixtures use APIs to
create buckets, so they also work when the CI step runs inside a container and
Docker bind mounts resolve on a different host.

```
act -j core -P ubuntu-latest=catthehacker/ubuntu:act-latest
```

## Packaging

```
cargo install cargo-packager --locked
cargo packager -p roam --release --formats app,dmg
```

One config in `crates/roam/Cargo.toml` covers every platform's formats — `.app` and
`.dmg`, `.deb` and `.AppImage`, `.msi` and the NSIS installer.

The packaging configuration uses the hardened runtime without App Sandbox.
Removing SFTP also removes the external SSH executable requirement. App Sandbox
and App Store distribution have not been validated; the existing distribution
configuration is retained. See `docs/PACKAGING.md` for signing and packaging.

## What is not verified

- **The real cloud services.** MinIO, Azurite, fake-gcs-server and Apache are
  compatible, not identical: SigV4 regional details, throttling and eventual
  consistency need actual accounts.
- **GCS large objects.** OpenDAL's concurrent GCS writer uses the XML multipart
  endpoint, which `fake-gcs-server` answers with 404.
- **Notarization and Gatekeeper.** Packaging is verified up to a working, ad-hoc
  signed `Roam.app` and `.dmg` that launch. Notarizing needs a Developer ID
  Application certificate, which this machine does not have — so "double-clicks on
  someone else's Mac" is untested.
- **GitHub's own runners.** The `core` job ran under `act` on arm64 Linux
  containers, not on a hosted x86_64 `ubuntu-latest`, and `rust-cache` no-ops
  locally. The `ui` job cannot be checked this way at all: `act` has no macOS
  image, and substituting Linux would pass gpui tests that could never run there.
  Its commands are run directly on macOS instead — the same OS as the runner.

[OpenDAL]: https://opendal.apache.org
[GPUI Kit]: https://github.com/longbridge/gpui-kit
