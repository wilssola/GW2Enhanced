# Interception

How the graphics feature sees the game's Direct3D 11 calls, and how to read its log when it does not. The code is in
`runtime/src/intercept.rs` (pure decisions), `d3d.rs` (the hooks), `ctxid.rs`, `recorder.rs`, `output.rs` and `diag.rs`.

## Design

- **Function-body detours, vtable patch as fallback.** For each method in the table of `intercept.rs` (17 methods: name,
  vtable slot, class) the function address is read once from the vtable of a live object and detoured inline with
  `retour`. A body that already starts with a `jmp` (another tool's hook) is handled. If the detour cannot be installed,
  the vtable slot is patched instead (`decide_how`). Methods whose bodies are shared with another method are not detoured
  twice (`plan_slot`).
- **Two detour slots per method.** A second class of objects (for instance a wrapper layer such as ReShade's) gets the
  second slot; every detour is a generic `h_xxx::<S>`, so each slot has its own address. A hook that runs inside another
  hook on the same thread (a wrapper calling the native object, or the mod's own calls) is passed through unrecorded
  (`Reentry`).
- **Classes are discovered from every device.** Hooks are installed from `D3D11CreateDevice`,
  `D3D11CreateDeviceAndSwapChain` (also with a NULL device out pointer: the device is then taken from the swapchain) and
  from every device that reaches a device hook or a factory `CreateSwapChain` (`discover_device`, once per device: its
  class, the class of its immediate context and its factory). So the game's own classes are found even if the creation
  hooks only saw another component's probe device.
- **No device identity.** Device hooks apply to every device outside the mod's own calls. The rules that pick what to
  change (render-target size equal to the screen, structural pass rules) exclude tool and probe devices by themselves.
  Substitute shaders are created on the device that owns the binding context and cached by (device, hash).
- **Contexts.** `GetType()` is asked once per distinct context pointer (bounded `VerdictTable`); deferred contexts are
  ignored. Each immediate context has its own recorder (up to 8). At the frame tick, the context with the most draws is
  the game's (tie: the first one); only that context's next frame can trigger the scene work.
- **The swapchain is chosen by window, not by device.** The game window is the visible top-level window of the process
  whose class starts with `ArenaNet` (the biggest one if there are several), cached once found. A swapchain on that
  window, from either creation path, is converted for HDR output when that is on ([hdr10.md](hdr10.md)); any failure
  retries once with the game's own description, so there is never a half-configured swapchain. `ResizeBuffers`
  recognises the game's swapchain by pointer or by window.

Hooked methods (vtable index): context `OMSetRenderTargets` (33), `OMSetRenderTargetsAndUnorderedAccessViews` (34),
`Draw` (13), `DrawIndexed` (12), `DrawInstanced` (21), `DrawIndexedInstanced` (20), `Dispatch` (41), `PSSetShader` (9),
`CopyResource` (47), `CopySubresourceRegion` (46), `ResolveSubresource` (57); device `CreateTexture2D` (5),
`CreateRenderTargetView` (9), `CreateShaderResourceView` (7), `CreatePixelShader` (15); factory `CreateSwapChain` (10);
swapchain `ResizeBuffers` (13).

## Reading the log

| Line | What it tells |
|---|---|
| `d3d: inline hook <m> at A ok (module M, detour slot N)` | A in `d3d11.dll`: the native class is hooked; in `dxgi.dll` or another module: a wrapper class is |
| `... failed: <error>` then `<m>: vtable slot N patched instead` | the body could not be detoured; the vtable fallback is used |
| hook lines with `detour slot 1` | two classes exist (native and wrapper, or a probe and the game) |
| `d3d: first <m> call this=.. type=0` | the game's immediate context reaches the hook (`type=1`: only deferred contexts do) |
| `hooks:` (every 5 s while the classifier does not match, 30 lines at most) | per method: `calls`, distinct `this`, how it is hooked, and the live `vtbl` / `slot` modules: a slot that points to another module than the mod's means it was overwritten after the mod |
| `immediate contexts (draws last frame): [..], classified` | which context carries the game's draws |
| `devices top3 CreateTexture2D / CreatePixelShader` | which device the game renders with, against probe devices |
| `d3d: <creation call> returned hr=.. swapchain=.. device=..` (first 40) | whether a creation failed, returned null outputs (a probe) or had no device out pointer |
| `output: game window 0x..`, `output: scRGB / SDR (...)` | the swapchain on the game window was found and decided |

`Dump frame` in the overlay always answers: without a recorded frame it writes the counters and
`no frame recorded yet (the hooks have not seen any render-target binding)`.

## Known limits

- An inline detour is more invasive than a vtable patch. The fallback covers a detour that cannot be installed, not a
  wrong relocation of one that was; after a Windows or driver update, start and quit the game once with the log open.
- `discover_device` runs inside the first call of a new device on the game's thread; it is guarded and panic-safe.
- Hooks are normally installed before rendering starts; installing one while another thread runs that function is not
  covered.
- If the first context class cannot hook `PSSetShader`, shader substitution stays off for the session.
- Substitute shaders of a destroyed device stay cached until the game exits.
