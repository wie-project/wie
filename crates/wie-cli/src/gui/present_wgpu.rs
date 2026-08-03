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
    pub(crate) fn present(
        &mut self,
        frame: SurfaceFrame,
        window_w: u32,
        window_h: u32,
    ) -> Result<()> {
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
        } = frame;
        let frame_w = frame_w_raw.max(1);
        let frame_h = frame_h_raw.max(1);

        // Recreate the staging texture when the guest frame size changed.
        // Publish-model rework: every publish is a FULL frame, so the
        // staging texture always equals the last published frame — the
        // region-delta contract ("changes since the last guest publish")
        // cannot survive present skipping, which would leave a losing
        // publish's delta permanently absent from the persistent staging.
        if self
            .staging
            .as_ref()
            .is_none_or(|s| s.width != frame_w || s.height != frame_h)
        {
            self.staging = Some(self.make_staging(frame_w, frame_h)?);
        }
        let staging = self.staging.as_ref().context("staging texture")?;

        // Full-frame upload: zero-copy u32 → u8 view of the 0RGB buffer (LE
        // on all supported hosts). `bytemuck::cast_slice` is safe: u32 → u8
        // is any-bit-pattern.
        let bytes = bytemuck::cast_slice(&pixels);
        let pitch = usize::try_from(frame_w)
            .context("frame width")?
            .saturating_mul(4);
        let needed = usize::try_from(frame_h)
            .context("frame height")?
            .saturating_mul(pitch);
        let src = bytes.get(..needed).context("full frame upload slice")?;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &staging.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            src,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(u32::try_from(pitch).context("pitch")?),
                rows_per_image: Some(frame_h),
            },
            wgpu::Extent3d {
                width: frame_w,
                height: frame_h,
                depth_or_array_layers: 1,
            },
        );
        // wgpu copied the pixels into the staging buffer synchronously; the
        // render pass below only samples the staging texture. Release the
        // guest Arc now instead of holding it across the render + present.
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
                                // Clear to white: untouched areas of the
                                // window (e.g. behind an unmaximized guest
                                // frame) read as a fresh white surface rather
                                // than black. The whole surface is redrawn
                                // every frame and the swapchain target is
                                // transient, so a clear is cheaper than
                                // loading the previous frame's contents.
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 1.0,
                                    g: 1.0,
                                    b: 1.0,
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
            }
            // Nothing to draw — try again on the next redraw.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                tracing::debug!(target: "wiegui", "surface acquire skipped (timeout/occluded)");
            }
            // The surface geometry changed; reconfigure and let the next
            // redraw re-acquire.
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.resize(window_w, window_h);
            }
            // Lost — recreate the surface from the window and reconfigure.
            wgpu::CurrentSurfaceTexture::Lost => {
                self.recreate_surface()?;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                tracing::debug!(target: "wiegui", "surface acquire validation error; skipping frame");
            }
        }
        Ok(())
    }

    /// Create a staging texture at `width`×`height` plus its view and bind
    /// group. `Rgba8Unorm`: the guest's 0RGB u32 LE bytes are R,G,B,0 in
    /// memory, which is exactly an RGBA8 texel row.
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
            format: wgpu::TextureFormat::Rgba8Unorm,
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
