# Vibrance — Specification

**Status:** Draft — not signed off
**Last updated:** 2026-11-01

This document defines what Vibrance is, what it does, and what it looks like. Once signed off, it is the sole brief for implementation. Anything not written here is out of scope unless added.

---

## 1. Overview

Vibrance is a native music player for Linux. It is simple, fast, and fully customizable in appearance and behavior.

## 2. Guiding principles

1. **We own every pixel.** Nothing in the UI comes from the system toolkit, system theme, or desktop environment. Fonts, icons, controls, and window chrome ship with the app. The only exceptions are the opt-in system title bar and the system folder picker (3.1.4).
2. **Fast.** Any delay a person can notice is a bug (section 7).
3. **Simple.** Few concepts and few settings screens. Defaults that work well out of the box.
4. **Customizable.** Look and behavior are configurable without recompiling.

## 3. Technology

### 3.1 GUI framework

#### Requirements

The framework **must**:

- Render all widgets itself. It must not wrap or inherit from GTK, libadwaita, Qt Widgets, or any toolkit that pulls in system themes, fonts, or icons.
- Produce a native compiled binary. No web views and no browser runtime.
- Use GPU-accelerated rendering, with a software fallback.
- Redraw only when something changes, so idle CPU stays near zero.
- Run natively on Wayland, with X11 support.
- Allow theming at runtime (colors, fonts, sizes, spacing) from user config without recompiling.
- Handle virtualized lists and grids for libraries of 100k+ tracks.
- Support an undecorated window with our own title bar and window controls (see 3.1.4).
- Be documented and stable enough for an agent to build from this spec.

#### Excluded

| Option | Why it's out |
|---|---|
| GTK / libadwaita (incl. gtk-rs, Relm4) | Explicitly rejected. Inherits system theming. |
| Qt (Widgets and QML) | Large system-integrated runtime, platform-theme plumbing. Same baggage. |
| wxWidgets | Wraps GTK on Linux. |
| Flutter | The Linux embedder hosts the app inside a GTK window. |
| Tauri / Wails | Uses WebKitGTK on Linux. A web view, not native. |
| Electron | Not native. Heavy. |
| egui | Fully custom-drawn, but immediate mode gives a tool-like look and makes polished, animated consumer UI harder. Viable, not preferred. |

#### Shortlist (all Rust)

| | **Slint** | **iced** | **GPUI** (Zed) |
|---|---|---|---|
| Model | Declarative `.slint` markup + Rust logic | Elm-style, all Rust | Hybrid immediate/retained, Tailwind-like styling in Rust |
| Rendering | Own renderers (Skia / FemtoVG / software) over winit | wgpu, tiny-skia fallback | GPU (Vulkan on Linux) |
| Runtime theming | Global properties settable at runtime. The `.slint` interpreter can load user UI files at runtime (deep skinning). | Style closures driven by runtime config | Build our own |
| Large lists | Built-in virtualized `ListView` | Build our own | Built-in `uniform_list` |
| Animation | Declarative, built-in | Basic support | Built-in |
| Custom drawing (visualizers, waveforms) | Limited: image buffers or GL underlay | Excellent: custom widgets and shaders | Good |
| Maturity | Stable 1.x, strong docs | Pre-1.0, API churn, thinner docs. Proven by the COSMIC desktop. | API unstable, sparse docs, built around Zed's needs |
| License | GPLv3, royalty-free (with attribution), or commercial | MIT | Apache-2.0 |

#### Decision: **iced**

Visualizers are a core feature (4.3, 4.4), and custom drawing is where iced is strongest. Custom widgets, `Canvas`, and wgpu shaders make spectrum bars, the animated seek bar, and full-screen visualizers first-class features rather than workarounds. Slint was the runner-up and was rejected because its custom drawing is limited.

Constraints:
- **Pin the iced version.** The API changes between releases, so do not upgrade casually.
- **Build our own virtualized list and grid widgets.** iced has none built in.
- **Rendering paths:** wgpu is the primary renderer.
  - The bar spectrum and seek bar are drawn with `Canvas`, so they also work on the tiny-skia software fallback.
  - The feedback visualizer (4.4) needs wgpu. On the fallback, it is replaced by an enlarged bar spectrum.
- **Components:** build our own component set. Do not lean on iced's default widget styling.

#### 3.1.4 Implications of "own every pixel"

- **Window decorations:** a user setting, `title_bar = "custom" | "system"`. The default is `custom`, and changing it takes effect immediately.
  - `custom`: the window runs undecorated. Vibrance draws its own title bar, window controls, drag regions, and resize handles.
  - `system`: the window requests server-side decorations from the compositor. On compositors that don't provide them (notably GNOME), winit's fallback client-side decorations are used, with an Adwaita-like look.
- **Fonts and icons:** Ship them with the app. Never use system fonts or icon themes.
- **File/folder pickers:** use the system dialog via xdg-desktop-portal (e.g. the `rfd` crate). This is an accepted exception to principle 1.

### 3.2 Audio

**Stack:**
- Decoding uses Symphonia (pure Rust), plus libopus for Opus.
- Output uses cpal, which on a typical desktop goes through ALSA to PipeWire (or PulseAudio).
- **No external player processes.** Vibrance never launches mpv, ffplay, or similar. All audio is decoded and played inside the Vibrance process.

