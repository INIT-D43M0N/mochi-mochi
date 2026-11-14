# Mochi Mochi

**Mochi Mochi** is an independent Wayland compositor and window manager written in Rust using [Smithay](https://github.com/Smithay/smithay).

* **License**: `GPL-3.0-or-later`
* **Configuration Format**: [KDL](https://kdl.dev/) (`config/mochi.kdl`) — **Live Hot-Reload Enabled**
* **Architectural Specification**: [docs/TECHNICAL_REPORT.md](docs/TECHNICAL_REPORT.md)

---

## Visual Style System (Real Offscreen FBO Multi-Pass Blur & Refraction)

Mochi Mochi is architected around three official visual languages selectable at runtime:
1. **`normal-blur`** — 13-tap Dual-Kawase background blur sampling the live offscreen FBO (blurring background windows and wallpaper underneath translucent windows).
2. **`frosted-glass`** — Diffused acrylic surfaces with `1.38x` background saturation bleed, anti-banding grain, 1px specular border, and dual-layer SDF drop shadows.
3. **`liquid-glass`** — Refractive optical surfaces with 3D SDF lens bevel normals, Snell's-law UV refraction, RGB chromatic dispersion of background windows, and cursor-reactive caustics.

---

## Running Mochi Mochi

Validate your KDL configuration:
```bash
./target/debug/mochi-compositor --check-config
```

### 1. Nested Development Mode (`--backend winit`)
```bash
./target/debug/mochi-compositor --winit -c "ptyxis --standalone"
```

### 2. Bare-Metal Hardware Mode (`--backend udev`)
```bash
./target/debug/mochi-compositor --udev -c "ptyxis --standalone"
```

---

## Live IPC Control CLI (`mochi-msg`)

While Mochi Mochi is running, you can inspect and control it from any terminal using `mochi-msg`:
```bash
./target/debug/mochi-msg status
./target/debug/mochi-msg style liquid-glass
./target/debug/mochi-msg style cycle
./target/debug/mochi-msg layout toggle
./target/debug/mochi-msg workspace 2
./target/debug/mochi-msg reload
./target/debug/mochi-msg spawn "ptyxis --standalone"
```

---

## Keybindings & Mouse Controls (`config/mochi.kdl`)

* **`Alt/Super + Return`**: Spawn a Wayland terminal inside Mochi Mochi
* **`Alt/Super + T`**: Toggle active workspace between **Floating** and **Master-Stack Auto-Tiling**
* **`Alt/Super + Shift + Return`**: Swap focused window with the Master pane in Auto-Tiling mode
* **`Alt/Super + [ / ]`**: Decrease / Increase the Master pane width ratio (`25%..75%`)
* **`Alt/Super + Shift + R`**: Hot-reload `config/mochi.kdl` immediately (also auto-reloads on file save!)
* **`Alt/Super + Left Mouse Drag`**: Move window (drag to screen edges or corners for live **Snap Preview**!)
* **`Alt/Super + Right Mouse Drag`**: Resize window directly from the nearest corner quadrant
* **`Alt/Super + Left / Right`**: Snap focused window to Left / Right half of the screen
* **`Alt/Super + Up / Down`**: Maximize / Center-restore focused window
* **`Alt/Super + F`**: Toggle fullscreen on focused window
* **`Alt/Super + Tab`**: Cycle focus to the next window in the active workspace
* **`Alt/Super + 1..4`**: Switch active workspace (1 to 4)
* **`Alt/Super + Shift + 1..4`**: Move focused window to workspace 1..4
* **`Alt/Super + S`**: Cycle active Visual Style (`normal-blur` → `frosted-glass` → `liquid-glass`)
* **`Alt/Super + Q`**: Close focused window
* **`Alt/Super + Escape`**: Quit Mochi Mochi
* **`Ctrl + Alt + F1..F12`**: Switch Linux virtual terminal (VT) in bare-metal mode
