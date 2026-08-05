//! wgpu (Metal) present backend.
//!
//! The guest thread publishes [`SurfaceFrame`]s (0RGB u32, top-down); this
//! module uploads them to a persistent staging texture and blits to the
//! window's CAMetalLayer-backed surface.
//!
//! Why a blit pass instead of `write_texture` straight into the surface?
//! Guest pixels are 0RGB with the alpha byte ALWAYS 0. wgpu honors alpha, so a
//! direct upload would present a fully transparent window (the AppKit path
//! must force alpha opaque). The blit forces `alpha = 1.0` and gives
//! dirty-region uploads real savings: the staging texture persists across
//! frames, so only the dirty region is re-uploaded.
//!
//! All wgpu calls are safe — no `unsafe` lives outside wie-cpu.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context, Result};
use wie_winapi::gdi32::IRect;
use wie_winapi::present::SurfaceFrame;
use winit::window::Window;

/// Fullscreen-triangle blit: sample the staging texture and write opaque RGB.
///
/// The fragment output must force alpha to 1.0 — guest pixels carry alpha=0
/// (0RGB), and without the override the window would be fully transparent.
/// Nearest sampling gives the same scaling semantics as the CPU
/// `stretch_nearest` when the window size differs from the frame size.
const BLIT_SHADER: &str = r#"
@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

struct VsOut {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    // Three vertices cover the whole viewport with no vertex buffer: idx 0 →
    // (-1,-1), idx 1 → (3,-1), idx 2 → (-1,3). The x multiplier must be 4.0
    // (not 2.0): with 2.0 the triangle is (-1,-1),(1,-1),(-1,3), which leaves
    // the top-right corner of the viewport uncovered (black triangle).
    let pos = vec2f(
        f32(idx & 1u) * 4.0 - 1.0,
        f32(idx & 2u) * 2.0 - 1.0,
    );
    var out: VsOut;
    out.position = vec4f(pos, 0.0, 1.0);
    // Clip-space y grows upward but texture v=0 is the first (top) row, so
    // flip y to sample the texture top-down.
    out.uv = vec2f((pos.x + 1.0) * 0.5, (1.0 - pos.y) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let c = textureSample(frame_tex, frame_sampler, in.uv);
    return vec4f(c.rgb, 1.0);
}
"#;

/// The persistent guest-frame staging texture and its GPU-side bindings.
///
/// Recreated only when the guest frame's dimensions change. The sampler is
/// shared (created once in [`WgpuPresenter::init`]); the bind group references
/// the per-texture view plus that shared sampler. The view itself is not
/// stored — the bind group keeps it alive inside wgpu.
struct StagingFrame {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Whether a [`WgpuPresenter::present`] call actually drew the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentOutcome {
    /// The frame was uploaded and presented to the swapchain.
    Drawn,
    /// The frame was NOT drawn — the surface acquire skipped it. `retry` is
    /// `true` for transient skips (occluded / out-of-date / lost surface) that
    /// a re-requested redraw can resolve; `false` for persistent failures
    /// (validation) where re-requesting would spin. The app bounds the
    /// re-request to once per presentable frame (`RetryBudget` in app.rs), so
    /// a persistently-`retry: true` skip cannot spin the event loop.
    NotDrawn { retry: bool },
}

/// wgpu/Metal present backend for one winit window.
///
/// Owns an [`Arc<Window>`] (alongside the app's copy) so the `Surface` can be
/// `'static` — wgpu keeps the raw window handle alive internally, avoiding a
/// self-referential struct in `WieApp`.
pub(crate) struct WgpuPresenter {
    window: Arc<Window>,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Current surface configuration; width/height are updated on resize.
    /// The pixel format is baked into this config and the pipeline.
    config: wgpu::SurfaceConfiguration,
    /// Fullscreen-triangle blit pipeline (layout from the explicit bind-group
    /// layout below).
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    /// Nearest clamp-to-edge sampler shared by every staging bind group.
    sampler: wgpu::Sampler,
    /// `max_texture_dimension_2d` of the requested device limits — the swap
    /// chain width/height are clamped to this so `surface.configure` can never
    /// fail validation on an absurd window size.
    max_tex_dim: u32,
    /// Guest-frame staging texture (frame-sized), recreated on size change.
    staging: Option<StagingFrame>,
    /// The frame content currently staged in `staging` — the pixels Arc plus
    /// the frame size. A present whose surface acquire skipped the frame is
    /// retried by the app (it re-requests the redraw) with the SAME pixels
    /// Arc; the Arc + size compare skips the re-upload on that retry so the
    /// event-loop thread is not pinned re-copying a frame it already holds.
    /// The compare is sound because this entry keeps the compared-to
    /// allocation alive (the same keep-alive contract as the app's
    /// `last_presented_pixels`) and the size check covers a staging
    /// recreation.
    last_uploaded: Option<(Arc<Vec<u32>>, u32, u32)>,
    /// Generation-gap fallback: `true` when the staging texture may NOT hold
    /// the full previous frame, so the NEXT upload must be a full-frame copy
    /// regardless of the frame's region. A frame's region is relative to the
    /// previous PUBLISH — if that publish's present was skipped (`NotDrawn`)
    /// or failed, its delta was never staged, and a region-limited upload
    /// would permanently lose it (the exact bug that killed the old
    /// region-delta contract). Set by any present that did not draw; cleared
    /// once a full upload (or a same-frame upload skip) completes.
    force_full: bool,
}

impl WgpuPresenter {
    /// Create the Metal instance, surface, device, and blit pipeline.
    ///
    /// The instance is restricted to the Metal backend (macOS-only policy).
    /// The two async requests (adapter, device) block via `pollster` — wie-cli
    /// has no async runtime.
    pub(crate) fn init(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });
        // `Arc<Window>` implements HasWindowHandle + HasDisplayHandle (rwh_06),
        // so create_surface yields a 'static surface — the presenter owns a
        // window clone and wgpu keeps the handle alive internally.
        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("request wgpu adapter")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            // Full device limits, NOT downlevel_defaults: that set caps
            // `max_texture_dimension_2d` at 2048, which fails
            // `surface.configure` validation for full-screen windows (e.g.
            // 2094×1366 physical pixels on a 15" MacBook). The default set
            // allows 8192 — plenty for any macOS display.
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .context("request wgpu device")?;
        let max_tex_dim = device.limits().max_texture_dimension_2d;

