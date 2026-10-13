# hindsight

![Windows x64](https://img.shields.io/badge/Windows-x64-0078D4)
![NVIDIA NVENC](https://img.shields.io/badge/NVIDIA-NVENC-76B900)
![BSD-2-Clause license](https://img.shields.io/badge/license-BSD--2--Clause-555555)

<p align="center">
  <img src="assets/logo/monochrome-with-text.png" alt="hindsight" width="520">
</p>

**The lowest-overhead instant replay for Windows: captured and encoded on the GPU, no overlay, no launcher, no bloat.**

- **A tray icon and one small window.** No overlay, no account, no launcher.
- **Cheaper than Instant Replay.** Less CPU and fewer lost frames than NVIDIA's at the same settings, measured in-game.
- **Buffer in RAM, VRAM or on disk**, your choice.
- **H.264, HEVC or AV1** at up to 240 fps, with system sound as AAC.

<p align="center">
  <img src="assets/screenshots/status.png" alt="hindsight's Status tab while recording cs2 at 240 fps" width="470">
</p>

You need **64-bit Windows 10 version 1903 or newer**, and an **NVIDIA GPU** with a current driver (531 or newer).

## Get started

1. Download **`hindsight-setup-x64.exe`** from the Releases page and run it.
2. Launch **hindsight** from the Start menu. It waits for a full-screen app, then records it.
3. Press **Alt+F10** (or the hotkey you set) to save a clip.
4. Optional: read [Getting the best performance](docs/performance.md) for what each setting costs and what to choose for your hardware.

## How it stays light

- **Capture and encode never touch the CPU's memory.** Each new frame is copied once, GPU-side, into an input texture NVENC has registered; NVENC converts it to YUV and encodes it on its own silicon, which games do not use.
- **With NvFBC, the encoder runs on the capture's own device.** NVENC opens on the Direct3D 9 device NvFBC grabs into and reads each grab where the driver left it, so nothing is copied, the GPU carries the grab out before the encode on its own, and the CPU never waits for a grab to land. Recording AV1 at 1080p and 240 fps costs about 261 Mcycles/s (about 5.5% of one core) without sound.
- **Encoding is asynchronous.** Three input slots rotate: while NVENC works on one frame the capture thread prepares the next and collects finished ones, so it never waits on the GPU unless all three are still in flight.
- **Nothing polls.** A high-resolution waitable timer wakes the capture thread once per frame; it sleeps in the kernel in between. Windows is asked not to deliver frames much faster than that.
- **The capture thread runs in the multimedia scheduling class** ("Capture", via MMCSS), the real-time range Windows reserves for audio and capture, so a game's threads never delay its wake-up. Clips are written by a thread in background mode, with low CPU, I/O and memory priority, so a save never competes with the game for the disk.
- **The CPU never reads the video.** NVENC reports each frame's type, so the compressed packets are copied into the buffer without being parsed (a parse would cost about 100 µs per frame, most of the budget).
- **An unchanged screen costs almost nothing**: the last picture is repeated GPU-side, keeping the output at a constant frame rate.
- **Single-pass rate control, no B-frames, no lookahead, no temporal AQ, no weighted prediction**: each frame leaves the encoder as soon as it is done, and nothing runs on the GPU's shader cores, which the game needs. Only the RGB-to-YUV conversion inside the driver does, a fraction of a millisecond per frame.
- **The GPU scheduler favours the copies.** The capture device raises its GPU thread priority and the process its GPU scheduling class (high, never realtime, which could starve the game of the GPU), so a game holding the GPU at 100% does not starve capture. Windows power throttling is turned off for the process.
- **Two keyframes a second** at any frame rate, with the sequence header on every keyframe, so a clip can start at any of them and the buffer trims to within half a second of its length. The rest of the bitrate goes to the P-frames.
- **The encoder is set up like Instant Replay's.** Preset P4 with high-quality tuning and no B-frames, single-pass VBR peaking at twice the bitrate you choose, and none of the rate-control extras (lookahead, adaptive quantization) that would cost the game GPU time. HEVC and AV1 stay within 16×16 to 32×32 blocks.
- **Sound is the one stream the CPU encodes, and only when you save.** The loopback capture's thread wakes every 60 ms and reads the few packets the audio engine has gathered, on a core away from the capture thread's, and keeps them as 16-bit PCM. A save hands the clip's span of it to Media Foundation's AAC encoder, the one Windows ships, on the saving thread: it encodes about 300 times faster than real time, so 5 minutes of sound add about a second to the save. Encoding as the sound arrived cost about 1% of one core while recording (0.9% in hindsight, measured with a game playing), almost all of it the encoder; keeping PCM instead costs 192 kB of RAM a second rather than 24 kB. Long silences are not kept at all. With **Record system sound** off none of this runs.

Run as administrator for the highest GPU priority. To check parity yourself, compare CPU use and frame times (PresentMon or CapFrameX) with hindsight, with Instant Replay and with neither; PresentMon's present mode should stay *Hardware: Independent Flip*.

## Quick fixes

- **An error names the encoder**: your GPU's NVENC lacks that codec or the driver is old; choose another codec in Settings or update the NVIDIA driver.
- **The hotkey does nothing**: another application holds it; choose another in Settings.
- **Clips have no sound**: the Logs tab says why.

Display changes, driver resets and mode switches recover by themselves within seconds. HDR displays are captured as SDR for now.

## Build from source

```powershell
cargo build --release
./scripts/build-windows.ps1   # dist\hindsight.exe and, with NSIS installed, the installer
```

The capture and encoding code is Windows-only; everything else (the replay buffers, the MP4 writer for all three codecs and the sound track, settings, hotkeys) builds and tests on any platform with `cargo test`. The MP4 writer is tested against real H.264, HEVC, AV1 and AAC encoder output in `tests/fixtures`.

## License

[BSD 2-Clause](LICENSE).