**Threading:**
- **Decode thread:** decodes the current file a fraction of a second ahead into a lock-free ring buffer.
- **Output callback** (real-time thread): pulls from the ring buffer. It never allocates, locks, or waits on the UI.
  - It also publishes the samples it plays, with timestamps, for the visualizers and seek bar. They show what is heard, not what was decoded. The tap sits before the volume stage, so visuals don't shrink when the volume is lowered.
- **UI thread:** sends commands (play, pause, seek, stop) to the audio side over a channel and never blocks on them.

**No orphaned audio.** The user must always be able to stop sound.
- Playback lives inside the Vibrance process, so its audio stream belongs to that process.
  - If Vibrance exits for any reason (closed, crashed, killed), the OS closes its connection to the sound server.
  - Sound then stops within the output buffer length (target ≤ 100 ms).
- Closing the main window exits the app and stops playback. There is no tray or background mode.
- A file that fails to decode must not crash the app. Vibrance marks the track as unplayable and advances.
- **MPRIS:** Vibrance registers on D-Bus as an MPRIS media player (via `zbus`, no GTK).
  - Media keys, the desktop's media controls, and `playerctl` can pause or stop it.
  - The MPRIS handler runs off the UI thread, so these controls work even if the window hangs.

**Signal path:** decode → resample (`rubato`, only when needed) → normalization gain (if on) → visualizer tap → volume → output.

**Gapless playback:**
- The decode thread opens and decodes the next item (as set by the queue and playback modes) before the current one ends. Tracks join with no gap, including a Repeat-song loop.
- Encoder delay and padding are trimmed (MP3 LAME/iTunes info, AAC).
- The output stream runs at one fixed rate. Tracks at other sample rates are resampled, so a rate change never causes a gap.

**Volume normalization** (setting, off by default):
- Uses the ReplayGain track gain from tags, targeting the ReplayGain 2.0 reference (−18 LUFS).
- **Untagged files:**
  - Files without ReplayGain tags are measured (EBU R128 via `ebur128`).
  - Measuring runs in the background at low priority, and only while normalization is on.
  - Results are cached in the library database.
  - Until a track is measured, it plays unadjusted.
- Gain is capped using the track's peak, so normalization never clips.

**Volume:**
- Software volume inside Vibrance. It never changes the system volume.
- A perceptual (cubic) curve makes the slider feel even across its range.
- Changes ramp over ~10 ms to avoid clicks.

### 3.3 Language & build

Rust.

### 3.4 Storage

| What | Where | Format |
|---|---|---|
| Settings | `$XDG_CONFIG_HOME/vibrance/config.toml` (default `~/.config/vibrance/`) | TOML |
| Library database and UI state (queue, layout, columns, volume, playback modes) | `$XDG_DATA_HOME/vibrance/library.db` (default `~/.local/share/vibrance/`) | SQLite |
| Album art thumbnails | `$XDG_CACHE_HOME/vibrance/art/` (default `~/.cache/vibrance/`) | Image files |
| Startup snapshot (7.2) | `$XDG_CACHE_HOME/vibrance/library.snapshot` | Binary, memory-mapped |
| GPU pipeline cache | `$XDG_CACHE_HOME/vibrance/gpu-pipeline.cache` | Binary |

Vibrance never modifies, moves, or deletes audio files.

## 4. Features

### 4.1 Library browser (multi-pane)

Three panes: **Artists**, **Albums**, **Songs**. Selection filters downward.

**Filtering:**
- Artists and Albums each have a pinned first row, **All (N)**. Selecting it means "no selection." It is the default state.
- No artist selected: Albums lists every album.
- Artist(s) selected: Albums lists only those artists' albums.
- No album selected: Songs lists every song from the albums currently listed in Albums.
- Album(s) selected: Songs lists only those albums' songs.
- Changing the artist selection resets Albums to **All**.
- Ctrl/Shift multi-select works in all panes. Filtering uses the union of the selection.
- The Artists pane lists **album artists**. A track with no album artist falls back to its track artist. Compilations therefore stay under one artist.

**Columns** (all three panes):
- Clicking a header sorts by that column. Clicking again reverses the order. Sorting is stable, so ties keep the pane's default order.
- Drag a header edge to resize. Drag a header to reorder.
- Right-click the header to show or hide columns.
- Text sorts case-insensitively and naturally ("Track 2" before "Track 10").
- Column order, widths, visibility, and sort persist across restarts.

| Pane | Visible by default | Hidden by default | Default sort |
|---|---|---|---|
| Artists | Artist, Albums, Tracks | Duration | Artist |
| Albums | Album, Artist, Year, Tracks, Duration | Genre | Artist → Year → Album |
| Songs | #, Title, Artist, Album, Duration | Disc, Year, Genre, Album Artist, Format, Bitrate, Sample Rate, Date Added, File Path | Album Artist → Year → Album → Disc → # |

**Type to jump** (all panes, including Queue):
- There is no search bar. Panes never filter by typed text. Typing only moves the selection.
- With a pane focused (click it), typing builds a search term. The selection jumps to the best match.
- **Best match:**
  1. The first row, in current display order, whose primary column starts with the term. Matching ignores case and accents.
  2. If no row starts with it, the first row whose primary column contains it.
  3. If no row contains it, the selection stays put.