        // Validation errors (e.g. a mis-sized configure) otherwise translate
        // into a panic via wgpu's default fatal handler. Log them instead —
        // the present loop skips/reconfigures and keeps running.
        device.on_uncaptured_error(Arc::new(|e| {
            tracing::warn!(target: "wiegui", error = %e, "wgpu uncaptured error");
        }));

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .or_else(|| caps.formats.first().copied())
            .context("surface advertises no formats")?;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // Guest pixels are already sRGB-encoded; Auto resolves to sRGB for
            // 8-bit formats, which matches how they were produced.
            color_space: wgpu::SurfaceColorSpace::Auto,
            // Clamp to the device's 2D texture limit so configure can never
            // fail validation on an absurd window size.
            width: window.inner_size().width.max(1).min(max_tex_dim),
            height: window.inner_size().height.max(1).min(max_tex_dim),
            // Fifo = vsync, the blocking-present equivalent.
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: Vec::new(),
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wie present blit shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(BLIT_SHADER)),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wie present blit layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wie present blit pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("wie present nearest sampler"),
            // Nearest = identical scaling semantics to the CPU `stretch_nearest`.
            // Clamp-to-edge: sampling past the last
            // pixel row/column repeats the edge, matching a clamped blit.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 0.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wie present blit pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // No blending: the shader outputs opaque colors.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            window,
            instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            max_tex_dim,
            staging: None,
            last_uploaded: None,
            force_full: false,
        })
    }

    /// Re-create the surface (e.g. after `CurrentSurfaceTexture::Lost`) and
    /// reconfigure it at the current window size.
    fn recreate_surface(&mut self) -> Result<()> {
        self.surface = self
            .instance
            .create_surface(self.window.clone())
            .context("recreate wgpu surface")?;
        self.surface.configure(&self.device, &self.config);
        Ok(())
    }

    /// Reconfigure the surface for a new window size. The staging texture
    /// stays frame-sized — the blit pass nearest-scales. The size is clamped
    /// to the device's 2D texture limit so an absurd size can never trip
    /// `surface.configure` validation.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1).min(self.max_tex_dim);
        self.config.height = height.max(1).min(self.max_tex_dim);
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload `frame` (region or full) to the staging texture and blit it to
    /// the surface. `window_w/h` is the current client size, used only when the
    /// surface must be reconfigured (`Outdated`) or recreated (`Lost`).
    ///
    /// Returns whether the frame actually reached the swapchain. A surface
    /// acquire that skips the draw (`Timeout`/`Occluded`/`Outdated`/`Lost`/
    /// `Validation`) reports `NotDrawn` instead of pretending the frame was
    /// presented — the app must NOT record a skipped frame as presented, or
    /// its per-window `Arc::ptr_eq` skip would reject the retry of that same
    /// frame (and a modal dialog that publishes exactly once would stay
    /// invisible until the next repaint).
    ///
    /// The upload copies only the frame's `region` hint (a partial repaint,
    /// e.g. a caret blink) onto the persistent staging texture; the
    /// fullscreen-triangle blit then samples the whole texture. Any present
    /// that did NOT draw arms the generation-gap fallback, so the next upload
    /// is full — the region delta must never be applied to a staging texture
    /// that missed a skipped frame.
    pub(crate) fn present(
        &mut self,
        frame: SurfaceFrame,
        window_w: u32,
        window_h: u32,
    ) -> Result<PresentOutcome> {
        // Generation-gap fallback: a frame whose present did NOT draw (acquire
        // skip, or an upload error) leaves the staging possibly missing that
        // frame's delta. The next frame's region is relative to the previous
        // PUBLISH, not the last successfully-staged frame, so a region-limited
        // upload would lose the gap permanently (the exact bug that killed the
        // old region-delta contract). Force a full upload next.
        let outcome = self.present_inner(frame, window_w, window_h);
        if !matches!(outcome, Ok(PresentOutcome::Drawn)) {
            self.force_full = true;
        }
        outcome
    }

    /// The upload + draw core of [`Self::present`]; the caller arms the
    /// generation-gap fallback when the draw did not happen.
    fn present_inner(
        &mut self,
        frame: SurfaceFrame,
        window_w: u32,
        window_h: u32,
    ) -> Result<PresentOutcome> {
        // Consume the frame up front: the pixel Arc is only needed for the
        // staging upload below, and `write_texture` copies it synchronously.
        // Taking ownership (instead of borrowing) lets us drop the Arc right
        // after the upload — before the render pass — shrinking the host hold
        // window so the guest's next `ensure_surface` hand-back can
        // `Arc::try_unwrap` the published buffer zero-copy instead of cloning.
        let SurfaceFrame {
            width: frame_w_raw,
            height: frame_h_raw,
            pixels,
            background_color,
            region,
        } = frame;
        let frame_w = frame_w_raw.max(1);
        let frame_h = frame_h_raw.max(1);

        // Recreate the staging texture when the guest frame size changed. A
        // fresh texture is blank — the upload must cover it fully.
        if self
            .staging
            .as_ref()
            .is_none_or(|s| s.width != frame_w || s.height != frame_h)
        {
            self.staging = Some(self.make_staging(frame_w, frame_h)?);
            self.force_full = true;
        }
        let staging = self.staging.as_ref().context("staging texture")?;

        // Skip the re-upload when the staging texture already holds this exact
        // frame: a present whose acquire skipped is retried with the same
        // pixels Arc, and re-copying ~4 MB per retry would pin the event-loop
        // thread. The size check covers a staging recreation (the new texture
        // is blank, so a size change forces the copy). When the staging holds
        // this frame, a generation-gap flag set by a skipped present is moot.
        let pixel_arc = Arc::clone(&pixels);
        let already_staged = self.last_uploaded.as_ref().is_some_and(|(prev, w, h)| {
            *w == frame_w && *h == frame_h && Arc::ptr_eq(prev, &pixel_arc)
        });
        if already_staged {
            self.force_full = false;
        } else {
            // A generation gap forces the whole frame; otherwise upload only
            // the published region (None region = full frame).
            let upload_region = if self.force_full { None } else { region };
            let upload = upload_source(&pixels, frame_w, frame_h, upload_region);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &staging.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: upload.origin_x,
                        y: upload.origin_y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                upload.bytes.as_ref(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(upload.bytes_per_row),
                    rows_per_image: Some(upload.rows),
                },
                wgpu::Extent3d {
                    width: upload.width,
                    height: upload.height,
                    depth_or_array_layers: 1,
                },
            );
            // wgpu copied the pixels into the staging buffer synchronously; the
            // render pass below only samples the staging texture. Release the
            // guest Arc now instead of holding it across the render + present.
            self.last_uploaded = Some((pixel_arc, frame_w, frame_h));
            if upload_region.is_none() {
                // A full upload completed — the staging equals the frame.
                self.force_full = false;
            }
        }
        drop(pixels);

        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(stex)
            | wgpu::CurrentSurfaceTexture::Suboptimal(stex) => {
                let view = stex
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("wie blit to surface"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                // Clear to the owning window's background
                                // color (recorded on the frame by the erase
                                // machinery; falls back to COLOR_WINDOW-white):
                                // untouched areas of the window (e.g. behind
                                // an unmaximized guest frame, or resize seams
                                // the frame does not yet cover) read as the
                                // window background rather than black. The
                                // whole surface is redrawn every frame and the
                                // swapchain target is transient, so a clear is
                                // cheaper than loading the previous frame's
                                // contents.
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: color_channel(background_color, 16),
                                    g: color_channel(background_color, 8),
                                    b: color_channel(background_color, 0),
                                    a: 1.0,
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &staging.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
                self.queue.submit(Some(encoder.finish()));
                // wgpu 30 moved present from `SurfaceTexture::present()` to
                // `Queue::present(&self, stex)` — same operation.
                self.queue.present(stex);
                Ok(PresentOutcome::Drawn)
            }
            // Nothing to draw — the frame is NOT presented. Report it so the
            // caller re-requests the redraw and retries this same frame; the
            // upload-skip above makes the retry cheap.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                tracing::debug!(target: "wiegui", "surface acquire skipped (timeout/occluded)");
                Ok(PresentOutcome::NotDrawn { retry: true })
            }
            // The surface geometry changed; reconfigure and let the retried
            // redraw re-acquire.
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.resize(window_w, window_h);
                Ok(PresentOutcome::NotDrawn { retry: true })
            }
            // Lost — recreate the surface from the window and reconfigure.
            wgpu::CurrentSurfaceTexture::Lost => {
                self.recreate_surface()?;
                Ok(PresentOutcome::NotDrawn { retry: true })
            }
            // Persistent misconfiguration: re-requesting would spin forever.
            wgpu::CurrentSurfaceTexture::Validation => {
                tracing::debug!(target: "wiegui", "surface acquire validation error; skipping frame");
                Ok(PresentOutcome::NotDrawn { retry: false })
            }
        }
    }

    /// Create a staging texture at `width`×`height` plus its view and bind
    /// group. `Bgra8Unorm`: guest pixels are 0RGB u32s whose little-endian
    /// memory bytes are B,G,R,0 — exactly a BGRA8 texel row, so the zero-copy
    /// upload stays valid and the R channel receives the guest's R. (An
    /// RGBA8 staging here would swap R/B: guest blue #0078D7 rendered as
    /// orange.)
    fn make_staging(&self, width: u32, height: u32) -> Result<StagingFrame> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wie guest frame staging"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wie guest frame staging bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Ok(StagingFrame {
            width,
            height,
            texture,
            bind_group,
        })
    }
}

