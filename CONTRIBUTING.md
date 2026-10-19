# Contributing to rulogman

Contributions to terminal behavior, SSH and SFTP, local shells, accessibility,
translations, tests and documentation are welcome. Start with the
[README](README.md) and [user guide](docs/user-guide.md) to understand the
behavior the application already promises.

## Reporting a problem or proposing a change

Search the [issues](https://github.com/xcomart/rulogman/issues) before opening
one. For a bug, include the application version or commit, OS version, steps to
reproduce, expected behavior and actual behavior. Terminal reports should also
name the shell, remote OS, terminal program, encoding and font when relevant;
Linux UI reports should say whether the session uses X11 or Wayland.

Use a minimal example. Remove passwords, private keys, host names and private
file contents from logs and screenshots. Discuss substantial UI, transport or
dependency changes in an issue before implementing them.

## Set up a development checkout

Use a recent stable Rust with `rustfmt` and `clippy`, Git, and a native compiler
toolchain: MSVC and the Windows SDK on Windows, Xcode command line tools on
macOS, or a C compiler and development packages on Linux. The workspace uses
Rust edition 2024; CI and the release builds pin Rust 1.98.0, with a
non-blocking clippy job on the latest stable.

On Debian or Ubuntu, install the same native libraries as CI:

```sh
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libfontconfig1-dev
```

From a checkout of `dev`, run:

```sh
rustup component add rustfmt clippy
cargo build --workspace --locked
cargo run -p rulogman-app --locked
```

The first build downloads the pinned Git dependencies and compiles gpui. Running
the app needs a graphical session. Windows release builds also need the SDK's
`fxc.exe`; see the [Windows build notes](README.md#release-builds-on-windows-need-fxcexe)
for `GPUI_FXC_PATH`.

## Where changes belong

| Location | Responsibility |
| --- | --- |
| `crates/rulogman-app` | Workspace, terminal rendering, sessions, files panel and application settings UI |
| `crates/rulogman-core` | Profiles, dashboards, settings, keychain access and known hosts |
| `crates/rulogman-ssh` | SSH authentication, jump hosts, shell channels and SFTP transport |
| `crates/rulogman-pty` | Unix PTYs and Windows ConPTY |
| `crates/rulogman-term` | Terminal model, snapshots, key encoding and character conversion |
| `crates/rulogman-stream` | Bounded output queues, final events and UI event batches |
| `crates/rulogman-app/locales` | Interface translations |
| `crates/rulogman-app/syntaxes` | Shipped editor syntax definitions |

Reusable widgets, the file editor and the application shell live in
[rugpui](https://github.com/xcomart/rugpui). Make shared UI fixes there and follow
its [contribution guide](https://github.com/xcomart/rugpui/blob/dev/CONTRIBUTING.md).
The local Cargo patch workflow is documented there too. Keep every rugpui Git
reference in [Cargo.toml](Cargo.toml) on the same revision, including the four
gpui patches and the `unicode-width` patch. Check the resulting `Cargo.lock`
when updating a dependency; do not commit temporary local path overrides.

## Validate a change

Run the checks used by [CI](.github/workflows/ci.yml) from the repository root:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

For a shorter feedback loop, test the affected layer first:

```sh
cargo test -p rulogman-term --locked
cargo test -p rulogman-stream --locked
cargo test -p rulogman-ssh --test e2e --locked
cargo test -p rulogman-pty --locked
```

The ordinary SSH suite starts its own loopback servers and needs no external
SSH account. PTY tests exercise the local platform. Keychain tests are ignored
because they touch the real OS credential store. The SFTP end-to-end tests in
[`sftp_e2e.rs`](crates/rulogman-app/src/files/sftp_e2e.rs) require Docker, and
the tests in [`wsl_fs.rs`](crates/rulogman-app/src/files/wsl_fs.rs) require a real
WSL distribution. Read their setup instructions and select the relevant tests
explicitly; do not enable all ignored tests as a substitute for setup.

For UI changes, launch the app and check the affected flow with both mouse and
keyboard, light and dark themes, and a non-English locale. Terminal changes
deserve checks for resize, scrollback, wide characters, IME input, disconnects
and sustained output as applicable. Report which platforms you exercised;
cross-platform CI does not replace an interactive check.

## Change guidelines

- Keep network and blocking file work off the UI thread. Preserve the boundary
  between the terminal model and gpui, and between the renderer and SSH.
- Preserve bounded output and reliable disconnect/final events. Add regression
  coverage for changes involving queues, cancellation or session lifetimes.
- Keep russh's default features disabled and its `ring` backend selected, as
  described in the root manifest.
- Keep secrets in the OS credential store and preserve existing profile and
  settings compatibility. Use temporary data and generated keys in tests.
- Add user-visible strings to the locale files and keep translation keys and
  interpolation placeholders aligned across all eight languages. Check long
  labels and keyboard focus in the UI.
- Follow the surrounding Rust style and use `cargo fmt --all` before checking
  formatting. Add a focused regression test for a behavioral fix; documentation
  changes need link and example checks rather than unrelated code changes.
- Update the user guide when shortcuts, settings or visible behavior change.

## Submit your contribution

`dev` is the integration branch. Fork the repository if you do not have write
access, create a focused branch from the latest upstream `dev`, and open a pull
request targeting `dev`. Keep one logical change per contribution.

Maintainers working directly in this workspace review, validate and commit on
`dev`, then push to `origin/dev` with `git push origin dev`.

Use `type: summary` for new commit subjects, with a concise imperative summary.
Choose `feat`, `fix`, `docs`, `refactor`, `test`, `perf`, `build`, `ci` or `chore`
to describe the change, for example `fix: preserve terminal output on disconnect`
or `docs: add contribution guide`. Release and merge commits also use a prefix,
such as `chore: release <version>` or `chore: merge dev into main`. This convention
applies across all five repositories; do not rewrite existing history to rename
older commits.

In a pull request, explain the problem, resulting behavior, checks run and any
checks not run. Include screenshots for visible changes and reproduction steps
for fixes. Review the diff for unrelated formatting, build output and local
configuration before submitting.

CI runs on pull requests and on pushes to `dev` and `main`. Run the local
checks before pushing. Routine work ends at
`origin/dev`; promoting `dev` to `main` and publishing a release are separate
maintainer actions. For a release, merge the `dev` → `main` PR after its required
checks, fetch `origin/main`, and tag its verified latest commit before publishing.
Do not move an existing release tag.

Contributions are covered by the repository's [MIT license](LICENSE). Preserve
third-party notices when incorporating external code.
