# After a Guild Wars 2 patch

The graphics feature uses no fixed addresses. It finds the world frame by its pass structure and the engine's
shaders by the hash of their bytecode. A patch can change either. If it does, the classifier reports
`frame does not match` (or a substitute is simply never bound) and the HDR features stay off. In SDR the
game then renders as before: nothing breaks, the mod just does nothing until refreshed. With HDR output
(scRGB) it is not that harmless: if `afdc14cb` is missing or changed, the final conversion cannot run and the
picture is dark or wrong (the log has a `WARNING` and the overlay `Final pass:` line says so). Set
`hdr_output=off` until the hash is refreshed.

## 1. Check the pass structure

1. Start the game, load into the world, open the overlay's **Graphics** tab (`Shift+Home`;
   `Shift+End` in a graphics-only build), read the **Classifier** line.
   `frame matches` means the structure is still recognised. Anything else shows the reason.
2. Click **Dump frame** in the world. It writes `GW2Graphics\frame-passes-<unix seconds>.log` (pass
   structure only, no shader lines). The dump may contain the overlay's own draw pass: it is drawn
   at the start of the next frame, the same way every frame, and the classifier tolerates it. For a
   clean fixture delete that pass by hand (renumber or not: the parser ignores the numbers) or take
   the dump with the overlay window closed.
3. Compare with [frame-structure.md](frame-structure.md) and the fixture `runtime/src/frame_fixture.rs` (pass count, which
   passes are full size, quarter size, 1x1, the UI pass with full-size depth).
4. If the structure changed and the world is correct, save the new dump as a new fixture next to the
   old one (`frame-passes-<date>.log`), point the tests at it (`include_str!` in `classify.rs` and
   `recorder.rs`, and the screen size they use), and run:

   ```powershell
   cargo test --release -p gw2graphics classify
   ```

   If it fails, adapt the rules in `classify.rs` (they are described in [specs/graphics-m1-design.md](specs/graphics-m1-design.md), step 2) until the
   old and the new fixture both pass, or the new one replaces the old.

## 2. Check the shader hashes

The mod replaces four of the game's pixel shaders, matched by FNV-1a 32 of their DXBC:

| Hash | What |
|---|---|
| `f5cd4832` | tone/exposure remap (pass 13) |
| `2ed4a75f` | bright-pass downsample (passes 10, 11) |
| `14b1e90e` | bloom threshold (pass 14) |
| `afdc14cb` | final pass (only replaced with HDR output) |

```powershell
python -I graphics/tools/dxbc-dump/dxbc_dump.py index "G:\path\to\Gw2-64.exe"
```

Check that all four hashes are in the list. Then re-run the equivalence tests against the new exe:

```powershell
$env:GW2_EXE = "G:\path\to\Gw2-64.exe"
cargo test --release -p gw2graphics
```

They compare each replacement with the original blob on a software D3D11 device (in-range input
must match, values above 1.0 must pass through). They fail ("original blobs not all found") if one of the three shader hashes is gone from the exe.

## 3. A hash changed

1. The log tells which of the four is missing: after a minute in the world it should hold one
   `seen engine shader <hash>` line per hash (and `substitute applied <hash>` for `f5cd4832`,
   `2ed4a75f`, `14b1e90e` once `frame matches`, plus `afdc14cb` with HDR output). Find the new shader
   with `index` (list of hashes and sizes) and pick the candidates of a similar size; the pass
   it runs in is in `graphics/docs/engine-post-chain.md`.
2. Disassemble the old and the new one:

   ```powershell
   python -I graphics/tools/dxbc-dump/dxbc_dump.py disasm "G:\old\Gw2-64.exe" f5cd4832 > old.txt
   python -I graphics/tools/dxbc-dump/dxbc_dump.py disasm "G:\new\Gw2-64.exe" <newhash> > new.txt
   ```

3. Diff them. If only temporaries or ordering changed, the math is the same: update the hash constant
   (`hdr_shaders.rs`, `output.rs`) and the expected interface in `hdr_shaders_tests.rs`, re-run the
   tests with `GW2_EXE`.
4. If the math changed, re-derive the HLSL in `runtime/shaders` (`graphics/docs/engine-post-chain.md`
   explains what each stage does and why it is replaced), keep the same inputs, slots and constant
   buffer layout, and re-run the tests.
5. Until then, leaving the hash out is safe: that stage keeps its original shader (and clamps), so
   highlights are clipped at that stage but nothing else changes. This is true for the three clamping
   shaders and for SDR. It is NOT true for `afdc14cb` in scRGB: without its substitute the picture is dark or
   wrong (and screens that never run `afdc14cb`, such as loading screens, are dim). Use `hdr_output=off`
   until it is refreshed.

## Proxy exports

The feature has no exports of its own: it is part of `msimg32.dll`, whose 5 exports are forwarded to
`System32\msimg32.dll` by `enhanced/src/proxy.rs` (see
[the architecture of the mod](../../docs/specs/unified-mod-design.md#why-msimg32dll)). A game patch does not change
them. `graphics/tools/check-exports` belongs to the earlier standalone `wtsapi32.dll` build and does not apply to
`msimg32.dll`.
