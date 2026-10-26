export interface Point {
  readonly title: string;
  readonly body: string;
}

export const requirements = [
  "64-bit Windows 10 version 1903 or newer",
  "An NVIDIA GPU with driver 531 or newer",
] as const satisfies readonly string[];

export const features = [
  {
    title: "A tray icon and one small window.",
    body: "No overlay, no account, no launcher. It waits for a full-screen game and records it.",
  },
  {
    title: "Cheaper than Instant Replay.",
    body: "Less CPU and fewer lost frames than NVIDIA's at the same settings, measured in-game.",
  },
  {
    title: "Keep 5 seconds to 10 minutes.",
    body: "Hold the recent gameplay in RAM, VRAM or on disk, wherever your machine has room to spare.",
  },
  {
    title: "H.264, HEVC or AV1 at up to 240 fps.",
    body: "With system sound as AAC. The settings only offer what your GPU can encode.",
  },
] as const satisfies readonly Point[];

export const internals = [
  {
    title: "Nothing touches the CPU's memory.",
    body: "Each frame is copied once, on the GPU, into a texture NVENC owns, and NVENC converts and encodes it on its own silicon. With NvFBC the encoder reads the grab where the driver left it, so even that copy goes.",
  },
  {
    title: "The CPU never reads the video.",
    body: "NVENC reports each frame's type, so packets go into the buffer without being parsed.",
  },
  {
    title: "Nothing polls.",
    body: "A waitable timer wakes the capture thread once per frame, in the multimedia scheduling class, and three encode slots rotate so it never waits on the GPU.",
  },
  {
    title: "Nothing runs on the shader cores.",
    body: "Single-pass rate control, no B-frames, no lookahead, no adaptive quantization. An unchanged screen is repeated GPU-side.",
  },
  {
    title: "Capture gets the GPU first.",
    body: "High GPU thread and scheduling priority, never realtime, and no power throttling.",
  },
  {
    title: "Sound is kept raw.",
    body: "It is encoded to AAC only when you save, on the saving thread. Long silences are dropped.",
  },
  {
    title: "Saves run in the background.",
    body: "At low CPU, I/O and memory priority, so they never compete with the game.",
  },
] as const satisfies readonly Point[];

export const tips = [
  {
    title: "Keep the NVIDIA NvFBC backend.",
    body: "It is the default and costs the game least. Switch to Windows Graphics Capture for a laptop whose display hangs off the integrated GPU, or for HDR.",
  },
  {
    title: "Record at the frame rate your clips need.",
    body: "Cost scales with frames captured, not with codec or bitrate. 60 fps costs about a third of what 240 fps does.",
  },
  {
    title: "Pick the best codec your card offers.",
    body: "They all cost the same CPU; they differ in picture per bit.",
  },
  {
    title: "Keep the buffer in RAM if you have the room.",
    body: "Otherwise VRAM if the card has the room, otherwise Disk on an SSD. Its size is bitrate × length: 30 s at 50 Mb/s is about 190 MB.",
  },
  {
    title: "Turn off system sound if you never use it.",
    body: "It is cheap, under a fifth of the video's CPU, but not free.",
  },
] as const satisfies readonly Point[];

export interface Codec {
  readonly name: string;
  readonly bitrate: string;
  readonly cards: string;
}

export const codecs = [
  { name: "AV1", bitrate: "about half of H.264's", cards: "RTX 40 series and newer" },
  { name: "HEVC", bitrate: "about two thirds of H.264's", cards: "RTX 20 series and newer" },
  { name: "H.264", bitrate: "the most; plays everywhere", cards: "every NVIDIA GPU" },
] as const satisfies readonly Codec[];
