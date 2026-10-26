# hindsight

![Windows x64](https://img.shields.io/badge/Windows-x64-0078D4)
![NVIDIA NVENC](https://img.shields.io/badge/NVIDIA-NVENC-76B900)
![BSD-2-Clause license](https://img.shields.io/badge/license-BSD--2--Clause-lightgrey)

<p align="center">
  <img src="assets/logo/monochrome-with-text.png" alt="hindsight" width="520">
</p>

**The lowest-overhead instant replay for Windows: captured and encoded on the GPU, no overlay, no launcher, no bloat.**

- **A tray icon and one small window.** No overlay, no account, no launcher.
- **Cheaper than Instant Replay.** Less CPU and fewer lost frames than NVIDIA's at the same settings, measured in-game.
- **Buffer in RAM, VRAM or on disk**, your choice.
- **H.264, HEVC or AV1** at up to 240 fps, with system sound and, if you like, your microphone as AAC.

<p align="center">
  <img src="assets/screenshots/status.png" alt="hindsight's Status tab while recording cs2 at 240 fps" width="472">
</p>

You need **64-bit Windows 10 version 1903 or newer**, and an **NVIDIA GPU** with a current driver (531 or newer).

## Get started

1. Download **`hindsight-setup-x64.exe`** from the [Releases page](https://github.com/malinoskj2/hindsight/releases) and run it.
2. Launch **hindsight** from the Start menu. It waits for a full-screen app, then records it.
3. Press **Alt+F10** (or the hotkey you set) to save a clip.
4. Optional: read [Getting the best performance](docs/performance.md) for what each setting costs and what to choose for your hardware.

## How it stays light

- **Nothing touches the CPU's memory.** Each frame is copied once, on the GPU, into a texture NVENC owns, and NVENC converts and encodes it on its own silicon. With NvFBC the encoder reads the grab where the driver left it, so even that copy goes.
- **The CPU never reads the video.** NVENC reports each frame's type, so packets go into the buffer without being parsed.
- **Nothing polls.** A waitable timer wakes the capture thread once per frame, in the multimedia scheduling class, and three encode slots rotate so it never waits on the GPU.
- **Nothing runs on the shader cores.** Single-pass rate control, no B-frames, no lookahead, no adaptive quantization. An unchanged screen is repeated GPU-side.
- **Capture gets the GPU first.** High GPU thread and scheduling priority, never realtime, and no power throttling.
- **Sound is kept raw** and mixed and encoded to AAC only when you save, on the saving thread. The microphone is read on the same thread as system sound and kept in mono. Long silences are dropped.
- **Saves run in the background** at low CPU, I/O and memory priority, so they never compete with the game.

Run as administrator for the highest GPU priority. To check for yourself, compare CPU use and frame times (PresentMon or CapFrameX) with hindsight, with Instant Replay and with neither.

## To do

- AMD GPU support
- Linux support

## Contributing

Bug reports and pull requests are welcome, and you do not need to know Rust or GPU programming to help. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[BSD 2-Clause](LICENSE).
