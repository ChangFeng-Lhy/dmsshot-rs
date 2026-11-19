# dmsshot-rs

Native Rust screenshot utility for Wayland compositors that provide wlr screencopy and layer-shell.
It uses those protocols directly, so no `grim` or `slurp` process is required. Optional compositor
backends provide features that are not available through common Wayland protocols. niri and KDE
Plasma/KWin are supported.

## Relationship to DankMaterialShell

`dmsshot-rs` is an independent Rust rewrite of the screenshot module from
[DankMaterialShell (DMS)](https://github.com/AvengeMedia/DankMaterialShell). Its capture workflow,
region selector, scrolling capture, and stitching behavior are based on the original DMS
implementation. DankMaterialShell is developed by Avenge Media LLC and distributed under the MIT
License; see [LICENCE](LICENCE) for the applicable copyright and license notice.

## Build

```sh
cargo build --release
```

The resulting binary is `target/release/dmsshot-rs`. Building requires Rust 1.88 or newer, Wayland
development files, `pkg-config`, and `libxkbcommon`. At runtime, `wl-copy` provides persistent
Wayland clipboard ownership, `notify-send` sends the actionable completion notification, and
`xdg-open` opens captured images or their containing folders.
The `ocr` command uses PaddleOCR PP-OCRv4 Chinese/English ONNX models by default. Inference runs
locally through the Rust ONNX Runtime bindings; Python and PaddlePaddle are not required.

### Arch Linux package

The included `PKGBUILD` downloads pinned and checksummed PP-OCRv4 mobile detection,
orientation-classification, and Chinese/English recognition models from RapidOCR. It installs the
models under `/usr/share/dmsshot-rs/models` and the binary in `/usr/bin`:

```sh
makepkg -si
```

For a user-local installation in `~/.local/bin`, first build the package so that its model sources
are downloaded and verified, then install the binary and model files as the current user:

```sh
makepkg -s
install -Dm755 target/release/dmsshot-rs "$HOME/.local/bin/dmsshot-rs"
install -Dm644 ch_PP-OCRv4_det_infer.onnx \
  "$HOME/.local/share/dmsshot-rs/models/ch_PP-OCRv4_det_infer.onnx"
install -Dm644 ch_ppocr_mobile_v2.0_cls_infer.onnx \
  "$HOME/.local/share/dmsshot-rs/models/ch_ppocr_mobile_v2.0_cls_infer.onnx"
install -Dm644 ch_PP-OCRv4_rec_infer.onnx \
  "$HOME/.local/share/dmsshot-rs/models/ch_PP-OCRv4_rec_infer.onnx"
```

Make sure `$HOME/.local/bin` is present in `PATH`. A PKGBUILD intentionally cannot install directly
into an arbitrary user's home directory: pacman packages are installed as root and must own stable
system paths.

## Usage

```sh
dmsshot-rs                 # interactive region
dmsshot-rs magnifier       # magnifier and color picker, no screenshot
dmsshot-rs magnifier --zoom 4
dmsshot-rs full            # focused output when known, otherwise first output
dmsshot-rs all             # composite every output
dmsshot-rs output DP-1     # named output
dmsshot-rs window          # focused window (niri or KDE Plasma/KWin)
dmsshot-rs scroll          # interactive scrolling capture
dmsshot-rs ocr             # recognize text in an interactive region
dmsshot-rs region --magnifier --zoom 2
dmsshot-rs list            # output information
dmsshot-rs --be niri full  # force the niri backend
dmsshot-rs --be kde window # force the KDE/KWin backend
dmsshot-rs --float region  # keep the result in an always-on-top window
dmsshot-rs --pin full      # alias for --float
```

Run `dmsshot-rs --help` for image format, destination, cursor, JSON, and stdout options. Region mode
uses Enter or Space to confirm, Escape to cancel, `P` to toggle the captured pointer, and Shift to
constrain a drag to a square. Pass `--magnifier` to `region`, `scroll`, or `ocr` to show the cursor
magnifier and enable color copying with `C`; it is off by default. The standalone `magnifier`
subcommand shows the same loupe and color picker over an undimmed snapshot without taking a
screenshot; `C` or a left click copies the color and exits. Use `--zoom 1-5` to select the
magnification (default `2`). While the magnifier is visible, press `Tab` to cycle the color text
through `HEX`, `RGB`, and `HSL`; the default is `HEX`. Color text is always opaque; full-output
captures are already composited by the compositor, so visually transparent windows still produce
opaque colors.
Scroll mode starts as soon as the selection is released and finishes with Enter or the Done button;
if nothing was scrolled, the selected region is pinned as a normal capture. Successful file
captures are copied and notified by default; use `--no-clipboard` or `--no-notify` to disable either
integration. When niri is selected, the `window` command relies on niri's native
`ScreenshotWindow` action, which always sets the clipboard as an inherent compositor side effect,
including with `--no-clipboard`; the KDE window path does not have this side effect.
Left-clicking a completion notification opens the captured image; its Open Folder
action opens the destination directory. Binary `--stdout` output remains free of clipboard and
notification side effects.

Unless `--dir` or `$DMS_SCREENSHOT_DIR` specifies another destination, screenshots are saved in the
user's XDG Pictures directory under `Screenshots` (normally `~/Pictures/Screenshots`). The directory
and any missing parents are created automatically. If the XDG Pictures directory is not configured,
`~/Pictures/Screenshots` is used; only environments without a home directory fall back to the
current directory.

The default format is PNG and the generated filename is
`screenshot_YYYY-MM-DD_HH-MM-SS.png`, using local time. Selecting JPEG or PPM changes the filename
extension accordingly, while `--filename` overrides the generated name.

## Floating preview

Pass `--float` (alias `--pin`) to keep the captured image in an always-on-top window after the
save, clipboard, path, and JSON steps complete. It works with `region`, `scroll`, `full`, `all`,
`output`, and `window`; `ocr`, `magnifier`, and `list` reject it, and it conflicts with `--stdout`.
The process stays in the foreground until the window closes and then exits successfully. Desktop
notifications are suppressed while the floating window is visible because the window itself is the
feedback; clipboard copying is unchanged.

The window opens at the original selection for `region` and `scroll`, centered on the captured
output for `full` and `output`, and centered on the focused output for `all` and `window` when the
backend can report it (otherwise the first output). At 100% the preview is rendered at the target
output's physical pixel density, so a capture and display scale of 1.5 maps pixels 1:1 instead of
being upscaled from a logical-size buffer; the initial size is capped at 90% of the output.
Left-drag moves the image freely across outputs, the wheel zooms between 10% and 400% around the
window center, `O` opens the saved file, `F` opens its folder, and Escape, double-click, or
middle-click closes the window. A percentage badge appears whenever the zoom is not 100%.

The window uses a layer-shell Overlay surface with 12px rounded corners, a theme-colored border,
and a 20px shadow. Only the image rectangle accepts pointer input, so clicks on the shadow fall
through to the application below. KWin 6.8 exposes `zwlr_layer_shell_v1`; on compositors without
layer-shell the command fails instead of silently showing a window that may not stay on top.

## Wayland and compositor backends

Output discovery uses `wl_output` and `xdg-output`; screenshot pixels use
`wlr-screencopy-unstable-v1`. Region, scrolling, and OCR selection use layer-shell, `wl_shm`,
viewporter, and `wl_seat`. These commands no longer require `NIRI_SOCKET`.

niri is detected automatically and used as an optional backend for focused-output discovery and
focused-window capture.

Backend selection is automatic by default: dmsshot-rs prefers the common wlr-screencopy path when
it is available, then falls back to KWin ScreenShot2. Use the global `--be niri` or `--be kde`
option to force one compositor backend. Forced niri requires `NIRI_SOCKET` and
`wlr-screencopy-unstable-v1`; forced KDE requires the KWin ScreenShot2 D-Bus service. An unavailable
forced backend is reported as an error instead of silently falling back. The option may appear
before or after the command.

On KDE Plasma Wayland, dmsshot-rs automatically uses KWin's
`org.kde.KWin.ScreenShot2` D-Bus interface when wlr screencopy is unavailable. `full` captures the
active screen, `all` captures the complete workspace, `output` captures a named screen, and
`window` captures the active window. Region and OCR retain the dmsshot-rs overlay; scrolling uses
KWin area captures and requires ScreenShot2 version 5 or newer so KWin can hide the overlay owned
by dmsshot-rs.

KWin restricts this screenshot interface to installed desktop applications. The PKGBUILD installs
the executable as `/usr/bin/dmsshot-rs` and installs a root-owned
`/usr/share/applications/dmsshot-rs.desktop` whose absolute `Exec` path and
`X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2` declaration authorize that exact
executable. Run the installed `/usr/bin/dmsshot-rs` on Plasma: an unpackaged `./dmsshot-rs`, a
binary under `target/`, or a copy under `~/.local/bin` intentionally does not inherit this
permission. For a manual system installation, use:

```sh
sudo install -Dm755 target/release/dmsshot-rs /usr/bin/dmsshot-rs
sudo install -Dm644 data/dmsshot-rs.desktop /usr/share/applications/dmsshot-rs.desktop
kbuildsycoca6
```

KWin screenshots are returned as raw pixels and do not launch Spectacle or an
xdg-desktop-portal dialog.

Without a compositor backend, `full` falls back deterministically to the first output sorted by
name, while `window` returns a clear unsupported-backend error. More compositor backends can be
added under `src/backend/` without changing the overlay, OCR, or stitching layers.

## OCR

`dmsshot-rs ocr` opens the normal region selector and passes the selected RGBA pixels directly to a
local PaddleOCR pipeline: DBNet text detection followed by PP-OCRv4 Chinese/English recognition.
It does not save a temporary screenshot file. Detected blocks are restored to reading order and
artificial spaces between Chinese characters are removed. Recognized text is printed to stdout and
copied to the text clipboard by default; use `--no-clipboard` or `--no-notify` to disable either
integration. `--json` returns `status`, `text`, `width`, `height`, `engine`, and `language`.

Models are searched in `--model-dir`, `DMSSHOT_OCR_MODEL_DIR`, the XDG user data directory, the
installation prefix, and `/usr/share/dmsshot-rs/models`, in that order. A custom model directory
must contain `ch_PP-OCRv4_det_infer.onnx`, `ch_ppocr_mobile_v2.0_cls_infer.onnx`,
and `ch_PP-OCRv4_rec_infer.onnx`. The recognition model contains its matching character table:

```sh
dmsshot-rs ocr
dmsshot-rs ocr --model-dir /path/to/models
dmsshot-rs ocr --json
```

Tesseract remains available as an explicit compatibility fallback. Its language names must match
`tesseract --list-langs`; `--lang` and `--psm` apply only to this engine:

```sh
sudo pacman -S --needed tesseract tesseract-data-chi_sim tesseract-data-eng
dmsshot-rs ocr --engine tesseract --lang chi_sim+eng
```

The ONNX model files are converted from PaddleOCR models and distributed by
[RapidOCR](https://github.com/RapidAI/RapidOCR). PaddleOCR and its models are provided by
[PaddlePaddle](https://github.com/PaddlePaddle/PaddleOCR) under the Apache-2.0 license; the Rust
integration uses [paddle-ocr-rs](https://github.com/mg-chao/paddle-ocr-rs) under the Apache-2.0
license.

## GTK and Qt theme support

The selector overlays and the floating preview use GTK colors by default and switch to Qt colors
automatically in a KDE session (`XDG_CURRENT_DESKTOP` contains `KDE`). Pass `--gtk` to select GTK
explicitly or `--qt` to use Qt colors; the two options are mutually exclusive and may be placed
before or after the command.

GTK theme discovery uses `GTK_THEME` first and then
`org.gnome.desktop.interface gtk-theme` from `gsettings`. Both GTK 3 and GTK 4 CSS files are
supported, including themes installed in the standard XDG data directories, `~/.themes`, and user
overrides under `$XDG_CONFIG_HOME/gtk-3.0` or `$XDG_CONFIG_HOME/gtk-4.0`.

The CSS reader understands common GTK color definitions and aliases such as `window_bg_color`,
`window_fg_color`, `accent_color`, and `theme_selected_bg_color`.

Qt discovery checks qt6ct first, then qt5ct, and finally KDE's `kdeglobals` and referenced `.colors`
files. It understands qtct `active_colors` palettes and KDE `Colors:Window`, `Colors:View`, and
`Colors:Selection` groups. GTK and Qt are not build-time or runtime library dependencies. If the
selected source cannot be found or does not expose the required colors, the overlay uses its
built-in dark color scheme.

Examples:

```sh
GTK_THEME=Adwaita:dark dmsshot-rs --gtk
dmsshot-rs --qt region
```
