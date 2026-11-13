# stlcat

A compact Rust command-line viewer for STL files. It makes a shaded isometric
preview in memory and sends the resulting PNG using the [Kitty graphics
protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

## Install and use

```sh
cargo install --path .
stlcat model.stl
```

The terminal must support the Kitty graphics protocol (Kitty, WezTerm, or
Ghostty). `--width` and `--height` set the rendered pixel dimensions; `--cols`
and `--rows` set its terminal-cell footprint.

```sh
stlcat part.stl --width 1000 --height 700 --cols 90 --rows 35
```

### Rotation

Kitty 0.20+ can store animation frames and rotate the preview in the terminal:

```sh
stlcat part.stl --rotate --frames 36 --fps 12
```

`--frames` trades a smoother turntable motion for more terminal image memory
and a longer initial load. The animation remains terminal-driven after the CLI
exits.

The preview is automatically fitted into the current terminal's usable area.
It uses the terminal's reported cell-pixel dimensions to calculate matching
rows and columns, so the model is never stretched. On terminals that cannot
report these pixel metrics, it sends only the limiting dimension and lets Kitty
derive the other. Any explicit `--cols` or `--rows` values are also capped.

### Manual rotation

For a model viewer controlled from the keyboard, use:

```sh
stlcat part.stl --interactive --cols 90 --rows 35
```

Use Left/Right for horizontal rotation and Up/Down for vertical rotation. These
are fixed to the terminal view, so a key keeps the same meaning at every model
pose. Hold Shift with any arrow key to roll around the view axis. Press `q` or
Escape to close the viewer cleanly; its image is removed before the shell prompt
returns.

The model keeps a fixed scale while rotating. Press `+` (or `=`) to zoom in and
`-` to zoom out; zooming in is deliberately allowed to crop the model.

Interactive rotation includes a short original lightsaber-style swing (a
harmonic hum with Doppler pitch shift and a sweeping resonance) on each turn. Use `--mute` to disable it. On macOS
it uses the system audio player; on Linux it uses `aplay` when available.

It accepts both binary and ASCII STL. The preview stays in the terminal so it
can actually be viewed; it naturally scrolls away with terminal output. Use
`--delete-after` only for protocol testing, since it removes the preview before
some terminals have painted it.
