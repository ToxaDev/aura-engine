---
name: Bug report
about: Something converts wrong, plays wrong, crashes, or sounds off
title: ''
labels: bug
assignees: ''
---

**What happened**

**What you expected**

**Settings**
The two lines under the heading at the top of the window, e.g.:
`FIR [Kaiser] 64-bit • 30M Taps` / `FS8 (352.8k/384k) • FLAC`
and the stages lit in Advanced DSP — or simply the name of a converted file,
which lists every stage that ran, e.g.
`Track [AE · 44.1k→352.8k · Kaiser 10M · f64 · SUB15·AA·PFR·TFS].flac`

**Input file**
Format / sample rate / bit depth (e.g. FLAC 44.1 kHz 16-bit):

**If it happened while playing (not converting)**
- Output device, and the FS multiplier:
- BIT-PERFECT on or off:
- Instant start on or off (the player's right-click menu):
- Was the GPU chip in the player lit?
- Does it happen with Hardware GPU Acceleration unticked?
- If it was a radio stream: its address, or the station's name in the catalog:

**Environment**
- Windows version:
- GPU model + driver version:
- Does it reproduce with **Hardware GPU Acceleration** off? yes / no

**Log output**
```
paste relevant log lines here
```
