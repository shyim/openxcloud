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

## GitHub Actions

- `desktop.yml`: macOS/Linux formatting, workspace tests, Clippy, native packages,
  and downloadable artifacts retained for 14 days. Runs on pushes, pull requests,
  and manual dispatch. The synthetic H.264 test fixture is generated with FFmpeg.
- `transport.yml`: protocol and WebRTC tests on macOS, Linux, and Windows.

- `release.yml`: pushing a version tag such as `v0.1.0` runs both reusable check
  workflows, then uploads the macOS Apple Silicon ZIP, Ubuntu 24.04 amd64 `.deb`,
  and `SHA256SUMS` to a GitHub Release. Only the publish job has write permission.

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

Both platforms and transport tests must succeed before a draft release is created.
The draft becomes public only after all three assets upload. If uploading fails,
rerun the failed jobs to finish the draft. Published release assets are never
replaced by a rerun; use a new version tag for corrections. Downloaded packages
can be checked with `sha256sum -c SHA256SUMS` (macOS: `shasum -a 256 -c SHA256SUMS`).

Publishing does not add Developer ID signing or notarization. Windows desktop
packaging is pending native build and playback validation; its transport CI is not
a desktop compatibility guarantee.

For local workspace tests, first run `bash scripts/generate-test-fixture.sh`.
The generated video stays ignored by Git; no account or recorded stream is needed.
