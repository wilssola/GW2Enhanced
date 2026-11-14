# Frame structure

How Guild Wars 2 (DirectX 11) renders one ordinary frame in the open world, as the mod's recorder sees it. The sizes
below are for a 3440x1440 screen; "full", "half" and "quarter" scale with the screen, the shadow maps do not.
The same frame is the test fixture `runtime/src/frame_fixture.rs` (`FRAME_PASSES`), so the classifier, the recorder and
the draw diagnostic are checked against it. What each post-processing shader does is in
[engine-post-chain.md](engine-post-chain.md).

A pass starts at each `OMSetRenderTargets` that changes the bound targets. The structure is stable from frame to frame
and across aspect ratios; only the draw counts change with the scene.

## The 21 passes

| # | Render target 0 | Depth | Draws | Role |
|---|---|---|---|---|
| 0 | none | 2048² `R24G8` | ~10 | shadow map |
| 1 | 512² `R16G16_FLOAT` | none | 2 | shadow helper (moments) |
| 2 | none | 6144x2048 `R24G8` | ~1500 | shadow cascade atlas (3 x 2048²) |
| 3 | full `B8G8R8A8_UNORM` | full `R24G8` | ~900 | **opaque scene**, forward lit, 8-bit |
| 4 | full `R32_FLOAT` | none | 1 | linear depth |
| 5 | half `R32_FLOAT` | none | 1 | half-size linear depth |
| 6 | half `R16G16_UNORM` | none | 3 | engine SSAO |
| 7 | full `R16_FLOAT` | none | 1 | AO blur / upsample |
| 8 | full `B8G8R8A8_UNORM` | none | ~50 | full-screen lighting pass (reads the linear depth) |
| 9 | full `B8G8R8A8_UNORM` | full `R24G8` | ~700 | **transparents and effects**, still 8-bit, before bloom |
| 10 | quarter `B8G8R8A8_UNORM` | none | 1 | bright-pass downsample |
| 11 | 215² `B8G8R8A8_UNORM` | none | 1 | second downsample |
| 12 | 1x1 `R16G16B16A16_FLOAT` | none | 1 | average luminance (light adaptation) |
| 13 | full `B8G8R8A8_UNORM` | none | 3 | light adjustment |
| 14 | quarter `B8G8R8A8_UNORM` | none | 5 | bloom blur |
| 15 | full `B8G8R8A8_UNORM` | none | 1 | combine (scene + bloom, grade) |
| 16 | full `B8G8R8A8_UNORM` | full `R24G8` | 3 | anti-aliasing / late full-screen effects |
| 17 | full `B8G8R8A8_UNORM` | none | 1 | copy (the UI starts after it) |
| 18 | full `B8G8R8A8_UNORM` | full `R24G8` | ~300 | **UI** |
| 19 | 512² `B8G8R8A8_UNORM` | 512² `R24G8` | ~25 | small off-screen render (icons, portrait) |
| 20 | back buffer `R8G8B8A8_UNORM` | full `R24G8` | 1 | final copy to the swapchain |

What matters for the mod:

- There is **no GBuffer**: no normal, roughness or specular buffer. Effects that need normals rebuild them from depth.
- Additive effects (#9) are blended in 8 bits **before** bloom, so the mod's float upgrade of the scene targets lets them
  go above white and feed the bloom.
- The engine already writes a **linear depth** (#4), with a shader-resource bind, which AO, aerial perspective, contact
  shadows and SSR read. Transparents are not in it.
- The shadow maps and their matrices are ordinary textures and constants, so volumetric light can sample them.
- Everything is found by structure (sizes, formats, order, draw counts; `classify.rs`), never by index or address.
  Shader hashes identify only the few engine shaders the mod replaces, and those change with game patches
  ([after-a-game-patch.md](after-a-game-patch.md)).

## Render targets of a session

The textures the game creates for rendering in a play session, grouped by kind (count x size, format). The full-size and
fractional ones are recreated at every resize, with the same layout.

| Kind | Targets |
|---|---|
| Scene colour | full `B8G8R8A8_UNORM` (several), full `R8G8B8A8_UNORM`, full `R16G16_FLOAT`, full `R8_UNORM` |
| Depth | full `R24G8`, 2048², 1024², 512², 256², 128², 6144x2048 (cascades) |
| Linear depth and AO | full `R32_FLOAT`, half `R32_FLOAT`, half `R16G16_UNORM`, full `R16_FLOAT` |
| Shadow helpers | 512² `R16G16_FLOAT`, 1024² / 2048² `R32_FLOAT` and `R32G32_FLOAT`, 6144x2048 `R32_FLOAT` |
| Post chain | quarter and 215² `B8G8R8A8_UNORM`, 1x1 `R16G16B16A16_FLOAT` |
| UI and small renders | 1024x768, 512x384, 256x192, 512², 256², 128² (`B8G8R8A8_UNORM`, some with `R32_FLOAT` / `R16_FLOAT` / `R16G16_UNORM` companions) |
| Swapchain | `R8G8B8A8_UNORM`, 1 buffer (the mod changes it for HDR output, [hdr10.md](hdr10.md)) |

## How to capture it again

After a game patch, the overlay's `Dump frame` writes the current frame in the same format to `GW2Graphics\`
(pass structure only, without the shader lines). Compare it with this table and the fixture; if the classifier no longer
matches, follow [after-a-game-patch.md](after-a-game-patch.md).
