# Disc Xplorer

A cross-platform disc image browser and file extractor. Open a disc image, browse its filesystem, and pull out individual files or entire folders — no mounting required. Think ISOBuster or PowerISO, but native, free, and open source.

Runs on macOS, Windows, and Linux.

---

## Features

### Disc image formats

| Format | Notes |
|--------|-------|
| ISO / IMG | Raw 2048-byte and raw 2352-byte sector images |
| CUE/BIN | Single-file and multi-file (one BIN per track) |
| MDS/MDF | Alcohol 120% images |
| MDX | DiscJuggler images |
| NRG | Nero Burning ROM images |
| CCD/IMG/SUB | CloneCD images |
| CDI | DiscJuggler CDI |
| GDI | Sega Dreamcast GD-ROM images |
| CHD | MAME/RetroArch compressed hard-disk images |
| CSO / CISO | Compressed ISO (PSP / PS2) |
| ECM | Error Code Modeler compressed images |
| CDR / DMG | macOS disc images (mount via hdiutil) |

### Filesystems

| Filesystem | Systems |
|------------|---------|
| ISO 9660 + Joliet | Standard PC CD/DVD |
| UDF | DVD-Video, Blu-ray, modern optical media |
| HFS | Classic Mac OS discs |
| PC/Mac hybrid | Dual-filesystem discs with both ISO 9660 and HFS partitions |
| XDVDFS | Original Xbox and Xbox 360 game discs |
| 3DO OperaFS | 3DO Interactive Multiplayer |
| PC Engine CD-ROM | NEC PC Engine / TurboGrafx-16 |
| CD-i | Philips CD-i |
| GCM | Nintendo GameCube and Wii optical discs |

### Browse and extract

- Navigate the full directory tree of any supported disc image
- Extract individual files or entire directory trees to any destination
- No mounting required — all reads go directly through native Rust parsers

### Audio tracks

- View multi-track disc layouts with track numbers, durations, and sizes
- Export audio tracks as **WAV**, **MP3** (LAME), or **FLAC**

### Sector viewer

- Inspect raw sector data at any LBA
- Displays sector format (Mode 1/Mode 2), MSF address, and hex + ASCII content

### Mounting

- Mount disc images directly from the UI
- macOS: `hdiutil` (ISO, IMG, DMG, CDR)
- Windows: PowerShell `Mount-DiskImage` (ISO, IMG)
- Linux: CDemu virtual drive (CUE, MDS, MDX, NRG, CCD, and more); prompts to install if not present

### Physical drives

- Lists connected optical drives
- Open and browse physical discs the same way as image files
- Eject drives from the UI

### General

- Drag-and-drop to open disc images
- Dark theme
- Cross-platform: macOS (ARM + Intel), Windows, Linux

---

## Compared to other tools

| | Disc Xplorer | ISOBuster | PowerISO | Alcohol 120% | Daemon Tools |
|--|:--:|:--:|:--:|:--:|:--:|
| Free | ✓ | Partial | Partial | No | Partial |
| Open source | ✓ | No | No | No | No |
| macOS / Linux | ✓ | No | No | No | No |
| No install to browse | ✓ | No | No | No | No |
| Native file extraction | ✓ | ✓ | ✓ | No | No |
| Audio export (WAV/MP3/FLAC) | ✓ | ✓ | ✓ | No | No |
| Sector viewer | ✓ | ✓ | No | No | No |
| Xbox / GameCube / 3DO | ✓ | Partial | No | No | No |

ISOBuster and PowerISO are the closest feature equivalents. ISOBuster is Windows-only and partially free (file extraction is paywalled beyond a limit). PowerISO is commercial. Alcohol 120% and Daemon Tools are primarily virtual drive tools; file browsing is a secondary feature.

---

## Building

```
npm install
npm run tauri build
```

Requires Rust (stable), Node.js 18+, and the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your platform.

For development:

```
npm run tauri dev
```

---

## FFmpeg notice

Audio export (MP3/FLAC) uses FFmpeg libraries bundled at build time.
FFmpeg is licensed under the GNU LGPL v2.1 or later — see https://ffmpeg.org/legal.html.
