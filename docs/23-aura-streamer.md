# 23. Aura Streamer

Aura Streamer is the AuraEngine without a window. It is one small program that
runs on the computer next to the DAC, plays through the same engine, the same
filters and the same rack as the desktop app, and is controlled from a browser —
a phone, a tablet or another computer on the same network — or from any UPnP /
OpenHome control point. The sound is the engine's: nothing in the chain is
simplified for the remote. This page describes how to start it, what the remote
offers, how the control points reach it, how it updates itself, and what it
deliberately does not do.

- [Streamer or desktop app](#streamer-or-desktop-app)
- [1. Install and first start](#1-install-and-first-start)
- [2. The remote](#2-the-remote)
- [3. Your music](#3-your-music)
- [4. UPnP and OpenHome control points](#4-upnp-and-openhome-control-points)
- [5. Updating](#5-updating)
- [6. Security](#6-security)
- [7. Environment variables](#7-environment-variables)
- [8. Troubleshooting](#8-troubleshooting)

---

## Streamer or desktop app

| | Desktop app | Aura Streamer |
|---|---|---|
| **Window** | Its own window with the full interface | None — a console window with the engine's log |
| **Controlled from** | The window | A browser on any device on the network; UPnP / OpenHome control points |
| **Engine and rack** | The full rack, the converter, the player | The same engine and rack for playback (see [what the remote leaves out](#what-the-remote-leaves-out)) |
| **Library** | The player's list | Its own library of albums, artists, genres and folders, scanned from your music folders |
| **Best for** | Converting files, working at the desk | A computer that sits next to the DAC and is driven from the sofa |

The two can be installed side by side; they are separate programs.

---

## 1. Install and first start

Download the zip for your system from the releases page and unpack it anywhere
you can write to (a folder you can write to matters for [updating](#5-updating)).

| System | File |
|---|---|
| Windows | `aura-streamer-v<version>-windows-x64.zip` |
| Linux | `aura-streamer-v<version>-linux-x86_64.zip` |
| macOS | `aura-streamer-v<version>-macos-universal.zip` |

Start `aura-streamer` (`aura-streamer.exe` on Windows) on the computer that is
connected to the DAC. A console window opens and prints a banner with the
addresses to use, for example the computer's address on the network and its
name. Open one of them from a phone, a tablet or another computer on the same
network. The banner also shows how many filters it found on disk and where the
session log is.

- **The console window is the engine's log. Closing it stops the music.**
- **Port.** The remote answers on port 80, so the address is a plain
  `http://<address>`. When port 80 is taken or not allowed, it uses 8088
  (`http://<address>:8088`). `AURA_PORT` picks another port.
- **One copy per computer.** If a streamer already answers on this computer,
  a second one finds it and exits instead of starting.
- **Firewall.** The first time, Windows Firewall asks whether to allow the
  program. Allow it on private networks, otherwise the phone cannot reach it.
  Control-point discovery needs it too ([section 4](#4-upnp-and-openhome-control-points)).

### Filters

The streamer uses the same FIR filter files (`.npy`) as the app. It looks for
them in these places, in this order:

1. the folder named by `AURA_FILTER_DIR`;
2. on Linux and macOS, the filter folder in the app's data folder (where the
   desktop app's in-program download puts them);
3. `fir-optimizer/output` beside the streamer's program file;
4. `filters` beside the streamer's program file.

On Linux and macOS the filters the desktop app downloaded are found as they
are. On Windows the app keeps them next to its own program file, so the
simplest setup is to unpack `aura-streamer.exe` into the AuraEngine folder,
beside `aura-engine.exe`: both then use the same `fir-optimizer/output`. Or
point `AURA_FILTER_DIR` at that folder.

Without the desktop app nothing has to be fetched by hand: when the setting
chosen in the rack has no filter files, the remote shows the same line as the
app (**Download**, with the size) and the same dialog. The streamer downloads
the files itself, from the signed filter list, checks each one's size and
SHA-256, and puts them where it looks (on Windows `fir-optimizer/output` beside
its program file, on Linux and macOS the app's filter folder). The dialog's
**Storage** tab shows what the filters take and removes the ones this version
does not use. The filter packs on the releases page still work for an offline
computer.

### Where it keeps its files

Its own settings, library index and covers live in a `streamer` folder inside the
engine's data folder:

| System | Data folder |
|---|---|
| Windows | `%LOCALAPPDATA%\AuraEngine` |
| macOS | `~/Library/Application Support/AuraEngine` |
| Linux | `$XDG_DATA_HOME/AuraEngine`, or `~/.local/share/AuraEngine` |

`AURA_DATA_DIR` moves it. Logs are kept there as well.

---

## 2. The remote

The remote is the desktop app's own interface, adapted for touch. It is not a
simplified copy: the rack, the player bar and the list are the app's own
elements, so a setting you can change in the app you can change here, and the
remote follows the app's changes from one version to the next.

- **Library** — search, albums, artists, genres, folders and the app's
  playlists ([section 3](#3-your-music)).
- **Sound** — the rack as it is in the app: source, conversion and output. The
  strip along the top (FS multiplier, resolution, GPU, apodizing, headroom,
  output) opens the rack at that setting.
- **Player** — now playing with the cover and tags, transport, seek, output
  format, bit-perfect and volume, and the up-next list.
- **Analyzer** — opens as a full-screen page; on a phone it is split into pages
  you swipe through.

**Tablet and computer.** Below the top strip there are two columns: Library or
Sound on the left, the player and the list on the right.

**Phone.** One view at a time. The player bar is pinned at the foot with the
tabs Library, Player and Sound under it; the queue is part of Player.
A window such as the analyzer opens full screen and has a Back bar.

**Several devices, one state.** The streamer keeps the rack, its settings and the
list on the server. A phone and a tablet open at once show the same thing, and a
change on one is followed by the other. A device opened for the first time does
not start from defaults and does not reset the rack. Opening the page on a
second device while music plays does not interrupt it.

### What the remote leaves out

For safety and because there is no desktop to show them on, the remote
deliberately does not offer: converting files, loading a custom filter,
the ASIO input driver, the radio station catalog,
AutoEq import, crash reports, and the visualization scenes and GLSL studio (the
bouncing-bars analyzer is kept). Audio paths sent from the remote are accepted
only inside your music folders.

### Opening it quickly on a phone

The remote is a web page at the plain `http://` address; there is nothing to
install. To reach it in one tap, bookmark it or put a shortcut on the home
screen from the browser's menu (Android, Chrome: Add to Home screen; iPhone,
Safari: Share, Add to Home Screen). The shortcut opens the page in the browser.

On a page that cannot go full screen the full-screen controls are not shown.

---

## 3. Your music

The library is built from the music folders you choose.

- The first time it uses `AURA_MUSIC_DIR` if that is set, otherwise the Music
  folder in your user profile. Add or remove folders in the Folders tab of the
  Library; the list is kept in the streamer's data folder.
- It reads tags (title, artist, album, album artist, genre, year, track and disc,
  sample rate and bit depth, DSD, embedded art) with one pass over each file.
  Only new or changed files are read again; the result is kept, so the next
  start is fast. Hidden folders and the converter's own renders are skipped.
- Albums are grouped by folder and album tag; a `CD1` / `Disc 2` folder joins
  its parent. The best file of the album names its format tag, such as `24/96`
  or `DSD64`. Genre chips include Hi-Res and DSD.
- A rescan starts when the library is first opened and, when you open it, if
  the last one was more than 15 minutes ago. There is also a rescan button in
  Folders. Folders are not watched live.
- Album covers are made from the embedded art and cached.
- Play on an album makes the list that album; Add to queue appends it.
- DSD plays over DoP up to DSD256 live.

---

## 4. UPnP and OpenHome control points

The streamer is also a network audio renderer. A control point on the same
network (for example JRiver, BubbleUPnP or mConnect) sees it as **Aura Streamer**
(`AURA_UPNP_NAME` renames it), and plays through the engine and the rack.

- **What works.** UPnP AV (AVTransport, RenderingControl, ConnectionManager) and
  OpenHome (Product, Playlist, Time, Volume, Info): play, pause, stop, seek,
  volume and mute, a next track for gapless playback, and a real playlist in
  OpenHome mode. Internet radio streams from the control point play as streams.
- **Volume.** 0 to 100 on the control point is a step of 0.6 dB each; 0 is
  silence. The volume is shared with the remote.
- **Media.** Only `http` and `https` addresses on the network are accepted.
- **Limits.** A track is fetched whole before it starts, so the first track of a
  big file on Wi-Fi may take a few seconds to begin; the next one is fetched
  ahead. The remote's list does not list tracks a control point sends; the
  player shows what is playing. Shuffle is stored but not applied. The OpenHome
  Radio, Receiver and Credentials services are not offered.
- **Settings.** The rack settings are the engine's current ones. The first time,
  with no remote page opened since the start, it plays with the defaults.
- **Network.** Discovery uses UDP port 1900 (SSDP) and the control point reaches
  the renderer on its own TCP port, 49494 by default (`AURA_UPNP_PORT`).
  Allow them in the firewall on private networks. `AURA_UPNP=0` turns the
  renderer off.
- If the remote opens and starts playing while a control point is playing, the
  control point's playback stops: the last to take over wins.

---

## 5. Updating

The remote offers a new version with an Update button when there is one, with
what changed. Pressing it:

1. downloads the release zip for your system;
2. checks its signature against the same key the desktop app uses for its
   updates; a file that does not verify is not installed;
3. swaps the program file and starts the new version, which takes over the same
   port.

If the program's folder cannot be written to, the remote only tells you that a
new version exists and you update by hand. If the streamer runs under a service
manager (for example systemd, or `AURA_SUPERVISED=1`), it only exits after the
update and leaves restarting to the manager. The old program file is removed on
the next start.

---

## 6. Security

The streamer is for your home network, not for the internet.

- It answers only peers with private addresses (the local network and this
  computer). Anything else is refused.
- The page must be addressed by an address or a local name (an IP, a bare name,
  `.local`); requests carrying another name are refused, which guards against
  DNS rebinding.
- Changes (POST) must be JSON and must come from the streamer's own page (the
  Origin must match).
- The list of commands the remote can send is a fixed allowlist; see
  [what the remote leaves out](#what-the-remote-leaves-out). Files are only
  played from your music folders.
- UPnP control is guarded in the same way: private peers only, and a control
  request must match its declared action. Event callbacks go only to private
  addresses.
- There is no password or PIN. Do not forward its ports on your router: anyone
  on your network can control the music, and nothing here is meant to be exposed
  outside it. For remote access use a VPN into your home network.

---

## 7. Environment variables

All are optional. Set them in the shell or in a start script before starting
the program.

| Variable | Effect |
|---|---|
| `AURA_PORT` | Port of the remote. Default: 80, then 8088 if that is taken. |
| `AURA_FILTER_DIR` | Folder with the filter `.npy` files. |
| `AURA_DATA_DIR` | Where the engine and streamer keep their files. |
| `AURA_MUSIC_DIR` | The music folder used the first time. Default: the Music folder of the user. |
| `AURA_UPNP` | `0` turns the UPnP / OpenHome renderer off. |
| `AURA_UPNP_NAME` | The name control points show. Default `Aura Streamer`. |
| `AURA_UPNP_PORT` | TCP port of the renderer. Default 49494. |
| `AURA_SUPERVISED` | `1` when a service manager restarts the program: an update then only exits. |

---

## 8. Troubleshooting

- **The phone cannot open the address.** Check that it is on the same network as
  the computer (not a guest network or mobile data), that the firewall allowed
  the program on private networks, and try the other address from the banner.
- **"Cannot listen"** in the console. The port is in use; set `AURA_PORT` to a
  free one.
- **It starts and closes at once.** Another copy is already running on this
  computer; open its address.
- **No sound or a missing filter.** Check the banner's filter count; download
  the setting's files from the remote's **Download** line, or put them where
  [section 1](#filters) says.
- **The DAC is busy.** The streamer opens the device the way the app does, with
  the output mode set in the rack. In exclusive mode another program holding
  the DAC, the desktop app included, stops it from opening; close that program
  and press Play again.
- **A control point does not see it.** Check the firewall (UDP 1900 and the
  renderer's TCP port), that both are on the same subnet, and that `AURA_UPNP`
  is not 0.
- **Music stops when the console is closed.** It is meant to: leave the window
  open, or run it under a service manager on Linux.