- The **All** row never matches.
- **Primary column:** Artist (Artists), Album (Albums), Title (Songs, Queue).
- **Resetting the term:**
  - The term resets after 1 s without typing (configurable).
  - Escape resets it immediately.
  - Backspace removes the last character.
- While a term is active, it shows in a small transient overlay in the pane.
- A jump is a normal selection change. It scrolls the row into view and filters the downstream panes, just like a click.

### 4.2 Queue & playback

**All playback goes through the Queue pane.** Nothing plays from the library panes directly.

**Playing from the library replaces the queue.** This covers double-click, Enter, and context-menu "Play":
- On a song: clears the queue, adds the song, and plays it.
- On an album or artist: clears the queue, adds all its songs (in the order below), and plays the first one.
- On a multi-selection: clears the queue, adds the songs of every selected item, and plays the first one.

**Dragging to the queue** (adds without replacing):
- Any rows from Artists, Albums, or Songs can be dragged into the queue. Their songs are inserted at the drop position, shown by an insertion line. Dropping on empty space appends.
- The **All** rows are not draggable.

**Song order when queuing:**
- An artist's songs go by album (Year → Album), then Disc → #.
- An album's songs go by Disc → #.
- Several selected rows go in the order they appear in their pane.

**Managing the queue:**
- Drag items within the queue to reorder them.
- Delete or the context menu removes the selected items.
- **Clear** empties the queue but does **not** stop the current song.
  - The song keeps playing until it ends, Stop is pressed, or the app closes. The now-playing strip keeps showing it.
  - When it ends, playback continues with the first queue item if anything was added since the clear. Otherwise playback stops.
- Removing the current item works the same way: it keeps playing, then playback continues with the item that followed it.
- Double-clicking a queue item jumps playback to it.
- The current item is highlighted. Played items stay in the queue.
- At the end of a track, playback advances to the next item. What happens at the end of the queue depends on the repeat mode.

**Transport** (in the now-playing strip): Previous, Play/Pause, Stop, Next.
- **Previous:** restarts the current song if more than 3 s in. Otherwise it goes to the previous item.
- **Stop:** halts playback and resets the position to the start of the current item.

**Volume** (in the now-playing strip):
- An M3 slider, 0–100%.
- Clicking the speaker icon toggles mute.
- The scroll wheel over the slider adjusts the volume in 2% steps. A tooltip shows the percentage.
- Volume persists across restarts and is exposed through MPRIS.

**Playback modes:** buttons in the Queue pane header. They apply to the queue only.
- **Shuffle** (toggle):
  - Turning it on randomly reorders the upcoming items (everything after the current one) in the visible queue. Vibrance remembers their original order.
  - Turning it off restores the original order of the items still upcoming.
  - While it's on, Play from the library shuffles the new queue and starts with the first item in the shuffled order.
  - Dragged items go exactly where they are dropped.
- **Repeat** (one button, cycles Off → Repeat queue → Repeat song, with a distinct icon for each state):
  - **Off:** playback stops after the last item.
  - **Repeat queue:** after the last item, playback wraps to the first. If shuffle is on, the queue is reshuffled on wrap.
  - **Repeat song:** the current song loops. Next and Previous still move through the queue.
- Shuffle and repeat state persist across restarts.

**Queue pane display:**
- The header holds Shuffle, Repeat, and Clear.
- Columns: playing indicator, position, Title, Artist, Duration.
- Columns resize but do not sort, because the queue order is the play order.
- The footer shows the item count and total duration.
- Unplayable or missing files are dimmed with an indicator and skipped.

**Persistence:** queue contents and the current position persist across restarts. Playback does not auto-resume.

### 4.3 Seek bar

**Reference:** `reference/rivian-os2-seek-bar.png` (Rivian OS 2 player).

**Look:**
- A thick track with fully rounded ends.
- The elapsed portion is a solid light fill. The remaining portion is the same thickness in a dim, translucent tone.
- The thumb is a small solid light circle at the playhead.
- **The hump:** around the thumb, the track lifts into a smooth bell-shaped arc that passes over the thumb and returns to the baseline. The elapsed fill ends where the hump begins. The hump itself is drawn in the remaining-portion tone.
- Elapsed time sits below the left end (`0:19`). Remaining time, shown as negative, sits below the right end (`-2:04`).

**Motion** (inferred from a still image, to be confirmed):
- The thumb and hump move continuously with playback, never in steps.
- On hover, the hump rises higher and the thumb grows (spring-animated). Both settle back on leave.
- Dragging scrubs, with the hump following the cursor. The time labels preview the target, and the seek happens on release.
- Clicking anywhere on the track glides the thumb and hump to that point and seeks.

### 4.4 Visualizers

There are two visualizers:

| | Bar spectrum | Feedback visualizer |
|---|---|---|
| Where | Compact, in the now-playing strip | The **Visualizer** pane (6.2, hidden by default) and full screen |
| Look | Frequency bars with falling peak caps, in palette colors (5.3) | MilkDrop-style: warped trails, waveforms, and shapes, with full-spectrum color cycling. Ignores the palette. |
| Drawn with | iced `Canvas` | wgpu shaders |

**Full screen:**
- Entered by clicking the compact spectrum, the Visualizer pane's full-screen button, or F11. Escape exits.
- Fills the whole monitor.

**Mockup:** the "Full-screen visualizer" board in the mockups is a working WebGL prototype of this engine. Port its shaders to WGSL rather than redesigning them.

