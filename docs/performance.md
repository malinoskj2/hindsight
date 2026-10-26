# Getting the best performance

hindsight is built to cost a game as little as possible, but every setting moves that cost a
little. This page says what each setting costs and how to choose for your hardware. The costs
are given relative to one another and to NVIDIA Instant Replay at the same settings, measured
in a CPU-bound game on a scene played the same way each time; the ratios carry across machines
better than any absolute figure would.

## The short version

1. Keep the **NVIDIA NvFBC** backend, the default. It costs the game least.
2. Record at the frame rate you need for the clips you keep, not the game's. 60 fps costs about
   a third of what 240 fps does.
3. Use **AV1** if it is offered (RTX 40 series or newer), HEVC otherwise, H.264 only when clips
   must play everywhere. They all cost the same CPU; they differ in picture per bit.
4. Keep the buffer in **RAM** if the machine has the room; otherwise **VRAM** if the card has the
   room, otherwise **Disk** on an SSD.
5. Turn **Record system sound** off if you never use the sound, and leave **Record the microphone**
   off unless you want your voice in the clips.

## Recording and encoding: quality against cost

These settings decide what the clips look like and how big they are. Only one of them, the
frame rate, costs the game anything to speak of.

**Frame rate** is the one lever that matters. Every frame captured is driver work on the CPU and
an encode on the GPU, so the cost scales with frames per second, whatever the codec or bitrate.
Measured with NvFBC at 1080p, 240 fps costs the game about a fifth less than NVIDIA Instant
Replay at the same settings and about a third less CPU; 60 fps costs about a third of what
240 fps does, in both the game's frame rate and CPU.

Record at 240 fps only for fast games whose clips you slow down. For everything else 60 fps gives
the same clip at a third of the cost.

**Codec** does not change the CPU or game cost at a given frame rate: the encoder is NVENC and
the choice only changes which of its engines runs. It changes picture per bit, so it decides
what bitrate you need and how large the buffer and clips are. The Settings tab offers only the
codecs your GPU's encoder supports.

| Codec | Picture per bit | Plays on | Cards |
|---|---|---|---|
| AV1 | best; about half the bitrate of H.264 for the same picture | recent players, browsers and phones | RTX 40 series and newer |
| HEVC | good; about two thirds of H.264's bitrate | most players; some browsers need a system codec | RTX 20 series and newer |
| H.264 | needs the most bitrate | everything | all |

**Bitrate** costs nothing measurable in CPU or game frame rate. It sets the picture quality, the
buffer's size and the clips' size. Pick the lowest that looks right for the codec: with AV1,
25 Mb/s at 1080p 60 fps and 50 Mb/s at 240 fps hold up well; HEVC wants about half as much again;
H.264 about double. The default of 50 Mb/s is generous for 1080p 60 fps with any codec.

**Buffer length** costs nothing while recording. It only sets how much memory the buffer takes:
bitrate × length. At 100 Mb/s, 300 s is about 3.75 GB; at 50 Mb/s, 30 s is about 190 MB. The
Settings tab shows the figure for what you enter.

**Record system sound** costs less than a fifth of the video capture's CPU while recording: the
sound is kept raw and encoded to AAC only when a clip is saved. It takes about 57 MB of RAM for
5 minutes, always in RAM whichever memory holds the video. Turn it off if you never use the sound.

**Record the microphone** records the default recording device, read on the same thread as the
system sound and kept raw the same way, in mono: about 29 MB of RAM for 5 minutes. It is mixed into
the clip's one sound track when the clip is saved, so recording it adds no encoding while you play.

**Record the mouse cursor** makes no measurable difference either way.

## Backend and buffer location: the least CPU for your hardware

These settings do not change the clips. They change where the work happens and where the
buffer lives, which is what decides the cost on a given machine.

### Backend