/// The source bytes + copy layout for one `write_texture` call.
struct UploadSource<'a> {
    /// Source bytes. `Cow::Borrowed` for a full frame whose natural pitch is
    /// already 256-aligned (zero-copy); `Cow::Owned` for a padded row pack
    /// otherwise (region uploads and unaligned full frames).
    bytes: Cow<'a, [u8]>,
    /// Row stride in `bytes` — padded to wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT`
    /// (256), which `write_texture` requires.
    bytes_per_row: u32,
    /// Number of rows in `bytes`.
    rows: u32,
    /// Destination origin and extent inside the staging texture.
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
}

/// Build the source buffer for a `write_texture` of `region` (None = full
/// frame) out of the frame's `pixels` (0RGB u32, top-down).
///
/// Rows are packed into a buffer with the pitch padded to wgpu's 256-byte
/// alignment — a region narrower than the full frame (e.g. a caret blink)
/// must not stride by the full frame width, and the pitch must be aligned.
/// The full-frame case with an already-aligned pitch is zero-copy (the `Cow`
/// borrows the pixels buffer directly).
#[must_use]
fn upload_source<'a>(
    pixels: &'a [u32],
    frame_w: u32,
    frame_h: u32,
    region: Option<IRect>,
) -> UploadSource<'a> {
    let frame_w_u = usize::try_from(frame_w).unwrap_or(0);
    let frame_w_i = i32::try_from(frame_w).unwrap_or(0);
    let frame_h_i = i32::try_from(frame_h).unwrap_or(0);
    let (left, top, right, bottom) = match region {
        None => (0, 0, frame_w_i, frame_h_i),
        // Defensive clip: the winapi side clips the dirty rect to the
        // surface, but a malformed region must not over-read the buffer.
        Some(r) => (
            r.left.max(0),
            r.top.max(0),
            r.right.min(frame_w_i),
            r.bottom.min(frame_h_i),
        ),
    };
    let width = right.saturating_sub(left).max(0);
    let height = bottom.saturating_sub(top).max(0);
    let width_us = usize::try_from(width).unwrap_or(0);
    let height_us = usize::try_from(height).unwrap_or(0);
    let left_us = usize::try_from(left.max(0)).unwrap_or(0);
    let top_us = usize::try_from(top.max(0)).unwrap_or(0);
    if width_us == 0 || height_us == 0 {
        return UploadSource {
            bytes: Cow::Owned(Vec::new()),
            bytes_per_row: 0,
            rows: 0,
            origin_x: 0,
            origin_y: 0,
            width: 0,
            height: 0,
        };
    }
    let row_bytes = width_us.saturating_mul(4);
    // wgpu requires `bytes_per_row` to be a multiple of 256 (the
    // COPY_BYTES_PER_ROW_ALIGNMENT validation in wgpu-core).
    let padded = row_bytes.div_ceil(256).saturating_mul(256);
    if region.is_none() && row_bytes == padded {
        // Full frame with a naturally aligned pitch: zero-copy u32 → u8 view
        // of the 0RGB buffer (LE on all supported hosts). `bytemuck::cast_slice`
        // is safe: u32 → u8 is any-bit-pattern.
        return UploadSource {
            bytes: Cow::Borrowed(bytemuck::cast_slice(pixels)),
            bytes_per_row: u32::try_from(padded).unwrap_or(0),
            rows: u32::try_from(height_us).unwrap_or(0),
            origin_x: 0,
            origin_y: 0,
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
        };
    }
    // Pack the region rows into a padded buffer (region copies and unaligned
    // full frames). The inner copy is a per-row u32 memcpy: on the little-
    // endian hosts this emulator targets, the u32 LE bytes ARE the 0RGB pixel
    // (the zero-copy full-frame path above already relies on this identity),
    // so a slice copy lowers to the platform SIMD memcpy instead of the old
    // per-pixel 4-byte stores. `padded` is a multiple of 256, so the u32 view
    // of the pack buffer is exact, and `vec![0_u8; …]` is allocator-aligned.
    let mut buf = vec![0_u8; height_us.saturating_mul(padded)];
    let buf_u32: &mut [u32] = bytemuck::cast_slice_mut(&mut buf);
    let row_words = padded / 4;
    for row in 0..height_us {
        let src_start = top_us
            .saturating_add(row)
            .saturating_mul(frame_w_u)
            .saturating_add(left_us);
        let Some(src) = pixels.get(src_start..src_start.saturating_add(width_us)) else {
            continue;
        };
        let dst_start = row.saturating_mul(row_words);
        let dst_end = dst_start.saturating_add(width_us);
        if let Some(dst) = buf_u32.get_mut(dst_start..dst_end) {
            dst.copy_from_slice(src);
        }
    }
    UploadSource {
        bytes: Cow::Owned(buf),
        bytes_per_row: u32::try_from(padded).unwrap_or(0),
        rows: u32::try_from(height_us).unwrap_or(0),
        origin_x: u32::try_from(left).unwrap_or(0),
        origin_y: u32::try_from(top).unwrap_or(0),
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
    }
}

