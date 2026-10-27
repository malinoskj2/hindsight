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
    body: "With system sound as AAC, and your microphone on a track of its own if you like. The settings only offer what your GPU can encode.",
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
    title: "Screenshots cost nothing until you take one.",
    body: "A screenshot is of the whole monitor in front, recording or not. It reuses the picture being recorded when there is one, and otherwise captures the monitor once, in the background.",
  },
  {
    title: "Saves run in the background.",
    body: "At low CPU, I/O and memory priority, so they never compete with the game.",
  },
] as const satisfies readonly Point[];

export interface Setting extends Point {
  readonly id: string;
  readonly fields: readonly string[];
}

export interface SettingGroup {
  readonly name: string;
  readonly settings: readonly Setting[];
}

export const settingGroups = [
  {
    name: "Recording",
    settings: [
      {
        id: "length",
        fields: ["Replay length (s)"],
        title: "Replay length costs room, not CPU.",
        body: "The buffer holds bitrate × replay length: 30 s at 50 Mb/s is about 190 MB.",
      },
      {
        id: "frame-rate",
        fields: ["Frame rate"],
        title: "Record at the frame rate your clips need.",
        body: "Cost scales with frames captured, not with codec or bitrate. 60 fps costs about a third of what 240 fps does.",
      },
      {
        id: "hotkeys",
        fields: ["Save hotkey", "Screenshot key"],
        title: "Set one hotkey for clips and one for screenshots.",
        body: "Alt+F7 saves a clip and Alt+F8 a screenshot. To change one, click its field and press the keys, or type them.",
      },
      {
        id: "mode",
        fields: ["Record"],
        title: "Record full-screen apps, the default.",
        body: "Nothing runs while none is in front. The other choice records the whole primary monitor all the time.",
      },
      {
        id: "folder",
        fields: ["Clip folder"],
        title: "Pick where clips and screenshots go.",
        body: "Clips land in the folder's videos subfolder, screenshots in its screenshots subfolder.",
      },
      {
        id: "sound",
        fields: ["Record system sound", "Record the microphone"],
        title: "Turn off sound you never use.",
        body: "System sound is cheap, under a fifth of the video's CPU, but not free. Leave the microphone off unless you want your voice in the clips, where it gets a sound track of its own.",
      },
      {
        id: "cursor",
        fields: ["Record the mouse cursor"],
        title: "The cursor is free either way.",
        body: "Recording it makes no measurable difference.",
      },
    ],
  },
  {
    name: "More options",
    settings: [
      {
        id: "codec",
        fields: ["Bitrate (Mbps)", "Codec"],
        title: "Pick the best codec your card offers.",
        body: "They all cost the same CPU; they differ in picture per bit. A higher bitrate makes the buffer larger, not slower.",
      },
      {
        id: "backend",
        fields: ["Backend"],
        title: "Keep the NVIDIA NvFBC backend.",
        body: "It is the default and costs the game least. Switch to Windows Graphics Capture for a laptop whose display hangs off the integrated GPU, or for HDR.",
      },
      {
        id: "memory",
        fields: ["Buffer in"],
        title: "Keep the buffer in RAM if you have the room.",
        body: "Otherwise VRAM if the card has the room, otherwise Disk on an SSD.",
      },
      {
        id: "formats",
        fields: ["Clip format", "Image format"],
        title: "Save clips as MP4 or MKV, screenshots as PNG, JPEG, WebP or AVIF.",
        body: "MP4 plays everywhere. PNG and WebP keep every pixel; JPEG and AVIF are smaller, and AVIF takes a second or two of one core to save.",
      },
    ],
  },
  {
    name: "General",
    settings: [
      {
        id: "tray",
        fields: ["Close to tray"],
        title: "Closing the window keeps it recording.",
        body: "Close to tray is on by default, so hindsight stays in the tray. Turn it off and closing the window quits.",
      },
      {
        id: "updates",
        fields: ["Check for updates"],
        title: "Updates are one click.",
        body: "hindsight looks for a newer release when it starts and once a day. The About tab's Update button installs it and starts hindsight again. Turn it off and nothing is checked until you select Check now.",
      },
    ],
  },
] as const satisfies readonly SettingGroup[];

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