**Audio analysis** (shared by both visualizers, fed by the tap in 3.2):
- **FFT:** 2048-point with a Hann window, recomputed every frame.
- **Band levels:** bass (20–250 Hz), mid (250 Hz–4 kHz), and treble (4–16 kHz).
  - Each band has an instant value and a smoothed one (fast attack, slow release).
  - Values are normalized against a long-term running average, so quiet and loud tracks both move the visuals.
- **Beats:** spectral-flux onset detection with an adaptive threshold, plus a tempo estimate. Each beat event carries a strength.
- **Waveform:** the latest 512 samples, mixed to mono.

**Feedback visualizer engine.** Each frame runs three passes:
1. **Warp.** The previous frame is sampled through a per-pixel map and written to a second buffer (ping-pong). This is what leaves the trails.
   - The map combines zoom, rotation, swirl, sine warp, drift, and an optional kaleidoscope fold.
   - The sample gets a light blur, a hue rotation, and decay.
2. **Draw.** New geometry is added with additive blending: the waveform (as a ring or a line), beat-pulsed polygons, and treble sparkles. Colors cycle through the full hue range.
3. **Present.** A soft tone curve, a beat flash, and a vignette.

**What the audio drives:**
- Bass → zoom and pulse.
- Mid → rotation and hue speed.
- Treble → warp amount and sparkles.
- Beats → shape pulses, flashes, and preset changes.

**Presets:**
- **Contents:** a preset is a parameter set, stored as data so adding more is cheap. Parameters:
  - Motion: zoom, rotation, swirl, and drift.
  - Warp: amplitude, frequency, and speed.
  - Look: decay, hue speed, kaleidoscope folds, and blur.
  - Audio-response gains.
  - Drawn elements: waveform shape, shape sides, and sparkles.
- **Built-in set:** at least 12 presets ship. The mockup's 8 are the starting set.
- **Randomness:** every parameter wanders slowly within its preset's range, seeded per session, so a preset never looks the same twice.
- **Switching:**
  - Every 24–48 beats, Vibrance picks a random preset (never the current one) and switches on a beat.
  - Parameters morph over ~3 s. Discrete parameters (fold count, shape sides) switch at the midpoint.
- **Controls in full screen:**
  - N: next preset.
  - L: lock the current preset, which stops auto-switching.
  - Space: play/pause.
  - Escape: exit.
- When the preset changes, its name shows briefly.

**Full-screen overlay:**
- **Contents:**
  - Along the bottom: track info (art, title, artist), play/pause, and the seek bar.
  - Top right: the preset controls.
- **Surfaces:** translucent, at 82% opacity over the visuals.
- **Auto-hide:** the overlay and the cursor hide after 2.5 s without mouse movement, and return on movement.

**Render resolution:**
- The internal render scale adapts to hold the display refresh rate. It goes as low as 50% and is upscaled to the output.
- The reference machine is expected to run at full resolution at 120 Hz.

**Redraw policy:**
- **Playing:** visible visualizers redraw at the display refresh rate.
- **Seek bar:** redraws while it is animating, or when the playhead has moved at least ¼ physical pixel.
- **Paused:** the feedback visualizer keeps running for ~2 s so the trails fade out, then stops.
- **Hidden, paused, or stopped:** visualizers stop redrawing entirely, so the idle-CPU requirement still holds.

### 4.5 Library import & scanning

**Importing:**
- **Import Folder** opens the system folder picker. The chosen folder becomes a *tracked folder*. Every supported audio file in it and its subfolders is added.
- **Supported formats:** MP3, FLAC, Ogg Vorbis, Opus, AAC/M4A, ALAC, WAV, AIFF.
- **Tags** are read with `lofty`: title, artist, album artist, album, track #, disc #, year, genre, duration, format, bitrate, sample rate.
- **Missing tags:** Title falls back to the filename. Artist and Album fall back to "Unknown Artist" and "Unknown Album".
- **Album art:**
  - Source: embedded art first. Otherwise `cover`, `folder`, or `front` (`.jpg`/`.png`) in the album's folder.
  - Each image is extracted and scaled once, then cached as a thumbnail.
- **Background work:** import runs in the background with a progress indicator. Panes fill in as files are scanned, and the UI never freezes.
- **Tracked folders in settings:**
  - Settings lists the tracked folders.
  - Removing a folder removes its tracks from the library. The files themselves are untouched.
  - Nested or overlapping folders never create duplicate tracks.
  - Symlinks are followed, with loop protection.

**Fast launch:**
- On launch, Vibrance memory-maps the startup snapshot (7.2).
- It draws the fully populated UI before touching the database or the music folders.

**Staying up to date:**
- **Live watching:**
  - Tracked folders are watched with inotify (via the `notify` crate).
  - New, changed, deleted, and moved files update the library within ~2 s, with events debounced.
- **Launch rescan:**
  - Once the UI is up, a background rescan compares each file's size and modified time against the database.
  - Only new or changed files have their tags re-read. Missing files are removed.
- **Manual rescan:** the **Rescan** action does the same on demand.
- **Network mounts:** inotify does not see changes made from another machine on network mounts (NFS, SMB). The launch rescan and Rescan catch those.
- **Watch limit:** inotify needs one watch per directory. If the system limit (`fs.inotify.max_user_watches`) is hit, Vibrance tells the user and relies on rescans for the unwatched folders.

