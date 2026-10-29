# Releases and package channels

| Channel | GUI | CLI |
| --- | --- | --- |
| Homebrew tap `mizorewww/tap` | Cask `course2md-gui` (Apple Silicon, macOS 15+) | Formula `course2md` |
| Homebrew prerelease channels | Cask `course2md-gui@{alpha,beta,rc}` | Formula `course2md-{alpha,beta,rc}` (keg-only) |
| AUR (stable only) | `course2md-gui-bin` (x86_64) | `course2md-bin` (x86_64, aarch64) |

GUI packages contain a matching private CLI engine and do not install a terminal `course2md` command, so they coexist with the CLI package.

Desktop archives (DMG/app ZIP, Windows ZIP, Linux tar.gz) bundle conversion tools; system libraries and optional models remain separate. They include ffmpeg and ffprobe, yt-dlp's official onedir build pinned in `desktop/scripts/ytdlp_bundle.py` (a `yt-dlp/` directory beside the engine, or in `Contents/Resources` of the macOS app, whose universal files are thinned to arm64 and each signed with the hardened runtime; the one-file builds spent 8–14 s unpacking themselves on every call), and on Windows the official aria2 build pinned in `desktop/scripts/package.py`. Windows and Linux archives also bundle the official uv (beside the engine; it runs the Intel NPU worker and fetches OpenVINO on first use) and llama.cpp's Vulkan `llama-server` with its CPU and Vulkan backends (in `llama.cpp/`), pinned in `desktop/scripts/prebuilt.py`; CUDA, ROCm and SYCL builds of llama.cpp remain a separate install. On Linux the bundled llama-server needs the system's `libvulkan.so.1`, `libgomp.so.1` and OpenSSL 3; on Windows it needs the Microsoft Visual C++ 2015–2022 runtime (`msvcp140.dll`, `vcruntime140.dll`, `vcruntime140_1.dll`). Those DLLs are redistributable only under the distributor's own Visual Studio licence and are not published as pinned, hash-verifiable files, so the archive does not carry them: **Run checks** and setup detect them in System32 (or beside llama-server) and link Microsoft's installer, https://aka.ms/vs/17/release/vc_redist.x64.exe. Inside an app package (the directory holding `course2md-desktop` beside the engine, its `llama.cpp/` and, in a macOS bundle, `Contents/Resources/bin`) the engine runs a bundled tool before one on PATH; `COURSE2MD_FFMPEG`, `COURSE2MD_FFPROBE` and `COURSE2MD_YTDLP` name a binary explicitly, and `course2md doctor` and **Run checks** show where each came from. ffmpeg is LGPL-2.1 on every platform, built from the pinned upstream release with dav1d and the system's TLS (Secure Transport, Schannel on Windows, and on Linux the shared GnuTLS: `libgnutls.so.30` from the user's system) and autodetection off; `desktop/scripts/ffmpeg_bundle.py` holds the pins and options and writes them into `ffmpeg-build.txt` beside the licenses. Windows binaries are cross-compiled with the pinned llvm-mingw toolchain on Linux or macOS (`python3 desktop/scripts/ffmpeg_bundle.py --target Windows`); in CI the `windows-ffmpeg` job builds them and the Windows packaging job takes its artifact. The source builds need `meson`, `ninja`, `make`, `nasm` (x86-64 targets) and `pkg-config` (pip on macOS, apt on Linux) and, on Linux, `libgnutls28-dev`. Release builds stop if a tool cannot be bundled or fails its check (ffmpeg must list https; ffprobe, uv and llama-server must run); `--debug` builds fall back to PATH unless `--require-tools` is given, as CI does. Built tools and downloads are cached in `desktop/.cache/tools` and re-verified against their hashes and build manifest on reuse. aria2 is not bundled on macOS or Linux, where upstream publishes no build; without it models download over one connection.

The Homebrew formula and both AUR packages depend on the system ffmpeg, yt-dlp and aria2; the AUR GUI package installs only the app binaries from the archive, so it uses Arch's ffmpeg and yt-dlp. The Homebrew cask installs the app as built, bundled ffmpeg included, so it depends only on yt-dlp and aria2.

## Validate a Linux development archive

From the repository root, use an isolated Python environment for packaging dependencies:

```sh
python3 -m venv /tmp/course2md-packaging-venv
/tmp/course2md-packaging-venv/bin/python -m pip install --require-hashes -r desktop/scripts/requirements-packaging.txt
export PATH="/tmp/course2md-packaging-venv/bin:$PATH"
python -m unittest discover -s desktop/scripts -p 'test_*.py'
python desktop/scripts/package.py --debug --no-build --require-tools
```

Build both the CLI and desktop debug binaries from the intended source revision before running the last command. `--no-build` skips Rust compilation only: packaging still prepares, copies and checks the external tools. The archive is written to `desktop/target/packages/course2md-desktop-linux-<architecture>.tar.gz`. This command does not publish a release.

Meson and Ninja come from the pinned Python requirements. A fresh ffmpeg build also needs a C compiler, make, NASM on x86-64, pkg-config/pkgconf and GnuTLS development files. On Arch/EndeavourOS these are supplied by `gcc`, `make`, `nasm`, `pkgconf` and `gnutls`; Debian/Ubuntu use `libgnutls28-dev` for the GnuTLS headers. These are build dependencies, not applications end users must install to use the bundled ffmpeg. A verified cached ffmpeg build avoids rebuilding it. Tools extracted into a private prefix can be selected by prepending its `bin` directory to `PATH`.

For a complete bundle, omit all `--no-*` tool options and unset the corresponding `COURSE2MD_NO_YTDLP`, `COURSE2MD_NO_FFMPEG`, `COURSE2MD_NO_UV`, `COURSE2MD_NO_LLAMA`, `COURSE2MD_NO_ARIA2` and `COURSE2MD_NO_APPLE` variables. Any nonempty value, including `0`, enables these environment switches. `--require-tools` fails when an enabled tool cannot be bundled or checked; it does not override an explicit exclusion. Without it, a debug package can fall back to system tools after a bundling failure.

Extract the archive into a fresh directory and run its private engine explicitly:

```sh
./course2md doctor
```

Run this from the extracted directory containing both `course2md` and `course2md-desktop`, then test conversion there. A terminal-installed `course2md` or a binary under `target/debug` checks a different installation. Keep the complete archive tree, including yt-dlp's internal files and llama.cpp's sibling libraries; copying just the two application binaries does not preserve the bundled tools. An explicit `COURSE2MD_FFMPEG`, `COURSE2MD_FFPROBE` or `COURSE2MD_YTDLP` override takes precedence over the bundle, so unset unwanted overrides when validating it.

## Why a complete archive can still report missing components

The archive includes the supported platform's conversion tools, but not every component listed by **Run checks** or `doctor`. Inspect the individual status and tool source rather than treating every missing optional component as a packaging failure.

| Component | Expected source and when it is needed |
| --- | --- |
| ffmpeg, ffprobe, yt-dlp | Included in a complete desktop archive. Missing or unusable copies need investigation; these checks should identify the bundled paths. |
| uv and llama-server | Included on Windows/Linux. Their presence does not install speech models, GPU drivers or the Intel NPU runtime. |
| Speech models | Downloaded separately when configuring local transcription. Missing models do not prevent local video plus supplied subtitles from producing notes. |
| Poppler (`pdftoppm`, `pdftotext`) | Installed separately for conversion with slide PDFs; neither executable is bundled. |
| aria2 | Bundled on Windows only. Optional on Linux/macOS; without it, model downloads use one connection. |
| Apple speech backend, Intel NPU, GPU drivers | Platform/hardware dependent. An unavailable backend does not imply that the other conversion modes are broken. |
| API credentials | User configuration for cloud features, not package contents. Packaging does not validate cloud access. |
| System shared libraries | Supplied by the operating system, including the Linux libraries and Windows runtime described above. A missing library can prevent a bundled executable from running and must be resolved. |

The `doctor` legend distinguishes available, optional/unset, attention and failure statuses. A complete-tool packaging check verifies the bundled executables; it does not guarantee that every optional backend, model or online service is configured. The AUR GUI package and standalone CLI intentionally use system tools, as described above.

## Publishing a version

1. Set the same SemVer version (`2.0.0`, or `2.0.0-rc.N` / `-beta.N` / `-alpha.N`) in `Cargo.toml` and `desktop/Cargo.toml`, update both lock files, and write `docs/releases/v<version>.md`, which becomes the release body.
2. Freeze the GPUI revisions you tested: `python3 desktop/scripts/sources.py --freeze`, then commit `desktop/sources.lock.json`.
3. Push tag `v<version>`, or run the `release` workflow with that version. Running the workflow with `dry_run: true` builds, signs and notarizes every asset on any branch without publishing, for checking packaging changes before a release. It builds every CLI and desktop asset first, then publishes the GitHub Release with `SHA256SUMS`. Prerelease versions are marked as pre-releases and do not replace Latest.
4. `brew-tap` and `aur` run after a successful release. Homebrew updates only the release's channel; AUR skips prereleases and builds and checks the GUI package on Arch before pushing. Each pushes with its own key: `brew-tap` uses the `HOMEBREW_TAP_DEPLOY_KEY` secret (a deploy key with write access to `mizorewww/homebrew-tap` only) and `aur` uses `AUR_SSH_PRIVATE_KEY`.

macOS jobs of `ci`, `native desktop` and `release` run on the maintainer's self-hosted Mac (runner labels `self-hosted, macOS, ARM64, course2md`; installed under `~/actions-runners/course2md` as a launchd service). Pull requests from forks never reach it and use GitHub's `macos-26` instead. Release builds require Xcode's Metal compiler on that Mac and stop otherwise, because without it `build.rs` leaves out the Apple speech module: the runner's `.env` sets `DEVELOPER_DIR` to the installed Xcode, whose Metal Toolchain component comes from `xcodebuild -downloadComponent MetalToolchain`. Desktop jobs there skip `actions/setup-python`, whose macOS builds only install under `/Users/runner`, and create a virtualenv from the Homebrew Python 3.11+ instead. The signing step prepends a temporary keychain to the user's keychain search list and restores the list afterwards.

macOS bundles keep the full SemVer in `Course2mdVersion` and use Apple's numeric form for bundle versions (RC2 → `2.0.0` / `2.0.0fc2`).

Recipes are rendered from `packaging/homebrew/` and `packaging/aur/`. Change these templates, not the external repositories, which later releases overwrite. To retry a channel, run `brew-tap` or `aur` manually with `version`; `aur` accepts `dry_run: true`, and `packaging/aur/update-aur.sh <version> --dry-run` runs the same validation locally on Arch.


User documentation lives in the [GitHub Wiki](https://github.com/mizorewww/course2md/wiki) (`git clone git@github.com:mizorewww/course2md.wiki.git`). Keep the Chinese and English pages aligned.
