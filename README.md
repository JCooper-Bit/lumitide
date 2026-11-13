# Lumitide

This fork of [BreakLime/lumitide](https://github.com/BreakLime/lumitide) adds a persistent, keyboard-driven browser and player. **Build this fork from source for the features below**; upstream installers and releases do not include them.

A terminal music player for Tidal, written in Rust.

> [!WARNING]
> **If you are on v1.4.0 or earlier, please update to the latest version.**
> Tidal changed their streaming API — playback and downloads will fail on older versions.
> Grab the latest release from the [Releases](https://github.com/BreakLime/lumitide/releases) page or re-run the installer.

Stream audio from your Tidal account directly in the terminal, with album art, a live spectrum visualizer, beat/drop detection, and a background download queue.

> [!NOTE]
> **Audio quality is platform-dependent.** Windows uses the Tidal desktop app auth flow and streams/downloads **lossless FLAC**. Linux and macOS use a device code flow and receive **MP4 (HIGH quality / AAC)**. Native FLAC support for Linux/macOS is planned.
> See [docs/how-flac-works.md](docs/how-flac-works.md) for a full technical write-up of the auth flow and stream decryption.

![Lumitide demo](assets/demo.gif)

Press `?` for key bindings. Search with `/`, change search type with `Tab`, and press `Enter` to open or play. Music continues while browsing.

## Features

- Album, track, and artist search with in-screen query entry and filtering.
- Background playback: search and browse without stopping the current track.
- Playback queue: append tracks or entire collections, play next, jump to an item, remove, and reorder.
- Seek ±10 seconds or ±1 minute. Shuffle, repeat-one, repeat-all, and stop at the end.
- Like/unlike tracks, save/unsave albums, and follow/unfollow artists.
- Create playlists; add tracks/albums, remove tracks, and reorder. Playlist version checks reject conflicting edits.
- Artist pages with top tracks, albums, singles, and EPs.
- Complete paginated albums, playlists, mixes, and library collections.
- Queue, track position, shuffle, and repeat saved every five seconds and on exit. Restored sessions start paused; `Space` resumes.
- Startup album command that opens the TUI and immediately plays the full album in order.
- Automatic Noctalia/Pywal colour integration with live palette reload and terminal palette fallback.
- Existing Tidal streaming, Braille cover art, spectrum/beat visuals, media keys, radio, local file playback, and background downloads.

## Requirements

- A [Tidal](https://tidal.com) HiFi or HiFi Plus subscription
- A terminal with **truecolor** (24-bit) support (Windows Terminal, iTerm2, Alacritty, Kitty, etc.)

## Installation

### Quick install (one-liner)

**Linux / macOS:**
```sh
curl -fsSL https://raw.githubusercontent.com/BreakLime/lumitide/main/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/BreakLime/lumitide/main/install.ps1 | iex
```

Both scripts download the latest prebuilt binary for your platform and drop it somewhere on your `PATH`.

Pin a specific release:
```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/BreakLime/lumitide/main/install.sh | bash -s -- --version vX.Y.Z

# Windows (PowerShell)
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/BreakLime/lumitide/main/install.ps1))) -Version vX.Y.Z
```

To uninstall, remove the binary and undo the changes the installer made:
- **Linux / macOS:** `rm ~/.local/bin/lumitide`
- **Windows:** delete `%LOCALAPPDATA%\Programs\lumitide`, remove the `Lumitide` shortcut from `%APPDATA%\Microsoft\Windows\Start Menu\Programs`, and remove `%LOCALAPPDATA%\Programs\lumitide` from your User PATH (Settings → System → About → Advanced system settings → Environment Variables).

### Download (recommended)

Grab the latest release for your platform from the [Releases](https://github.com/BreakLime/lumitide/releases) page.

| Platform | File | Type |
|----------|------|------|
| Windows | `lumitide-installer.exe` | Installer — Start Menu shortcut, uninstaller, optional PATH |
| Windows | `lumitide-windows.exe` | Portable — just download and run, no install needed |
| Linux | `lumitide-linux` | Portable binary |
| macOS | `lumitide-macos` | Portable binary |

**Windows installer:** run `lumitide-installer.exe` and follow the wizard. Lumitide will appear in your Start Menu.

**Windows portable:** double-click `lumitide-windows.exe` or run it from a terminal.

**Linux / macOS:** make it executable first:
```sh
chmod +x lumitide-linux  # or lumitide-macos
./lumitide-linux
```

Optionally move it somewhere on your `PATH` so you can run it from anywhere:
```sh
mv lumitide-linux ~/.local/bin/lumitide
```

### Build from source

Requires the [Rust toolchain](https://rustup.rs).

```sh
git clone https://github.com/JCooper-Bit/lumitide.git
cd lumitide
cargo build --release
./target/release/lumitide
```

**Linux** also needs the ALSA development headers:
```sh
sudo apt install libasound2-dev  # Debian / Ubuntu
sudo dnf install alsa-lib-devel  # Fedora
```

## Authentication

Authentication is handled automatically on first run and differs by platform.

**Windows** — Lumitide opens a browser window for the Tidal login page. After
you log in, the OS redirects back to Lumitide automatically and the session is
saved. No copy-pasting required.

**Linux / macOS** — Lumitide prints a URL and a short code:

```
To log in to Tidal, visit:
  https://link.tidal.com/...
And enter code: ABCD-1234

Waiting for authorisation...
```

Visit the URL, enter the code, and approve the login.

In both cases your session is saved to `~/.config/lumitide/session.json` and
refreshed automatically on subsequent runs.

## Usage

Running `lumitide` opens the TUI. Use `/` to search, `Tab` to cycle Tracks / Albums / Artists, and `Enter` to submit. `Esc` cancels text entry. Text entry is used only for queries, filters, playlist names, download paths, and optional seek timestamps.

```sh
lumitide search "Netsky"               # track results
lumitide search "Discovery" --album    # album results
lumitide search "Daft Punk" --artist   # artist pages
lumitide mix                           # curated mixes
lumitide playlist                      # playlists, including creation/editing
lumitide library                       # liked tracks, albums, and artists
lumitide local                         # existing local-file player
lumitide config                        # edit the JSON configuration
```

### Play an album on startup

```sh
lumitide play album --id 30214
lumitide play album "Discovery" --artist "Daft Punk"
```

Both commands **open the TUI and start playing**. They load every track, start at track one in album order, and stop at the end. They override a restored queue's shuffle/repeat settings. An ID is the most reliable way to identify a particular release; it is the number in a Tidal album URL. Title lookup requires exactly one matching title/artist and shows an error instead of asking you to pick a release.

For a Hyprland startup file, launch a terminal to host the TUI. For example, with Kitty:

```ini
exec-once = kitty --class lumitide -e lumitide play album --id 30214
```

Replace the example ID and terminal with your choices. Authenticate to Tidal once before using autostart. A valid saved session is reused automatically.

## Key bindings

| Key | Action |
|---|---|
| `↑↓` / `k j`, `Home`, `End` | Navigate lists |
| `Enter` | Open collection/artist; play from selected track |
| `/`, `Tab` | Enter search; switch track/album/artist search |
| `F` | Filter the current list |
| `Esc` / `q` | Back (playback continues); quit from home |
| `Ctrl-q` / `Ctrl-c` | Quit from anywhere |
| `b`, `l`, `t`, `m` | Home, library, playlists, mixes |
| `N` | Now Playing, with cover art and spectrum |
| `Space` | Pause/resume; resume a restored session |
| `n` / `→`, `p` / `←` | Next / previous track |
| `+` / `-` | Volume |
| `[` / `]` | Seek backward / forward 10 seconds |
| `{` / `}` | Seek backward / forward 60 seconds |
| `g` | Jump to a timestamp (`mm:ss` or seconds) |
| `s`, `R` | Toggle shuffle; cycle repeat off/all/one |
| `Q` | Playback queue |
| `a`, `P` | Append selection; insert selection to play next |
| `Delete`, `J` / `K` | Remove / reorder in queue or playlist track list |
| `f` | Toggle track like, saved album, or followed artist |
| `w` | Choose a playlist to receive the selected track/collection |
| `c` | Create a playlist from the playlist browser/picker |
| `u` | Reload playlist/search; otherwise refresh favourite markers |
| `d`, `r` | Download selection; open selected track's radio |
| `?` | Show key bindings |

Seeking waits for the current file to finish buffering. Queue changes are local; playlist and favourite changes are sent to Tidal. Tidal permissions and subscription availability still apply. Errors appear in the footer; failed changes are not shown as successful.

Open Config from the home screen to change settings with `Enter`; booleans toggle immediately and theme modes cycle. Only folder paths need text entry. The standalone local-file player keeps its existing controls.

## Configuration

Run `lumitide config` to open the JSON file in your editor, or choose Config in the TUI. Settings are stored at
`~/.config/lumitide/config.json`.

| Field | Default | Description |
|-------|---------|-------------|
| `output_dir` | — | Directory for downloaded tracks (set on first run) |
| `volume` | `0.5` | Playback volume (0.0–1.0, saved across sessions) |
| `cover_size` | `640` | Album art fetch size in pixels |
| `search_limit` | `10` | Default number of search results |
| `drop_detection` | `true` | Enable beat/drop detection and color cycling |
| `always_color` | `true` | Always show album-art colors (not only during drops) |
| `theme` | `"auto"` | `auto`, `noctalia`, `pywal`, `terminal`, or `album` |
| `theme_file` | `null` | Optional generated colour JSON path, with `~/` support |
| `pywal` | `false` | Use [Pywal](https://github.com/dylanaraps/pywal) palette instead of album-art colors (reads `~/.cache/wal/colors.json` on all platforms) |
| `calm_mode` | `false` | Static spectrum shape, no drop/beat effects |
| `show_controls_hint` | `true` | Show "Press ? for ctrl" hint in the corner |

### Noctalia, Hyprland, and CachyOS colours

Hyprland itself does not prescribe a colour scheme. Lumitide follows the files produced by the desktop's theming tools. The default `"theme": "auto"` checks:

1. `theme_file`, if set.
2. `$NOCTALIA_CONFIG_DIR/colors.json`, then `$XDG_CONFIG_HOME/noctalia/colors.json` (normally `~/.config/noctalia/colors.json`), then the Noctalia cache directory.
3. Pywal's `colors.json` in the XDG cache or `~/.cache/wal/`.
4. The terminal's current ANSI palette.

Noctalia's `mPrimary`, `mSecondary`, `mTertiary`, `mSurface`, `mOnSurface`, and `mOnSurfaceVariant` colours are supported directly. Flat Material colour JSON and Matugen JSON exports are also accepted. Files are reloaded once per second; wallpaper/theme changes do not require restarting Lumitide. Malformed/missing files fall back safely.

If your Noctalia version themes Kitty/Alacritty/Foot rather than exporting `colors.json`, choose `"theme": "terminal"` to inherit its terminal palette. For another generated JSON file, set `"theme_file": "~/path/to/colors.json"`. No desktop files are modified and no hooks need to be installed. The legacy `pywal: true` setting still explicitly selects Pywal; disable it to use Noctalia auto-detection.

Album art retains its own colours with `"theme": "album"`; desktop palettes recolour the art and visualizer when available. Media keys and MPRIS metadata/progress remain available to Noctalia's media widget.

## Performance

Lumitide is built for low resource usage. Upstream figures below are reference values for the original player; they have not been remeasured for this fork:

| Metric | Idle | Playing |
|--------|------|---------|
| CPU | < 1% | 1–3% |
| RAM | ~5 MB | ~10 MB |

If RAM usage matters (e.g. on a low-power device), enabling `calm_mode` in config skips the beat/drop analysis thread entirely, keeping usage closer to the idle figure.

## Legal

Lumitide is an unofficial client. Use it only with a valid Tidal subscription and in accordance with [Tidal's Terms of Service](https://tidal.com/terms).

The app credentials bundled in `src/auth.rs` are not personal credentials. On Windows, Lumitide replicates the Tidal desktop app's PKCE auth flow to obtain lossless-capable tokens. On Linux/macOS it uses the same public client credentials as [python-tidal](https://github.com/tamland/python-tidal) and other open-source Tidal clients.

## License

MIT — see [LICENSE](LICENSE).