### 4.6 Keyboard shortcuts

**Rule:** printable keys are reserved for type-to-jump (4.1).
- A bare letter, digit, or punctuation key is never a shortcut. Shift does not count as a modifier.
- The one exception is **Space**: it toggles play/pause only when no type-to-jump term is active. Otherwise it is part of the term (e.g. "swollen m").

| Action | Shortcut |
|---|---|
| Play / Pause | Space (no type-to-jump term active) |
| Stop | Ctrl+. |
| Previous / Next track | Ctrl+← / Ctrl+→ |
| Seek −5 s / +5 s | ← / → |
| Volume −5% / +5% | Ctrl+↓ / Ctrl+↑ |
| Mute | Ctrl+M |
| Toggle Shuffle / cycle Repeat | Ctrl+S / Ctrl+R |
| Play selection (replaces queue) | Enter |
| Add selection to end of queue | Shift+Enter |
| Play selection next (after the current item) | Ctrl+Enter |
| Remove selected queue items | Delete (Queue focused) |
| Clear queue | Ctrl+Shift+Delete |
| Jump to the playing item in Queue | Ctrl+J |
| Move selection | ↑ ↓ PgUp PgDn Home End. Hold Shift to extend. |
| Select all in pane | Ctrl+A |
| Next / previous pane | Tab / Shift+Tab |
| Import Folder | Ctrl+O |
| Rescan | F5 |
| Settings | Ctrl+, |
| Full-screen visualizer | F11 (Escape also exits) |
| In full-screen visualizer: next preset / lock preset | N / L (no pane has focus there, so printable keys are free) |
| Quit | Ctrl+Q |

**Global media keys** (Play/Pause, Stop, Next, Previous):
- Handled through MPRIS (3.2), so they work while Vibrance is unfocused or minimized.
- KDE and GNOME route media keys to the active MPRIS player automatically.
- Vibrance ignores media-key events delivered to its own window, so a press never acts twice.
- On compositors that don't route them (e.g. Sway, Hyprland), the user binds the keys to `playerctl`. The README documents this.

### 4.7 Context menus

| Right-click on | Items |
|---|---|
| Artist, album, or song row | Play · Play Next · Add to Queue · — · Go to Artist · Go to Album (songs only) · — · Show in Folder |
| Queue row | Play · Remove · — · Go to Artist · Go to Album · — · Show in Folder |
| Column header | Checklist of columns (show/hide) · — · Reset Columns |
| Pane header | Hide Pane (not offered on Now Playing or Queue) |

**Behavior:**
- **Play Next** inserts the songs right after the current queue item. **Add to Queue** appends them. Neither replaces the queue.
- **Go to Artist / Go to Album** selects that item in its pane, which cascades the filter, and scrolls it into view.
- **Show in Folder** opens the containing folder in the system file manager (xdg-desktop-portal OpenURI).
- Menus act on the whole selection when the clicked row is part of it. Otherwise they act on the clicked row alone.

### 4.8 Album Art pane

- **What it is:** a dockable pane (6.2) showing the current track's art.
- **Sizing:** the art is the largest square that fits inside the pane's 12 px padding. It is centered, with Medium (12 px) corners.
- **Default:** shown, under Queue in the right column (5.2). It is hidden and shown from the Layout menu.
- **No header,** so the art gets all the space.
  - On hover, a small "Album Art" drag handle appears in the top-left corner.
  - Dragging the handle moves the pane, like a pane header elsewhere.
- **Art cache:**
  - Art is decoded off the UI thread at the pane's pixel size (up to 2048 px).
  - It is cached in `$XDG_CACHE_HOME/vibrance/art/`, alongside the thumbnails.
- **Track change:** the art crossfades over 300 ms.
- **No art:** a placeholder (album icon on `surfaceContainerHigh`).

## 5. Visual design

### 5.1 References

| File | Use it for | Do not take |
|---|---|---|
| `reference/winamp-layout.png` | Pane arrangement only: Artists and Albums side by side over Songs | Any of the look: beveled chrome, cramped small text, gray-on-black, 3D buttons. Also not the left navigation tree. |
| `reference/rivian-os2-seek-bar.png` | The seek bar (4.3) | — |
| Mockups: https://claude.ai/artifact/1kzBGa7t8bM7m4BGLUKM9K | The target look. Colors, type, shapes, layout, seek bar, spectrum, menus, settings, palettes. Palette colors were generated with the same crate and settings as 5.4. | Pixel positions. Where the spec and the mockups disagree, the spec wins. |

### 5.2 Default layout

```
┌───────────────────────────────────────────────────────────────┐
│ Title bar (custom or system, 3.1.4)                           │
├───────────────────────────────────────────────────────────────┤
│ Now playing: art · title/artist · transport · seek bar ·      │
│              volume · spectrum visualizer                     │
├────────────────────┬────────────────────┬─────────────────────┤
│ Artists            │ Albums             │                     │
├────────────────────┴────────────────────┤ Queue               │
│ Songs                                   │                     │
│                                         ├─────────────────────┤
│                                         │                     │
│                                         │ Album Art (4.8)     │
│                                         │                     │
└─────────────────────────────────────────┴─────────────────────┘
```

