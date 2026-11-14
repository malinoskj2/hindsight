# Changelog

## 0.2.1

- Share limit (MB), initially 20 to match Discord’s free upload limit, keeps clipboard clips small: a larger saved clip gets a separate NVENC H.264 MP4 copy, at up to 1080p and 60 fps. The original stays intact. Right-click a clip for Copy (original), or Generate shareable to make its copy in the background, with its progress over the clip's thumbnail; once made, the menu offers Copy shareable instead.

- A hotkey field says "Press keys…" when you click it and shows the modifiers as you hold them. Nothing typed goes into it, and Esc keeps the hotkey there was.
- If another app already uses the keys you press as its hotkey, Settings says so under the hotkey fields. It also says when a key needs Ctrl, Alt or Win held with it.
- Insert can be a hotkey on its own, as the function keys, Print Screen, Pause and Scroll Lock already could.
- A new setting, Copy saves to the clipboard, puts each clip and screenshot on the clipboard as it is saved, so Ctrl+V pastes it into Discord, a chat or a folder.
- Trim a clip: right-click it in the Clips tab and choose Trim. Play or drag through it, set where it starts and ends, and Save trim encodes that part on the GPU as a new clip beside the others, with its sound. The original is kept.
- Trim lets you choose how the cut is saved: MP4 or MKV, the codec, and either a bitrate or a file size to stay under. A file size makes the cut fit it, encoding again at a lower bitrate if the first try comes out too large. MP4 and H.264 to start.
- Record only the full-screen game's own sound, leaving out Discord, music and the rest of the desktop: under Advanced, set Sound from to Only the full-screen app's own sound. It needs Windows 10 version 2004 or newer; older Windows records all sound as before. With Whole desktop, always, all sound is recorded.

## 0.2.0

- Screenshots: Alt+F8 saves a picture of the whole monitor in front, whether or not hindsight is recording. Screenshots go into a `screenshots` folder and clips into a `videos` folder, both in the clip folder.
- A Screenshots tab beside Clips shows the screenshots you have taken.
- The default hotkeys are now Alt+F7 for a clip and Alt+F8 for a screenshot, clear of the NVIDIA overlay's keys. Hotkeys already in your settings are kept.
- Click a hotkey field and press the keys to set it.
- Record your microphone as well as the system sound. Your voice is in the clip's main sound, so it plays everywhere the clip does, and also on a track of its own, so you can turn it up, down or out when editing.
- Choose which output device's sound and which microphone to record, under Advanced. Default device follows Windows; a device you picked that is unplugged is replaced by the default until it is back.
- Save clips as MKV as well as MP4, and screenshots as JPEG, WebP or AVIF as well as PNG.
- Screenshots and clip saves run in Windows' efficiency mode, so they take a little longer but use less CPU.
- Settings shows the common options first. Advanced opens the rest.
- The replay buffer and capture method now suit the machine until you choose: the buffer goes in RAM, video memory or on disk depending on how much there is, and a laptop whose display runs on the integrated GPU uses Windows Graphics Capture.
- After you alt-tab out of a full-screen game, the replay holds the game's last picture rather than recording the desktop.
- Recording with NvFBC costs a game fewer frames, now fewer than NVIDIA's Instant Replay: in Counter-Strike 2 at 1080p and 240 fps, 3% of its frame rate, against 5% before and 4% for Instant Replay.
- With Record set to full-screen apps, NvFBC grabs the game's own frames rather than the monitor, as Instant Replay does. Nothing is added to the game.
- hindsight checks for updates and installs them from the About tab. Installers are signed, and hindsight only runs one it can verify. After an update it shows what is new, and About's What's new shows it again.
- A new setting quits hindsight when you close its window, rather than hiding it in the tray.
- The Status tab says what failed and when.
- The About tab links to the website.
- Windows lists hindsight by its name, rather than a long description, in the taskbar's tray icon settings, Task Manager and startup apps.
