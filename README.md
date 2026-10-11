# hindsight

![Windows x64](https://img.shields.io/badge/Windows-x64-0078D4)
![NVIDIA NVENC](https://img.shields.io/badge/NVIDIA-NVENC-76B900)
![BSD-2-Clause license](https://img.shields.io/badge/license-BSD--2--Clause-555555)

**Instant replay for Windows, captured and encoded on the GPU.**  
hindsight keeps the last stretch of whatever full-screen app is in front in a rolling buffer and saves it as an MP4 when you press a hotkey, like NVIDIA Instant Replay. With no full-screen app running it does nothing at all. Frames never leave video memory before they are encoded: Windows Graphics Capture hands each frame to NVENC on the GPU, and only the compressed video reaches system memory. The aim is the same cost to your game as Instant Replay: well under 1% of one CPU core and no measurable frame-rate loss.

You need **64-bit Windows 10 version 1903 or newer**, and an **NVIDIA GPU** with a current driver (531 or newer).

## Get started

1. Download **`hindsight-setup-x64.exe`** from the Releases page and run it.
2. Launch **hindsight** from the Start menu. It waits for a full-screen app, then records it.
3. Press **Alt+F10** (or the hotkey you set) to save the last 30 seconds. Clips go to `Videos\hindsight` as `hindsight_<date>_<time>.mp4`.

## What gets recorded

By default hindsight records **full-screen apps**: whatever app is in front with a window that fills its monitor, which is how games present whether in exclusive or borderless full screen. A maximized window doesn't count, since the taskbar still shows. Recording starts when such an app comes to the front and follows it if you alt-tab away; only the app's own window goes into the buffer, so nothing you switch to appears in a clip. When the app exits, its last 30 seconds stay saveable until the next one starts, so a crash to desktop can still be clipped. Between apps nothing runs: no capture, no encoder, no timer.

The other mode, **Whole desktop, always**, records the primary monitor from the moment hindsight starts, everything on it included.

**Sound** is recorded too: whatever the default output device plays (the game, voice chat, music), as Windows mixes it, captured with WASAPI loopback and stored as AAC at 160 kb/s beside the video, starting and ending with it. If you switch output devices or unplug a headset, recording follows the new default device as soon as Windows reports the change and the gap is silent. The microphone isn't recorded. Turn off **Record system sound** in Settings for silent clips.

## Capture backends

The **Capture** setting chooses how frames are taken. All three keep every frame on the GPU.

- **Windows Graphics Capture** (the default) can record just the app's window, includes the mouse cursor, and works on laptops whose display is connected to the integrated GPU.
- **DXGI Desktop Duplication** is the older, lower-level API and does a little less work per frame. It sees whole monitors only, so a full-screen app is recorded through its monitor: anything you alt-tab to while the app is still running shows up in the clip. The cursor isn't drawn. It only works for monitors connected to the NVIDIA GPU, and not for rotated monitors. On either of those it stops with an error that tells you to switch back. A UAC prompt or the lock screen pauses it without dropping the buffer.
- **NVIDIA NvFBC** (for licensed vendors) is the frame-buffer capture NVIDIA Instant Replay itself uses: the driver copies the display, cursor included, straight from the GPU. It is only for vendors NVIDIA has licensed: choosing it asks for the key NVIDIA issued you (in hexadecimal), which is saved for your Windows account in `HKEY_CURRENT_USER\Software\hindsight` and never in the settings file. Choosing it again lets you change the key; cancelling keeps the previous capture setting. Like Desktop Duplication it records whole monitors connected to the NVIDIA GPU, and not rotated ones. If NvFBC is turned off in the driver, run hindsight once as administrator to turn it on. A mode change or the secure desktop pauses it without dropping the buffer.

## Settings

| Setting | Default | Range |
| --- | --- | --- |
| Record | Full-screen apps | Full-screen apps, or the whole desktop always |
| Backend | Windows Graphics Capture | Windows Graphics Capture, DXGI Desktop Duplication, or NVIDIA NvFBC |
| Codec | H.264 | H.264, HEVC, AV1 (AV1 needs an RTX 40 series GPU or newer) |
| Frame rate | 60 fps | 24–240 fps |
| Buffer | 30 s | 5–600 s |
| Bitrate | 50 Mbps | 5–150 Mbps |
| Save hotkey | Alt+F10 | Ctrl, Alt, Shift or Win with a key; F-keys may stand alone |
| Clip folder | `Videos\hindsight` | Any absolute folder |
| Theme | Classic Windows | Classic Windows, Ember, Selene or Nova |
| Record system sound | On | On or off |
| Record the mouse cursor | Off | On or off; NvFBC and Windows Graphics Capture draw it, DXGI Desktop Duplication never does |

**Theme** (in the General group) dresses the window in one of three characters: their colours, rounded group panels, tab strip and buttons, and an illustration beside the controls. It applies the moment you choose it, without restarting recording, and is remembered. Classic Windows, the default, is the plain dialog of Windows' own controls, unchanged. The artwork is in `assets/themes`.

The buffer is kept in RAM: about 190 MB for 30 seconds at 50 Mbps, 1.9 GB for 5 minutes. The Settings tab shows the estimate for what you enter. **Apply** starts a new buffer, so save anything you want first.

## While you play

- The **Status** tab shows which app or monitor is being recorded, the frame rate (frames encoded, and how many of them brought new pictures), how much is buffered, and the last saved clip.
- The **Logs** tab lists what the app did and any errors. The same lines are written to `%LOCALAPPDATA%\Hindsight\hindsight.log`, replaced on each launch; attach it to a bug report.
- **Close the window** to keep recording from the tray. Click the tray icon to reopen it; right-click it for **Save replay** and **Quit**.
- **Only one copy runs.** Starting hindsight again, from the Start menu, a shortcut or a terminal, just brings the running window forward, even if it is in the tray or minimized.
- **Start with Windows** (Settings tab) starts the app hidden in the tray at sign-in, ready for the first full-screen app.
- **Stop recording** pauses everything, including noticing full-screen apps, until **Start recording** or **Apply**.
- Saving never pauses recording: the clip is written from a snapshot of the buffer while capture carries on.

## How it stays light

- **Capture and encode never touch the CPU's memory.** Each new frame is copied once, GPU-side, into an input texture NVENC has registered; NVENC converts it to YUV and encodes it on its own silicon, which games do not use.
- **Encoding is asynchronous.** Three input slots rotate: while NVENC works on one frame the capture thread prepares the next and collects finished ones, so it never waits on the GPU unless all three are still in flight.
- **Nothing polls.** A high-resolution waitable timer wakes the capture thread once per frame; it sleeps in the kernel in between. Windows is asked not to deliver frames much faster than that.
- **The capture thread runs in the multimedia scheduling class** ("Capture", via MMCSS), the real-time range Windows reserves for audio and capture, so a game's threads never delay its wake-up. Clips are written by a thread in background mode, with low CPU, I/O and memory priority, so a save never competes with the game for the disk.
- **The CPU never reads the video.** NVENC reports each frame's type, so the compressed packets are copied into the buffer without being parsed (a parse would cost about 100 µs per frame, most of the budget).
- **An unchanged screen costs almost nothing**: the last picture is repeated GPU-side, keeping the output at a constant frame rate.
- **Single-pass rate control, no B-frames, no lookahead, no temporal AQ, no weighted prediction**: each frame leaves the encoder as soon as it is done, and nothing runs on the GPU's shader cores, which the game needs. Only the RGB-to-YUV conversion inside the driver does, a fraction of a millisecond per frame.
- **The GPU scheduler favours the copies.** The capture device raises its GPU thread priority and the process its GPU scheduling class (high, never realtime, which could starve the game of the GPU), so a game holding the GPU at 100% does not starve capture. Windows power throttling is turned off for the process.
- **One keyframe every 30 frames**, as NVIDIA Instant Replay does, with the sequence header on every keyframe, so a clip can start at any of them and the buffer trims to within 30 frames of its length.
- **The encoder is set up like Instant Replay's.** Preset P4 with high-quality tuning and no B-frames, single-pass VBR peaking at twice the bitrate you choose, and none of the rate-control extras (lookahead, adaptive quantization) that would cost the game GPU time. HEVC and AV1 stay within 16×16 to 32×32 blocks.
- **Sound is the one stream the CPU encodes, and it is small.** The loopback capture's thread wakes every 30 ms and reads the few packets the audio engine has gathered, and Media Foundation's AAC encoder, the one Windows ships, turns it into 47 frames a second: about 1% of one core altogether (0.9% in hindsight and 0.2% in the Windows audio engine, measured with a game playing; see [the benchmarks](docs/benchmarks.md)), and 20 kB a second, under 1 MB for 30 seconds, in the buffer. Long silences are not encoded at all. With **Record system sound** off none of this runs.

Run as administrator for the highest GPU priority. To check parity yourself, compare CPU use and frame times (PresentMon or CapFrameX) with hindsight, with Instant Replay and with neither; PresentMon's present mode should stay *Hardware: Independent Flip*.

## Quick fixes

- **"this GPU's NVENC has no AV1 encoder"**: choose H.264 or HEVC in Settings.
- **"the NVIDIA driver supports NVENC API …"**: update the NVIDIA driver.
- **"the display is … larger than … this GPU's H.264 encoder accepts"**: H.264 stops at 4096 pixels across; choose HEVC or AV1 for super-ultrawide and 8K displays.
- **The monitor was unplugged or the driver reset**: capture starts again by itself within ten seconds.
- **The hotkey does nothing**: another application holds it; the Status tab says so. Choose another in Settings.
- **The display or the app's window changed size** (a game switched modes): capture restarts by itself and the buffer starts over.
- **An app in exclusive full screen** can't be captured as a window; hindsight notices within two seconds and captures its monitor instead.
- On Windows 10 a yellow border marks the captured screen; Windows 11 lets hindsight turn it off.
- **Clips have no sound**: the Logs tab says why. Without any output device the sound track is silent until one is connected; on Windows N editions install the Media Feature Pack, which brings the AAC encoder.

HDR displays are captured as SDR for now.

## Build from source

```powershell
cargo build --release
./scripts/build-windows.ps1   # dist\hindsight.exe and, with NSIS installed, the installer
```

The capture and encoding code is Windows-only; everything else (the replay buffers, the MP4 writer for all three codecs and the sound track, settings, hotkeys) builds and tests on any platform with `cargo test`. The MP4 writer is tested against real H.264, HEVC, AV1 and AAC encoder output in `tests/fixtures`.

## License

[BSD 2-Clause](LICENSE).