- All pane boundaries are draggable splitters. Sizes persist across restarts.
- Panes can be rearranged, hidden, and shown (6.2).
- The now-playing strip has an overflow menu (⋮) containing:
  - Import Folder
  - Rescan
  - Palette ▸ (quick switch)
  - Layout ▸ (show/hide panes, Reset Layout)
  - Settings

### 5.3 Visual style: Material 3

Vibrance follows **Material 3** (https://m3.material.io/). Where this section is silent, M3 guidance and default component specs apply. Deliberate deviations are listed at the end of this section.

**Units:** 1 dp = 1 logical pixel, scaled by the display scale factor (including Wayland fractional scaling).

**Bundled assets:**
- Font: **Roboto Flex** (OFL).
- Icons: **Material Symbols Rounded** (Apache-2.0), as a variable font.
- All time and duration text uses tabular figures, so digits don't jitter.

**Type scale** (M3 tokens):

| Use | Token | Size / line height |
|---|---|---|
| Now-playing title | Title Large | 22 / 28 |
| Now-playing artist | Body Large, `onSurfaceVariant` | 16 / 24 |
| Pane titles | Title Small | 14 / 20, medium weight |
| Column headers | Label Medium | 12 / 16, medium weight |
| Table rows | Body Medium | 14 / 20 |
| Seek bar time labels | Label Medium | 12 / 16 |
| Type-to-jump overlay | Title Medium | 16 / 24 |

**Color roles:**

| Element | M3 role |
|---|---|
| Window background | `surface` |
| Pane body | `surfaceContainerLow` |
| Pane header, column header, now-playing strip | `surfaceContainer` |
| Primary / secondary text | `onSurface` / `onSurfaceVariant` |
| Selected row | `secondaryContainer` / `onSecondaryContainer` |
| Currently playing queue item | `primary` indicator and text |
| Hover / focus / pressed / dragged | `onSurface` state layer at 8% / 10% / 10% / 16% |
| Dividers, column separators | `outlineVariant` |
| Play/Pause | Filled icon button, `primary` / `onPrimary` |
| Shuffle, Repeat | M3 toggle icon buttons |
| Seek bar elapsed fill and thumb | `primary` |
| Seek bar remaining track and hump | `secondaryContainer` |
| Spectrum bars / peak caps | `primary` / `tertiary` |

**Shape:**
- Panes are rounded containers using the Large shape (16 px).
- Selected and hovered rows use the Small shape (8 px), inset 4 px horizontally.
- Menus, dialogs, tooltips, and sliders use their M3 component defaults.

**Elevation:** tonal surfaces only. Shadows appear only on floating elements: menus, dialogs, and a pane being dragged.

**Motion:**
- Transitions use M3 easing and duration tokens: emphasized easing, and *medium2* (300 ms) for palette crossfades.
- Direct manipulation uses springs: the seek bar, pane dragging, and queue reordering.

**Deviations from M3:**
- **Table density.** M3 one-line list items are 56 dp, which is too sparse for a large library.
  - Rows are 32 px. Column headers are 36 px.
- **Pane gutters.** Gutters between panes are 8 px.
  - Each gutter is also the resize handle, showing an M3 drag handle on hover.
- **Seek bar.** It follows the Rivian design (4.3), using M3 slider color roles.

### 5.4 Color palettes

- **Generation:** each palette is a seed color plus an M3 scheme variant. All M3 color roles are generated at runtime for both light and dark.
  - Library: the `material-colors` crate (0.5.x), a Rust port of Google's `material-color-utilities`.
  - Settings: `SpecVersion::Spec2025` (current M3), `Platform::Phone` (the crate's default), contrast level 0.0.
- **Switching:** the palette is chosen from the overflow menu or Settings. It applies instantly with a 300 ms crossfade and persists across restarts.

| Palette | Seed | Scheme variant | Notes |
|---|---|---|---|
| **Vibrance** (default) | `#A23CF0` | Vibrant | Electric violet |
| **Tide** | `#0091C2` | Tonal Spot | Ocean blue |
| **Ember** | `#E8590C` | Tonal Spot | Burnt orange |
| **Moss** | `#5A8F29` | Tonal Spot | Green |
| **Graphite** | — | Monochrome | No hue |
| **Album Art** | Taken from the current album art | Content | See below |

**Album Art palette:**
- The seed comes from the current album art (Celebi quantizer + score, from the same crate). This is M3 "dynamic color", similar to the art-tinted Rivian player.
- It re-seeds on each track change and crossfades over 300 ms.
- When a track has no art, it falls back to Vibrance.

**Color mode:** Dark (default), Light, or System.
- System reads the freedesktop `color-scheme` preference through the xdg-desktop-portal Settings interface (D-Bus).
- It follows changes live.

## 6. Customization

### 6.1 Appearance

- Palettes and color mode are covered in 5.4.
- User themes and skins are out of scope (section 8).

### 6.2 Layout

**Panes:** Now Playing, Artists, Albums, Songs, Queue, Album Art, Visualizer.

**Rearranging:**
- Drag a pane by its header.
- Drop zones appear on the four edges of the pane under the cursor, and on the window edges.
- Dropping splits that pane (or the window edge) and inserts the dragged pane there.
- The dragged pane floats with a shadow and settles with spring motion.

**Resizing:**
- Drag the gutters between panes.
- Each pane has a minimum size. For list panes, that is the header plus 3 rows.

**Showing and hiding:**
- Use overflow menu → Layout.
- A hidden Artists or Albums pane counts as **All**, so it doesn't filter.
- Now Playing and Queue cannot be hidden.

**Reset Layout** restores the default layout (5.2).

**Persistence:** the layout, as a split tree, persists across restarts.

### 6.3 Settings

**Settings dialog:**
- Opens from the overflow menu as an M3 dialog.
- Sections: Appearance, Playback, Library, Behavior.
- Changes apply immediately.

**Config file:** settings are stored in `config.toml` (3.4).

| Setting | Key | Values | Default |
|---|---|---|---|
| Palette | `palette` | `vibrance`, `tide`, `ember`, `moss`, `graphite`, `album-art` | `vibrance` |
| Color mode | `color_mode` | `dark`, `light`, `system` | `dark` |
| Title bar | `title_bar` | `custom`, `system` | `custom` |
| Volume normalization | `normalization` | `true`, `false` | `false` |
| Type-to-jump reset | `type_to_jump_timeout_ms` | integer | `1000` |
| Tracked folders | `library.folders` | list of paths | empty |

## 7. Performance targets

**Rule:** any delay a person can notice is a bug. Every interaction must show its result on the next displayed frame. Launch is ≤ 300 ms (see 7.4 for why not 100).

**Reference machine** (the targets must hold here):
- AMD Ryzen 7 9800X3D, 64 GB RAM
- NVIDIA RTX 5090 (proprietary driver 615.x)
- Samsung 990 Pro NVMe
- KDE Plasma on Wayland
- 5120×1440 at 120 Hz, which gives an **8.3 ms frame budget**

**Reference library:** 100,000 tracks, 8,000 albums, 2,000 artists, generated synthetically (7.3).

### 7.1 Targets

| Area | Metric | Target |
|---|---|---|
| Launch | Process start → first presented frame, fully populated (library, queue, layout, now-playing art). Warm and cold start. | **≤ 300 ms**, of which Vibrance's own work (everything except toolkit and GPU driver start-up) **≤ 20 ms** |
| Launch | Splash screens, blank frames, partially populated frames, or "loading" states | **None** |
| Quit | Window gone after close | ≤ 1 frame |
| Quit | Process exit | ≤ 100 ms |
| Interaction | Any input (click, key, hover, type-to-jump) → visible response | **Next frame** (≤ 8.3 ms) |
| Interaction | Selection cascade, column sort, or dropping a 5,000-song artist into the queue | Next frame |
| Interaction | Scrolling, window resize, pane drag, all animations, full-screen visualizer | Every frame on time, zero dropped |
| Audio | Play → audible | ≤ 30 ms |
| Audio | Pause or Stop → silent | ≤ 30 ms |
| Audio | Seek → audible at the new position | ≤ 30 ms |
| Audio | Volume change → audible | ≤ 30 ms |
| Audio | Visualizer and seek bar vs. the audible sound | In sync within ±1 frame (compensated for output latency) |
| Audio | Gap or click between tracks | None |
| Audio | Buffer underruns (including during import, rescan, and heavy animation) | None |
| Background | Import: first tracks visible after the folder is chosen | ≤ 100 ms |
| Background | Import throughput | ≥ 2,000 files/s |
| Background | Launch rescan of 100k unchanged files | ≤ 1 s, with no effect on frame times |
| Resources | Idle (paused or stopped, nothing animating) | ~0% CPU, no timer wakeups |
| Resources | Playing, compact spectrum visible | ≤ 5% of one core |
| Resources | Memory (resident) with the reference library | ≤ 300 MB |

### 7.2 Required design measures

These are mandatory. The targets are not reachable without them.

1. **Startup snapshot.**
   - SQLite stays the source of truth, but launch never queries it.
   - Startup reads a compact binary snapshot, memory-mapped and used zero-copy (e.g. `rkyv`). It holds tracks, albums, artists, precomputed sort keys, and a pre-sorted index for every sortable column.
   - The snapshot is rewritten in the background after library changes.
   - If it is missing or invalid, Vibrance falls back to SQLite and rebuilds it. Only that one launch may miss the target.
2. **Parallel init.** The window and GPU are set up on the main thread. Meanwhile, worker threads load the snapshot, the UI state, and the current track's art thumbnail.
3. **Nothing slow before the first frame:**
   - No file system scanning, inotify setup, MPRIS registration, D-Bus or portal calls, or system font enumeration.
   - The first frame uses the last-known system color scheme, cached in UI state. The portal value is read afterward.
4. **Bundled fonts only.** System fonts are never enumerated.
5. **GPU pipeline cache on disk.** Shaders are not recompiled on every launch.
6. **Sorting and filtering are index operations.**
   - Changing the sort switches to that column's pre-sorted index.
   - The selection cascade walks that index, filtering by membership.
   - Neither re-sorts.
7. **The UI thread does no I/O.** Decoding, tag reading, image decoding, and database and snapshot writes all happen on worker threads.
8. **Continuous state saving.** State is saved with ~1 s debounce, so quitting writes nothing.
9. **Album art:** thumbnails are decoded off-thread and uploaded as GPU textures, held in an LRU cache.
10. **Release profile:** `opt-level = 3`, LTO, `codegen-units = 1`.

### 7.3 Measurement

- **Startup tracing:** `VIBRANCE_TRACE=1` logs a startup timeline. Milestones: process start, window created, GPU ready, snapshot loaded, first frame presented.
- **Frame timing:** a toggleable debug overlay shows a frame-time graph and counts dropped frames.
- **Benchmark tool:** a `vibrance-bench` tool generates the reference library.
  - Short, silent, tagged audio files, for the import and rescan benchmarks.
  - A database and snapshot of the same library, for the UI benchmarks.
- **Coverage:** every row in 7.1 has an automated benchmark. A result outside its target fails the run.

### 7.4 Feasibility check first

**Result (2026-11-01):**
- On the reference machine, a bare iced window takes ~170 ms to its first frame with Vulkan, ~100 ms with GL, and ~23 ms with the software renderer.
- The NVIDIA driver's device and pipeline setup is most of it.
- Decision: accept ≤ 300 ms total, and do not patch iced's GPU setup. Vibrance's own start-up work must stay minimal (≤ 20 ms).
- v0.1 measures ~9 ms of its own work and ~205 ms total (warm).

**Original plan:**

- Before any features are built, build a bare iced window on the reference machine and measure launch → first frame.
- GPU driver initialization (Vulkan on the NVIDIA proprietary driver) is the main risk to the 100 ms budget.
- If that baseline uses more than half the budget, fix it before continuing. Options include the pipeline cache, wgpu backend and instance options, and deferring non-essential GPU setup.
- Report the measured numbers.

## 8. Out of scope

- User themes and skins, beyond the built-in palettes.
- A search bar, or any filtering by typed text.
- Tray or background playback after the window closes.
- Editing tags, or modifying files in any other way.
- Rebinding keyboard shortcuts.

---

## 9. Development & verification

The implementer must be able to see and check their own work without a person in the loop. Build these early, alongside the 7.4 feasibility check.

- **Offscreen frame render.**
  - `vibrance --render-frame out.png --fixture <dir> --size 2560x1440 [--state state.json]` renders one frame with the real renderer and no window.
  - It is used for visual snapshot tests, and to compare against the mockups.
- **Headless UI tests.** These drive the real application state with simulated input: clicks, double-clicks, keys, drags, and type-to-jump. They assert on state and on rendered snapshots.
- **Audio to file.**
  - `--audio-sink wav:<path>` writes the final output stream to a WAV file instead of the sound server.
  - Tests analyze the file to check gapless joins, normalization gain, volume ramps, and that Stop produces silence.
- **Fixture library.** `vibrance-bench` (7.3) generates it.
- **MPRIS.** Verified with `playerctl` (play-pause, stop, next, metadata).
- **Real-desktop check.** On KDE, `spectacle -b -n -a -o <file>` captures the active window.

## Open questions

_None currently._

## Decision log

| Date | Decision |
|---|---|
| 2026-11-01 | GUI framework must be fully custom-rendered and native. GTK and system-themed toolkits are excluded. |
| 2026-11-01 | Framework: iced (Rust), chosen for custom-drawing strength because visualizers are core. |
| 2026-11-01 | Title bar is a user setting: custom (default) or system. |
| 2026-11-01 | Folder selection uses the system portal dialog. |
| 2026-11-01 | Library uses cascading Artists → Albums → Songs panes with sortable, resizable, reorderable columns. |
| 2026-11-01 | All playback goes through the Queue. Rows can be dragged into it, and it can be cleared. |
| 2026-11-01 | Seek bar modeled on Rivian OS 2. Winamp is a layout reference only; its look is rejected. |
| 2026-11-01 | Seek bar motion goes in as specced in 4.3. Revisit after trying it. |
| 2026-11-01 | Play from the library replaces the queue. Clear does not stop the current song. |
| 2026-11-01 | No search bar. Type-to-jump moves the selection in every pane without filtering. |
| 2026-11-01 | Shuffle and repeat (queue/song) live on the Queue pane and apply to the queue only. |
| 2026-11-01 | Folder import with a SQLite cache, inotify watching, and rescans on launch and on demand. |
| 2026-11-01 | Audio plays inside the Vibrance process (Symphonia → cpal). No external player processes. MPRIS for system controls. |
| 2026-11-01 | Visual style follows Material 3, with documented density deviations. Six runtime-swappable palettes, including one taken from album art. |
| 2026-11-01 | Panes can be docked, resized, hidden, and shown. No theme or skin support beyond palettes. |
| 2026-11-01 | Gapless playback. Volume normalization (ReplayGain / R128) as an on/off setting. In-app volume slider. |
| 2026-11-01 | Performance: launch ≤ 100 ms fully populated. Every interaction lands on the next frame. Targets are hard requirements on the reference machine and verified by benchmarks. |
| 2026-11-01 | Keyboard shortcuts never use bare printable keys (type-to-jump owns them). Global media keys go through MPRIS. Context menus defined in 4.7. |
| 2026-11-01 | Mockups published. Palettes use M3 color spec 2025. Development requires offscreen rendering, headless UI tests, and WAV audio capture (section 9). |
| 2026-11-01 | Album Art pane: dockable and headerless, under Queue by default. Chosen over a taller top bar. |
| 2026-11-01 | Visualizer pane and full screen use a MilkDrop-style feedback engine of our own in wgpu (not projectM). The bar spectrum stays in the now-playing strip. |
| 2026-11-01 | Launch target relaxed to ≤ 300 ms (GPU driver start-up dominates). Do not patch iced's GPU setup. Vibrance's own start-up work ≤ 20 ms. |
