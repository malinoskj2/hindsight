# Getting the best performance

hindsight is built to cost a game as little as possible, but every setting moves that cost a
little. This page says what each setting costs, and what to choose for your hardware. The
numbers come from measurements on a Ryzen 9 9950X3D with an RTX 5090, running Counter-Strike 2
at about 690 fps, where the game is limited by the CPU (see [the day run](day-run.md) for the
method and the raw data). On a slower machine the costs are similar in absolute terms and
therefore larger as a share.

## The short version

1. If you have an NVIDIA key for NvFBC, use **NVIDIA NvFBC**. Otherwise keep **Windows Graphics
   Capture**.
2. Record at the frame rate you need for the clips you keep, not the game's. 60 fps costs about
   a third of what 240 fps does.
3. Use **AV1** on an RTX 40 series card or newer, HEVC on older ones, H.264 only when you need the
   clips to play everywhere.
4. Keep the buffer in **RAM** unless you are short of it; then use **VRAM** if your card has room
   to spare, or **Disk** on an SSD (it writes the bitrate continuously, so mind the drive's endurance).
5. Turn **Record system sound** off if you never use the sound.

## What each setting costs

Costs are against no recorder, cs2 at 690 fps, unless the row says otherwise.

| Setting | Choice | What it costs | When to choose it |
|---|---|---|---|
| Backend | NVIDIA NvFBC | 2.3% of the game's frame rate and about 5.5% of one CPU core at AV1 240 fps | You have the key; it costs the game least, about a fifth less than NVIDIA Instant Replay and a third less CPU |
| | Windows Graphics Capture | about twice NvFBC's cost to the game at 240 fps, mostly in the desktop compositor | No NvFBC key; laptops whose display hangs off the integrated GPU; recording one window |
| | DXGI Desktop Duplication | the most: it copies every frame and keeps the compositor busy | Only when the other two fail |
| | NVIDIA NvFBC to CUDA | more CPU than NvFBC (twice) and a little more of the game | Not recommended; kept for experiments |
| Frame rate | 240 fps | the cost above | Fast games whose clips you slow down |
| | 60 fps | 0.7 to 1% of the game, a third of the CPU | Everything else: the cost scales with frames captured |
| Codec | AV1 | the same CPU as the others at the same rate | RTX 40 series and newer: best picture per bit |
| | HEVC | the same | RTX 20/30 series |
| | H.264 | the same | When clips must play on anything; needs the most bitrate for the same picture |
| Bitrate | any | no measurable CPU or game cost; it sets the buffer's size and the clips' size | As high as your storage allows for the picture you want |
| Buffer length | any | none while recording; it sets the buffer's memory | As long as you need |
| Buffer in | RAM (default) | none | Unless you are short of RAM |
| | VRAM | 0.4% of one core more, the game nothing; takes the buffer's size of the card's memory | Plenty of VRAM to spare and RAM to save |
| | Disk | the same CPU and game cost as RAM; writes the bitrate to the drive continuously (45 GB an hour at 100 Mb/s); hindsight's RAM stays near 120 MB | Short of RAM and of VRAM, with an SSD that can take the writes |
| Record system sound | on | under 1% of one core: the sound is kept raw and encoded only when a clip is saved; it takes 57 MB of RAM for 5 minutes | Off if you never use the sound |
| Record the mouse cursor | either | no measurable difference | As you like |

Buffer sizes: bitrate × length. At 100 Mb/s, 300 s is about 3.75 GB; at 50 Mb/s, 30 s is about
190 MB. The Settings tab shows it for what you enter.

## Your hardware

- **CPU-bound games (high frame rates, esports).** Every frame captured is driver work on the CPU,
  so the frame rate you record at matters most. 60 fps costs the game about a third of 240.
- **GPU-bound games (high settings, low frame rates).** hindsight's GPU work is small (one copy and
  one encode per frame on separate engines); the game loses little.
- **AMD X3D processors (two core complexes, one with the large cache).** Nothing to do: keeping
  hindsight off the game's cache die was measured and made no difference.
- **8 to 16 GB of RAM.** Use a shorter buffer or a lower bitrate, or move the buffer to VRAM or
  Disk, so the game keeps its memory.
- **Cards with 8 GB of VRAM or less.** Keep the buffer in RAM: a buffer in VRAM pushes the game's
  textures out.
- **Laptops with an integrated GPU driving the display.** Use Windows Graphics Capture; NvFBC and
  DXGI only see monitors attached to the NVIDIA GPU.
- **HDR displays.** Use Windows Graphics Capture: it is built to tone-map HDR to the clip's SDR
  (not yet checked on a real HDR display). NvFBC clips the highlights.
- **Hardware-accelerated GPU scheduling** and Windows' Game Mode can stay as they are; neither was
  found to change hindsight's cost.

## What hindsight already does for you

The capture thread runs in Windows' multimedia scheduling class so it is never late; frames
never leave video memory before they are encoded; NVENC reads the captured surfaces in place;
the encoder uses no extras that would run on the GPU's shader cores (no lookahead, no
B-frames, no weighted prediction); a clip is written in the background at low I/O priority.
Nothing runs at all while no full-screen app is in front.
