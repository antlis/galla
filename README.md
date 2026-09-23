# galla

**A minimal, keyboard-driven image & video thumbnail gallery for the desktop.**

![License](https://img.shields.io/badge/license-MIT-blue)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)
![Platform](https://img.shields.io/badge/platform-Linux%20(X11%20%7C%20Wayland)-lightgrey)

`galla` shows a grid of thumbnails for the images and videos in the paths you
give it (defaults to the current directory). Images open in a built-in viewer
with zoom and pan; videos are handed to a configurable external player
(`mpv` by default). It is a plain standalone tool — nothing about it is tied to
any particular file manager, so it works just as well from a shell, a launcher,
or as an `xdg` image handler.

## Features

- **Grid of thumbnails** for images and videos, generated in the background.
- **Video thumbnails** via `ffmpegthumbnailer`, cached and invalidated on change.
- **Built-in viewer** with scroll/`+`/`-` zoom, drag-to-pan, and reset.
- **Vim-style navigation** (`hjkl`) alongside the arrow keys.
- **Videos play in your player** of choice (`mpv` by default, configurable).
- **Copy** the file path *or* the image itself to the clipboard.
- **Drag-and-drop** a file into any other app (Telegram, browsers, file
  managers) via [`dragon-drop`](https://github.com/mwh/dragon).
- **In-app help overlay** (`?`) and toast feedback for clipboard/drag actions.

## Install

Build from source with Cargo:

```sh
git clone https://github.com/antlis/galla
cd galla
cargo build --release
# binary at target/release/galla
```

Make sure the runtime dependencies below are on your `PATH`.

## Usage

```
galla [--player CMD] [--drag CMD] [PATH ...]
```

- `PATH` may be image/video files or directories (scanned one level deep).
- With no path, the current directory is used.
- Passing a single image starts directly in the viewer.

### Keys

| Key             | Action                                    |
| --------------- | ----------------------------------------- |
| arrows / `hjkl` | move selection (grid) / prev-next (image) |
| `Enter`         | open (image → viewer, video → player)     |
| `y`             | copy selected file's path to clipboard    |
| `Y`             | copy the image itself to the clipboard    |
| `d`             | drag-and-drop the file into another app   |
| `?`             | toggle the keybinding help overlay        |
| `q` / `Esc`     | back to grid / quit                       |
| scroll, `+`/`-` | zoom (single image)                       |
| mouse drag      | pan (single image)                        |
| `0`             | reset zoom                                |

Video thumbnails are generated with `ffmpegthumbnailer` and cached under
`$XDG_CACHE_HOME/galla` (falls back to `~/.cache/galla`).

## Configuring the video player

Resolved most-specific first:

1. `--player "CMD"` on the command line
2. `$GALLA_PLAYER`
3. `~/.config/galla/config.toml` — `player = "mpv --loop-file=no"`
4. default: `mpv`

The path of the chosen video is appended as the final argument.

## Configuring drag-and-drop

The `d` key hands the selected file to an external drag-and-drop source so you
can drag it into another app (Telegram, a browser, a file manager…). Resolved
most-specific first, exactly like the player:

1. `--drag "CMD"` on the command line
2. `$GALLA_DRAG`
3. `~/.config/galla/config.toml` — `drag = "dragon-drop --and-exit"`
4. default: `dragon-drop --and-exit`

The file path is appended as the final argument.
[`dragon-drop`](https://github.com/mwh/dragon) provides the `dragon-drop` binary.

See [`config.example.toml`](config.example.toml) for a starting config.

## Dependencies

- `ffmpegthumbnailer` — video thumbnails
- a video player (`mpv` by default)
- a drag-and-drop source (`dragon-drop` by default) — only for the `d` key

## License

[MIT](LICENSE)
