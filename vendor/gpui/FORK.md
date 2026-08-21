# Fork delta vs gpui 0.2.2 (crates.io)

One capability, four seams. Upstream gpui has no per-element backdrop filter
and no shader hook, so real liquid glass — sampling and refracting what is
behind a panel — was impossible from app code. This fork adds a single new
primitive and nothing else; upgrades should re-carry these patches:

- `src/scene.rs` — `BackdropGlass` primitive (order/bounds/radii/mask/tint +
  blur radius), a `PrimitiveKind::BackdropGlass` placed **last** so the
  capture sorts after everything it captures, and the batch-iterator plumbing.
- `src/window.rs` — `Window::paint_backdrop_glass(bounds, radii, blur, tint)`,
  shaped exactly like `paint_quad`.
- `src/platform/mac/metal_renderer.rs` — the batch arm splits the render pass
  (same dance the Paths batch already does), blits the drawable into a capture
  texture, runs a two-pass separable gaussian into a half-resolution
  ping-pong pair, then reopens the encoder and draws the glass quads.
  `framebufferOnly` is turned off on the CAMetalLayer because reading the
  drawable back is the whole point.
- `src/platform/mac/shaders.metal` — `backdrop_glass_vertex/fragment`
  (rounded-rect SDF mask, refraction along the SDF gradient in a band one
  blur-radius wide, tint composite) and `glass_blur_vertex/fragment`
  (bufferless fullscreen triangle, linear-tap gaussian).
- `build.rs` — the three new `#[repr(C)]` types added to cbindgen's export
  list so the Metal side sees them.

Non-Metal backends (blade, DirectX) do NOT have arms for the new batch kind;
they are not compiled by this workspace, and the non-exhaustive match is the
compile-time reminder if that ever changes.
