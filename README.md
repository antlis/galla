# galla

A minimal image/video thumbnail gallery.

`galla` shows a grid of thumbnails for the images and videos in the paths you
give it (defaults to the current directory). Images open in a built-in viewer
with zoom and pan; videos are handed to a configurable external player
(`mpv` by default). It is a plain standalone viewer — nothing about it is tied
to any particular file manager.

## Usage

```
galla [--player CMD] [PATH ...]
```

- `PATH` may be image/video files or directories (scanned one level deep).
- With no path, the current directory is used.

### Keys

| Key            | Action                                   |
| -------------- | ---------------------------------------- |
| arrows         | move selection (grid) / prev-next (image)|
| `Enter`        | open (image → viewer, video → player)    |
| `y`            | copy selected file's path to clipboard   |
| `q` / `Esc`    | back to grid / quit                      |
| scroll, `+`/`-`| zoom (single image)                      |
| drag           | pan (single image)                       |
| `0`            | reset zoom                               |

Video thumbnails are generated with `ffmpegthumbnailer` and cached under
`$XDG_CACHE_HOME/galla` (falls back to `~/.cache/galla`).

## Configuring the video player

Resolved most-specific first:

1. `--player "CMD"` on the command line
2. `$GALLA_PLAYER`
3. `~/.config/galla/config.toml` — `player = "mpv --loop-file=no"`
4. default: `mpv`

The path of the chosen video is appended as the final argument.

## Dependencies

- `ffmpegthumbnailer` — video thumbnails
- a video player (`mpv` by default)

## License

MIT