/// Extract one 0RGB channel (bits `shift..shift+8` of an `0x00RRGGBB` u32)
/// as a normalized `f64` for the wgpu clear color.
#[must_use]
fn color_channel(background_color: u32, shift: u32) -> f64 {
    let byte = u8::try_from((background_color >> shift) & 0xFF).unwrap_or(u8::MAX);
    f64::from(byte) / 255.0
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::upload_source;
    use std::borrow::Cow;
    use wie_winapi::gdi32::IRect;

    /// A `w × h` frame filled with one color.
    fn frame(w: u32, h: u32, fill: u32) -> Vec<u32> {
        vec![fill; usize::try_from(w.saturating_mul(h)).unwrap_or(0)]
    }

    /// The region pixel at frame coordinates (col, row) inside a packed
    /// source, read back as a u32 (0RGB LE).
    fn packed_pixel(bytes: &[u8], bytes_per_row: usize, col: usize, row: usize) -> u32 {
        let off = row
            .saturating_mul(bytes_per_row)
            .saturating_add(col.saturating_mul(4));
        let Some(slot) = bytes.get(off..off.saturating_add(4)) else {
            return u32::MAX;
        };
        u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]])
    }

    /// The full-frame upload for a naturally 256-aligned pitch is zero-copy:
    /// `640 × 4 = 2560 = 10 × 256`.
    #[test]
    fn full_aligned_pitch_is_zero_copy() {
        let pixels = frame(640, 480, 0x00FF_FFFF);
        let src = upload_source(&pixels, 640, 480, None);
        assert!(
            matches!(src.bytes, Cow::Borrowed(_)),
            "an aligned full frame must borrow the pixels, not pack them"
        );
        assert_eq!(src.bytes_per_row, 2560);
        assert_eq!(src.rows, 480);
        assert_eq!(
            (src.origin_x, src.origin_y, src.width, src.height),
            (0, 0, 640, 480)
        );
    }

    /// A partial region is packed into a 256-aligned buffer (the alignment
    /// wgpu requires for `write_texture`) carrying the region's pixels, with
    /// the region's origin as the copy origin.
    #[test]
    fn region_packs_padded_rows_with_origin() {
        let mut pixels = frame(100, 50, 0x0000_00FF); // blue fill
        // A distinctive pixel inside the region: frame (30, 10).
        let idx = 10_usize.saturating_mul(100).saturating_add(30);
        if let Some(px) = pixels.get_mut(idx) {
            *px = 0x00FF_0000; // red
        }
        let region = IRect {
            left: 20,
            top: 5,
            right: 40,
            bottom: 15,
        };
        let src = upload_source(&pixels, 100, 50, Some(region));
        assert!(
            matches!(src.bytes, Cow::Owned(_)),
            "a region-limited upload must pack rows"
        );
        // 20 cols × 4 B = 80 B → padded to the 256-byte alignment.
        assert_eq!(src.bytes_per_row, 256);
        assert_eq!(src.rows, 10);
        assert_eq!(
            (src.origin_x, src.origin_y, src.width, src.height),
            (20, 5, 20, 10)
        );
        // Frame (30,10) → packed row 5 (10 − 5), col 10 (30 − 20).
        let px = packed_pixel(src.bytes.as_ref(), 256, 10, 5);
        assert_eq!(
            px, 0x00FF_0000,
            "the packed buffer carries the region pixel"
        );
        // A pixel OUTSIDE the region (frame (5,5)) is not in the packed rows.
        let px = packed_pixel(src.bytes.as_ref(), 256, 0, 0);
        assert_ne!(px, 0x00FF_0000, "rows outside the region are not copied");
    }

    /// A full frame whose natural pitch is not 256-aligned must be packed
    /// into a padded buffer (the latent-unaligned-width case).
    #[test]
    fn unaligned_full_frame_is_padded() {
        let pixels = frame(90, 40, 0x00AB_CDEF);
        let src = upload_source(&pixels, 90, 40, None);
        assert!(
            matches!(src.bytes, Cow::Owned(_)),
            "an unaligned pitch must be packed, not zero-copy"
        );
        assert_eq!(src.bytes_per_row, 512, "90 × 4 = 360 → padded to 512");
        assert_eq!(src.rows, 40);
        assert_eq!(
            (src.origin_x, src.origin_y, src.width, src.height),
            (0, 0, 90, 40)
        );
        // The first pixel survives the pack.
        let px = packed_pixel(src.bytes.as_ref(), 512, 0, 0);
        assert_eq!(px, 0x00AB_CDEF);
    }

    /// A degenerate region copies nothing (an empty extent); the copy layout
    /// is all zeros so `write_texture` is a no-op-sized call.
    #[test]
    fn degenerate_region_produces_no_copy() {
        let pixels = frame(100, 100, 0);
        let src = upload_source(&pixels, 100, 100, Some(IRect::empty()));
        assert_eq!((src.width, src.height), (0, 0));
        assert_eq!(src.bytes.as_ref().len(), 0);
    }

    /// A region running past the frame edge is clipped to the frame.
    #[test]
    fn region_is_clipped_to_the_frame() {
        let pixels = frame(50, 50, 0);
        let src = upload_source(
            &pixels,
            50,
            50,
            Some(IRect {
                left: 40,
                top: 40,
                right: 200,
                bottom: 200,
            }),
        );
        assert_eq!(
            (src.origin_x, src.origin_y, src.width, src.height),
            (40, 40, 10, 10)
        );
    }

    /// The region pack must be byte-identical regardless of row count (the
    /// per-row u32 memcpy path writes exactly the region's pixels, padding
    /// rows untouched).
    #[test]
    fn region_pack_round_trips_all_rows() {
        // 31 cols × 4 = 124 B/row → padded to 256; 5 rows, 2 padding rows.
        let pixels = frame(80, 20, 0x0011_2233);
        let region = IRect {
            left: 7,
            top: 3,
            right: 38,
            bottom: 8,
        };
        let src = upload_source(&pixels, 80, 20, Some(region));
        assert_eq!(src.rows, 5);
        assert_eq!(src.bytes_per_row, 256);
        let bytes = src.bytes.as_ref();
        for row in 0_usize..5 {
            for col in 0_usize..31 {
                let off = row
                    .saturating_mul(256)
                    .saturating_add(col.saturating_mul(4));
                let slot = &bytes[off..off.saturating_add(4)];
                assert_eq!(
                    u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]),
                    0x0011_2233,
                    "row {row} col {col} must round-trip"
                );
            }
        }
        // Padding bytes after each row's pixels stay zero (only the width is
        // copied, never the 256-byte stride).
        for row in 0_usize..5 {
            let pad_off = row
                .saturating_mul(256)
                .saturating_add(31_usize.saturating_mul(4));
            assert_eq!(&bytes[pad_off..pad_off.saturating_add(2)], &[0, 0]);
        }
    }
}