| Backend | Cost | Choose it when |
|---|---|---|
| NVIDIA NvFBC (default) | the least: about a fifth less of the game than NVIDIA Instant Replay at AV1 240 fps, and a third less CPU | your monitor is connected to the NVIDIA GPU, which is almost always |
| Windows Graphics Capture | about twice NvFBC's cost to the game at 240 fps, most of it in the desktop compositor rather than in hindsight | a laptop whose display hangs off the integrated GPU, an HDR display, or when NvFBC refuses the display (rotated, or of odd width or height) |
| DXGI Desktop Duplication | the most: it copies every frame and keeps the compositor busy | only when the other two fail |

### Buffer location

The buffer is the whole recorded stream for the buffer length at your bitrate (the size above).
It has to live somewhere, and the right place depends on what the game needs of the same memory.

| Buffer in | Cost | Takes |
|---|---|---|
| RAM (default) | none | the buffer's size in system memory |
| VRAM | a little more CPU than RAM, under a tenth of the capture's own; the game nothing | the buffer's size in the card's memory, held for as long as recording runs |
| Disk | the same CPU and game cost as RAM | about 120 MB of RAM and a file of the buffer's size on the drive; writes the bitrate continuously, 45 GB an hour at 100 Mb/s |

Choose by what is left over after the game:

- **RAM** when the machine has the buffer's size free with the game running. Games of this
  generation want 8 to 16 GB of their own, so: with 32 GB or more, any buffer; with 16 GB, keep the
  buffer under about 2 GB (for instance 60 s at 100 Mb/s, or 5 minutes at 50 Mb/s) or move it; with
  8 GB, move it.
- **VRAM** when RAM is short and the card has the buffer's size spare on top of what the game uses.
  A 16 GB or 24 GB card running a 1080p game has room for several gigabytes; a card with 8 GB or
  less does not, and a buffer there pushes the game's textures out, which costs far more than the
  buffer saves.
- **Disk** when both are short. It leaves RAM and VRAM to the game at the price of the drive's
  endurance: an SSD rated for a few hundred terabytes written takes years of daily sessions, a
  cheap one less, and a hard drive may fall behind (the buffer then goes back to RAM, and the log
  says so). It needs the buffer's size plus 1 GB free on the system drive.

When the buffer cannot be reserved where you asked, hindsight falls back to RAM and logs why.

### By machine

- **Desktop, 32 GB RAM, 12 GB or more of VRAM.** The defaults: NvFBC, buffer in RAM, as long a
  buffer as you like.
- **Desktop, 16 GB RAM.** NvFBC, buffer in RAM up to about 2 GB; longer buffers in VRAM if the
  card has 12 GB or more, else Disk.
- **Desktop, 8 GB RAM.** NvFBC, buffer in VRAM if the card has 12 GB or more, else Disk on an SSD.
- **Laptop with the display on the integrated GPU.** Windows Graphics Capture; NvFBC and DXGI only
  see monitors attached to the NVIDIA GPU. Laptops are usually short of both RAM and VRAM, so a
  short buffer at 60 fps, or Disk.
- **CPU-bound games (high frame rates, esports).** The frame rate you record at matters most; 60
  fps costs the game about a third of 240.
- **GPU-bound games (high settings, low frame rates).** hindsight's GPU work is small (one copy and
  one encode per frame on separate engines); the game loses little, and VRAM is the memory to
  protect.
- **AMD X3D processors.** Nothing to do: keeping hindsight off the game's cache die was measured and
  made no difference.
- **HDR displays.** Windows Graphics Capture tone-maps HDR to the clip's SDR (not yet checked on a
  real HDR display); NvFBC clips the highlights.
- **Hardware-accelerated GPU scheduling** and Windows' Game Mode can stay as they are; neither was
  found to change hindsight's cost.

## What hindsight already does for you

The capture thread runs in Windows' multimedia scheduling class so it is never late; frames
never leave video memory before they are encoded; NVENC reads the captured surfaces in place;
the encoder uses no extras that would run on the GPU's shader cores (no lookahead, no
B-frames, no weighted prediction); a clip is written in the background at low I/O priority.
Nothing runs at all while no full-screen app is in front.
