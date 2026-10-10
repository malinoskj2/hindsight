# dxgi

![Windows x64](https://img.shields.io/badge/Windows-x64-0078D4)
![NVIDIA NVENC](https://img.shields.io/badge/NVIDIA-NVENC-76B900)

**Instant replay for Windows, captured and encoded on the GPU.**  
dxgi keeps the last stretch of your screen in a rolling buffer and saves it as an MP4 when you press a hotkey, like NVIDIA Instant Replay. Frames never leave video memory before they are encoded: Windows Graphics Capture hands each frame to NVENC on the GPU, and only the compressed video reaches system memory. The aim is the same cost to your game as Instant Replay: well under 1% of one CPU core and no measurable frame-rate loss.

You need **64-bit Windows 10 version 1903 or newer**, and an **NVIDIA GPU** with a current driver (531 or newer).

## Get started

1. Download **`dxgi-setup-x64.exe`** from the Releases page and run it.
2. Launch **dxgi** from the Start menu. It starts recording the primary monitor straight away.
3. Press **Alt+F10** (or the hotkey you set) to save the last 30 seconds. Clips go to `Videos\dxgi` as `dxgi_<date>_<time>.mp4`.

## Settings

| Setting | Default | Range |
| --- | --- | --- |
| Codec | H.264 | H.264, HEVC, AV1 (AV1 needs an RTX 40 series GPU or newer) |
| Frame rate | 60 fps | 24–240 fps |
| Buffer | 30 s | 5–600 s |
| Bitrate | 50 Mbps | 5–150 Mbps |
| Save hotkey | Alt+F10 | Ctrl, Alt, Shift or Win with a key; F-keys may stand alone |
| Clip folder | `Videos\dxgi` | Any absolute folder |

The buffer is kept in RAM: about 190 MB for 30 seconds at 50 Mbps, 1.9 GB for 5 minutes. The Settings tab shows the estimate for what you enter. **Apply and restart** starts a new buffer, so save anything you want first.

## While you play

- The **Status** tab shows what is being recorded, the frame rate (frames encoded, and how many of them brought new pictures), how much is buffered, and the last saved clip.
- The **Logs** tab lists what the app did and any errors. The same lines are written to `%LOCALAPPDATA%\Dxgi\dxgi.log`, replaced on each launch; attach it to a bug report.
- **Close the window** to keep recording from the tray. Click the tray icon to reopen it; right-click it for **Save replay** and **Quit**.
- **Start with Windows** (Settings tab) starts the app hidden in the tray at sign-in.
- Saving never pauses recording: the clip is written from a snapshot of the buffer while capture carries on.

## How it stays light

- **Capture and encode never touch the CPU's memory.** Each new frame is copied once, GPU-side, into an input texture NVENC has registered; NVENC converts it to YUV and encodes it on its own silicon, which games do not use.
- **Encoding is asynchronous.** Three input slots rotate: while NVENC works on one frame the capture thread prepares the next and collects finished ones, so it never waits on the GPU unless all three are still in flight.
- **Nothing polls.** A high-resolution waitable timer wakes the capture thread once per frame; it sleeps in the kernel in between. Windows is asked not to deliver frames faster than that.
- **An unchanged screen costs almost nothing**: the last picture is repeated GPU-side, keeping the output at a constant frame rate.
- **Single-pass rate control, no B-frames, no lookahead**: each frame leaves the encoder as soon as it is done, and NVENC does the least work per frame.
- **The GPU scheduler favours the copies.** The capture device raises its GPU thread priority and the process its GPU scheduling class (realtime when run as administrator, high otherwise), so a game holding the GPU at 100% does not starve capture. Windows power throttling is turned off for the process.
- **One keyframe per second**, with the sequence header on every keyframe, so a clip can start at any second and the buffer trims to within a second of its length.

Run as administrator for the highest GPU priority. To check parity yourself, compare CPU use and frame times (PresentMon or CapFrameX) with dxgi, with Instant Replay and with neither; PresentMon's present mode should stay *Hardware: Independent Flip*.

## Quick fixes

- **"this GPU's NVENC has no AV1 encoder"**: choose H.264 or HEVC in Settings.
- **"the NVIDIA driver supports NVENC API …"**: update the NVIDIA driver.
- **"the display is … larger than … this GPU's H.264 encoder accepts"**: H.264 stops at 4096 pixels across; choose HEVC or AV1 for super-ultrawide and 8K displays.
- **The monitor was unplugged or the driver reset**: capture starts again by itself within ten seconds.
- **The hotkey does nothing**: another application holds it; the Status tab says so. Choose another in Settings.
- **The display changed resolution** (a game switched modes): capture restarts by itself and the buffer starts over.
- On Windows 10 a yellow border marks the captured screen; Windows 11 lets dxgi turn it off.

HDR displays are captured as SDR for now, and audio is not recorded yet.

## Build from source

```powershell
cargo build --release
./scripts/build-windows.ps1   # dist\dxgi.exe and, with NSIS installed, the installer
```

The capture and encoding code is Windows-only; everything else (the replay buffer, the MP4 writer for all three codecs, settings, hotkeys) builds and tests on any platform with `cargo test`. The MP4 writer is tested against real H.264, HEVC and AV1 encoder output in `tests/fixtures`.
