# Contributing

Thanks for your interest. Bug reports, ideas and pull requests are all welcome.

**You do not need to know Rust or GPU programming to contribute.** If you can describe a
problem or a fix, send it in whatever state it is in. We will help you shape, optimise and
clean up the code before it merges, and we are happy to explain anything along the way.

## Reporting a bug

Open an issue with:

- What you expected and what happened.
- Your GPU, NVIDIA driver version and Windows version (the About tab lists all three).
- The log: `%LOCALAPPDATA%\Hindsight\hindsight.log`, or the Logs tab.
- The game or app being recorded, if it only happens with one.

## Building

You need 64-bit Windows, the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`)
and the Windows SDK, which Visual Studio's "Desktop development with C++" workload installs.

```
cargo build
cargo test
```

The installer needs [NSIS](https://nsis.sourceforge.io/); `scripts\build-windows.ps1` builds
the release and the installer together.

The capture, encoding and GUI only compile on Windows, but the rest (the replay buffer, the
MP4 writer, the worker and the settings) builds and tests on Linux and macOS too, so a lot of
the project can be worked on without an NVIDIA GPU.

## Before you open a pull request

CI runs these, so running them first saves a round trip:

```
cargo fmt
cargo clippy --all-targets
cargo test
```

Clippy runs with the `pedantic` group on. If a lint fires and you think it is wrong for this
case, allow it with `#[expect(..., reason = "...")]` and say why.

A few conventions, none of which you need to get right on the first try:

- **Only commit code you understand.** If a tool, a snippet or an AI wrote something and you
  cannot say what each line does, ask before committing it. We would rather explain it with
  you than review it blind.
- **Unsafe code** is denied by default and enabled per module with
  `#[expect(unsafe_code, reason = "...")]`. Every `unsafe` block has a `// SAFETY:` comment
  saying why it is sound. Keep unsafe at the driver and OS boundary; the code above it should
  be safe Rust.
- **Comments** are plain technical English. Say what a thing is or does, in full sentences.
- **No new dependencies** without a note in the pull request on why the crate is needed.
- **Performance** is the point of the project. If a change touches the capture or encode path,
  say in the pull request how you checked it did not cost the game anything.
  [docs/performance.md](docs/performance.md) explains what each part costs today.
- Keep commits focused. One change per commit, with a subject that says what it does.

Small pull requests get reviewed faster. If you are planning something larger, open an issue
first so we can agree on the approach before you spend time on it.

## Licence

Contributions are licensed under the project's [BSD 2-Clause licence](LICENSE).
