# Native packages

We use [cargo-bundle](https://github.com/burtonageo/cargo-bundle), pinned to 0.12.0.
Run packaging from a checkout on the target OS:

```sh
cargo install cargo-bundle --version 0.12.0 --locked
bash scripts/package.sh
```

The script builds the desktop app with the locked dependencies and all features,
then packages that exact executable. Output goes to `dist/`.

## macOS

```sh
brew install ffmpeg cmake pkg-config llvm dylibbundler
```

`OpenXCloud-macos-<architecture>.zip` contains `OpenXCloud.app`. FFmpeg and its
transitive dynamic libraries are copied into the app and their load paths rewritten.
Packaging rejects unresolved external library paths and verifies an ad-hoc signature.
It also includes the project licence and third-party notices.

These are test artifacts, not notarized releases. Developer ID signing and Apple
notarization are still needed for public distribution. The package targets the
build machine's architecture; CI currently builds Apple Silicon on macOS 15
using [GitHub's standard runner labels](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job).
Although the app's deployment setting is macOS 13, bundled Homebrew libraries may
require a newer macOS. Validate the entire bundle on each supported OS before
advertising a minimum version. Runtime-library licences and corresponding source
requirements must also be reviewed before publishing binaries.

## Linux (Debian/Ubuntu)

CI builds on Ubuntu 24.04. Install prerequisites:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config cmake clang libclang-dev \
  ffmpeg dpkg-dev libssl-dev python3 libasound2-dev libudev-dev libavcodec-dev libavformat-dev \
  libavutil-dev libavdevice-dev libavfilter-dev libswscale-dev libswresample-dev \
  libx11-dev libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers
```

`OpenXCloud-linux-<architecture>.deb` uses system runtime libraries. The packaging
script derives versioned dependencies with `dpkg-shlibdeps` and adds the dynamically
loaded Vulkan loader. Install it with `sudo apt install ./dist/OpenXCloud-linux-*.deb`
on a compatible distribution. Packages are tied to their build distro and
architecture; they are not portable Linux binaries. A working graphics driver and
desktop session are required for playback.

## Windows (x64)

Windows uses its own script, run from PowerShell (5.1 or 7) on Windows 10/11:

```powershell
$env:FFMPEG_DIR = 'C:\path\to\ffmpeg-n9.0.2-...-win64-lgpl-shared-9.0'
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
.\scripts\package-windows.ps1
```

Prerequisites: Rust with the MSVC toolchain and the Windows SDK (Visual Studio Build
Tools; `rc.exe` embeds the icon, version info and manifest), LLVM for bindgen,
[Inno Setup 6](https://jrsoftware.org/isinfo.php) (or pass `-Iscc path\to\ISCC.exe`),
and an FFmpeg 9 **shared** build. CI uses the LGPL shared build from
[BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds), pinned by URL and SHA-256
in `.github/actions/native-deps/action.yml`. BtbN keeps month-end builds published, so
bump the pin to another month-end `autobuild-*` release.

Outputs in `dist/`:

- `OpenXCloud-windows-x64-setup.exe`: Inno Setup installer
  (`packaging/windows/openxcloud.iss`). Installs per user without admin rights by
  default; its first page also offers an all-users install. Adds a Start menu entry,
  an optional desktop shortcut, an uninstaller, and an Apps & features entry.
  Re-running a newer installer upgrades in place and closes a running OpenXCloud
  first. `/VERYSILENT` (add `/ALLUSERS` or `/CURRENTUSER`) installs unattended.
  Uninstalling keeps settings, sign-in tokens (Windows Credential Manager) and logs.
- `OpenXCloud-windows-x64.zip`: the same files as a portable folder.

What the script does:

1. Builds with the static MSVC runtime (`+crt-static`, in `target\windows-package`),
   so no Visual C++ redistributable is needed.
2. Reads the PE import tables of the exe and, transitively, of every DLL it bundles.
   It copies the FFmpeg DLLs that are actually imported and fails on any other
   non-system DLL (including `vcruntime140.dll`).
3. Adds `LICENSE.txt`, `THIRD_PARTY_NOTICES.md`, FFmpeg's licence, and
   `licenses\FFmpeg-BUILD.txt`, which records the exact FFmpeg build (version,
   configure line, archive URL and SHA-256) and its source location.
4. Starts the staged `openxcloud.exe --version` once. A missing DLL fails process
   creation, so this proves the bundle is complete.
5. Zips the folder and compiles the installer.

Release builds use the GUI subsystem (no console window). Started from a terminal,
the app attaches to it, so `--help`, `--smoke` and the stderr log still show.
Requirements: 64-bit Windows 10 1809 or newer. NVIDIA/Intel Video Super Resolution
needs the DX12 adapter and the driver setting (see the README).

The installer and the exe are **not code-signed**. SmartScreen shows "Windows
protected your PC" until the publisher has reputation. Signing needs a code-signing
certificate (for example Azure Trusted Signing) and a `signtool sign` step over
`openxcloud.exe` and the setup exe. Inno Setup is free for non-commercial use, which
covers this project; check its licence before reusing the script commercially.

## GitHub Actions

- `ci.yml`: runs on pull requests, pushes to `main`, merge queues and manual dispatch.
  - `Format`: `cargo fmt --check` (no native dependencies, finishes in seconds).
  - `Clippy and tests`: Clippy and the full workspace test suite on Linux and macOS.
    The synthetic H.264 test fixture is generated with FFmpeg.
  - `Portable transport`: protocol and WebRTC tests on Windows.
  - `Desktop build (windows)`: Clippy for the desktop app against the pinned FFmpeg,
    plus Clippy and tests for the GPU crate with Video Super Resolution compiled in.
    Runners have no GPU, so the hardware tests skip.
  - `Package`: native packages for macOS, Linux and Windows, uploaded as artifacts
    retained for 14 days. Built only on pushes to `main`, manual dispatch, and
    releases, never for pull requests. Inno Setup is pinned by URL and SHA-256 and
    installed in portable mode.
  - Native libraries come from the shared `.github/actions/native-deps` action.
  - Build caching uses [`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache).
    Only `main` writes caches; pull requests and tags restore them read-only so
    short-lived branches don't evict the repository's 10 GB cache. `cargo-bundle` is
    cached separately instead of being compiled on every package run.
- `release.yml`: pushing a version tag such as `v0.1.0` runs `ci.yml` with packaging
  enabled, then uploads the macOS Apple Silicon ZIP, Ubuntu 24.04 amd64 `.deb`,
  Windows x64 installer and portable ZIP, and `SHA256SUMS` to a GitHub Release. Only the publish job has write permission.
- `dependabot.yml`: weekly grouped updates for GitHub Actions and Cargo dependencies.

### Publish a release

Set `workspace.package.version` in `Cargo.toml`, update `Cargo.lock`, and commit
and push the release changes (including the release workflow). Tag that commit:

```sh
git tag -a v0.1.0 -m "OpenXCloud 0.1.0"
git push origin v0.1.0
```

The tag must match the Cargo version exactly. Tags containing a prerelease suffix
(such as `v0.2.0-beta.1`) create prereleases. Branch pushes and pull requests only
produce CI artifacts. No additional token or signing secret is required: the
publish job uses GitHub's built-in `GITHUB_TOKEN`.

All CI checks and all three platform packages must succeed before a draft release is
created. The draft becomes public only after all five assets upload. If uploading fails,
rerun the failed jobs to finish the draft. Published release assets are never
replaced by a rerun; use a new version tag for corrections. Downloaded packages
can be checked with `sha256sum -c SHA256SUMS` (macOS: `shasum -a 256 -c SHA256SUMS`).

Publishing does not add Developer ID signing, notarization or Windows code signing.

For local workspace tests, first run `bash scripts/generate-test-fixture.sh`.
The generated video stays ignored by Git; no account or recorded stream is needed.
