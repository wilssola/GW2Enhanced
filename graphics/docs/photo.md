# Photo mode

A full-resolution capture of the current frame, on demand, with every mod effect. Purely visual: nothing here changes what
the player sees while playing, and nothing gives a gameplay advantage.

## Use

- Overlay (Shift+Home), Graphics tab, section **Photo mode**: **Capture screenshot**, **Compare with and without mod**, the checkbox
  **Hide the UI (3D scene only)** and a status line.
- Hotkey `photo_key` (default `Ctrl+Shift+End`, same syntax as `overlay_key`; read at start). The modifiers must match
  exactly, so `Shift+End` (the overlay of a graphics-only build) and `Ctrl+Shift+End` (a photo) never trigger each other, and the bare key never does
  anything. Works with the overlay closed, only while the game has the focus.
- Config: `photo_key`, `photo_hide_ui` (live through hot reload). `status_photo=` is in the overlay's Debug status and in the
  debug capture's `latest.txt`.

## Files (`GW2Graphics\photos\`)

| file | what |
|---|---|
| `gw2-<YYYYMMDD-HHMMSS>.png` | sRGB 8-bit, full back buffer size. HDR output: value / (paper white / 80), clipped at 1.0 = what an SDR screen shows (the same conversion as the debug capture, but not halved). The settings are also in a `tEXt` chunk ("Settings") |
| `gw2-<stamp>.hdr` | Radiance RGBE, linear light, **1.0 = paper white**, values above 1.0 kept (highlights up to the peak), BT.709 primaries. Flat (uncompressed) scanlines, 4 bytes per pixel (about 20 MB at 3440x1440): every Radiance reader understands them (Photoshop, GIMP, Blender, OpenCV, stb_image...) |
| `gw2-<stamp>.txt` | statistics of the picture (blow-out fractions, p99, mean, saturation) and every effective setting and status line, the same text as the debug capture |
| `gw2-<stamp>-mod.png/.hdr`, `-vanilla.png/.hdr`, `gw2-<stamp>.txt` | the comparison: the mod's picture and the mod's own vanilla emulation (what you see with Enabled unchecked), each with a `.hdr`; the `.txt` holds both settings and the difference |

The time stamp is UTC. If a name exists, `-2`, `-3`... is appended. **Nothing is ever deleted or overwritten** in this folder,
and there is no retention: the debug capture's clean-up only looks at `GW2Graphics\debug\shot-*` and `pair-*`.

The Radiance format has no colour-space tag; the `.hdr` is linear Rec.709 relative to paper white, which is also what the
`.txt` says. Under HDR10 output the capture is decoded from PQ/BT.2020 back to the same linear Rec.709 values.

## Which picture: with UI, or without

- **Default (with UI).** A copy of swapchain buffer 0 taken at present time, just before the overlay is drawn: the final picture
  including the game's own UI, without the mod's overlay (the same moment and the same staging ring as the debug capture; the
  render thread only issues a GPU copy and one memcpy, conversion, PNG and file writing run on a background thread).
- **`photo_hide_ui=1` (3D scene without the UI), single captures only.** The UI is drawn after the engine's pre-UI copy pass
  (`ps#c0ad1686`), so the UI-free scene is the texture that pass reads. The request arms a one-shot GPU copy; at the next draw
  of that pass the `Draw*` hook copies the texture bound as t0, AFTER the mod's passes (AA, lens, grading) bound their result,
  so the photo contains every mod effect (AO, reflections, shafts, contact shadows, aerial perspective, fur/rim, bloom, AA,
  grading) but no UI. That texture is display-referred (gamma encoded, before the final pass), so the final pass' maths
  (decode, energy bleach, tone shoulder, paper white, clamp at the peak) are applied on the CPU (`photo::final_pass_cpu`, tested
  against the shader's reference). Without HDR output the scene is clamped and decoded like the SDR final pass.
  If that pass is not seen within 90 presents, or the copy fails, the normal photo (with UI) is taken and the status line says
  why. **The comparison always includes the UI**: its two shots are taken at the present boundary, and a scene copy would need
  one more frame between forcing the mod off and restoring it.
- Limits: an MSAA back buffer or an unsupported format is skipped with a status message. Screens where the engine does not run
  that pre-UI copy pass (possibly the loading screen or character select) make `photo_hide_ui` fall back to the normal photo.

## One capture at a time

Photo requests wait in a queue (at most 3). `photo::arbitrate` (pure, tested) decides what the capture step does: a queued photo
waits while a pair (debug or photo) holds `Enabled` forced off or a scene copy is armed, and **while a photo is queued the debug
timer and the debug buttons do not start** (so a photo cannot be starved), and a running pair holds the next photo back.
A photo comparison is the debug pair state machine itself, with all its guarantees: the user's `Enabled`, paper white and peak are
restored after the vanilla shot, on any error, on a panic (`ForceGuard`), and by the watchdog thread 2 s after the force even if
the render loop stopped. If the mod is already off, a comparison request becomes a single shot.

## Disk space

Before writing, the writer thread reads the free space of the drive (`GetDiskFreeSpaceExW`). It writes only if more than
**200 MB plus the size of the files** is free; otherwise the photo is skipped and `status_photo=` says "not saved: skipped: only N
MB free on the drive, need more than M MB". If the free space cannot be read the photo is written anyway (a full disk then shows
as a write error in the status). Files are written to a `.tmp` name and renamed.

## Not included

Optional photo filters on the saved PNG are not implemented: the existing colour grading (section Color grading) is already in
the picture.

## Check in the game

1. Press `Ctrl+Shift+End` (overlay closed): `GW2Graphics\photos` gets `.png`, `.hdr`, `.txt`; the status line says `saved ...`.
2. Open the `.hdr` in an HDR-aware viewer: bright lights are above 1.0; the PNG is the clipped SDR look.
3. Tick **Hide the UI**, capture: no UI in the PNG, the effects (AO, grading...) are in; check `status_photo=` says "UI-free scene copy used".
4. **Compare with and without mod**: the screen flickers about 0.3 s, two shots are saved, your settings are back afterwards.
5. Press the hotkey several times quickly: they queue (status "photo queued (n waiting)"), none is lost, none overlaps.
