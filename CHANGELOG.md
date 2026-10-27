# Changelog

## 0.2.0

- Screenshots: Alt+F8 saves a picture of the whole monitor in front, whether or not hindsight is recording. Screenshots go into a `screenshots` folder and clips into a `videos` folder, both in the clip folder.
- A Screenshots tab beside Clips shows the screenshots you have taken.
- The default hotkeys are now Alt+F7 for a clip and Alt+F8 for a screenshot, clear of the NVIDIA overlay's keys. Hotkeys already in your settings are kept.
- Click a hotkey field and press the keys to set it.
- Record your microphone as well as the system sound. Your voice is in the clip's main sound, so it plays everywhere the clip does, and also on a track of its own, so you can turn it up, down or out when editing.
- Choose which output device's sound and which microphone to record, under More options. Default device follows Windows; a device you picked that is unplugged is replaced by the default until it is back.
- Save clips as MKV as well as MP4, and screenshots as JPEG, WebP or AVIF as well as PNG.
- Screenshots and clip saves run in Windows' efficiency mode, so they take a little longer but use less CPU.
- Settings shows the common options first. More options opens the rest.
- The replay buffer and capture method now suit the machine until you choose: the buffer goes in RAM, video memory or on disk depending on how much there is, and a laptop whose display runs on the integrated GPU uses Windows Graphics Capture.
- After you alt-tab out of a full-screen game, the replay holds the game's last picture rather than recording the desktop.
- hindsight checks for updates and installs them from the About tab. Installers are signed, and hindsight only runs one it can verify. After an update it shows what is new, and About's What's new shows it again.
- A new setting quits hindsight when you close its window, rather than hiding it in the tray.
- The Status tab says what failed and when.
- The About tab links to the website.
