//! wgpu surface + egui painting.
//!
//! The compositor's RGBA frame is handed to egui as a texture, so the whole
//! desktop is drawn by one painter in one render pass. That keeps this file
//! small and means the desktop, the dock and the dialogs share one frame order:
//! wallpaper → windows → chrome.

use samlator_compositor::Frame;

use crate::runtime::Runtime;
use crate::ui::{self, Action, UiState};

const WALLPAPER: wgpu::Color = wgpu::Color {
    r: 0.043,
    g: 0.043,
    b: 0.063,
    a: 1.0,
};

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,
    /// Desktop frame uploaded through egui's texture manager.
    desktop: Option<egui::TextureHandle>,
    desktop_size: (u32, u32),
    last_seq: u64,
    /// Largest texture the adapter accepts; egui must be told the same number.
    max_side: usize,
    pub size: (u32, u32),
}

impl Gpu {
    pub fn new(
        window: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance.create_surface(window).ok()?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .ok()?;

        // `downlevel_defaults()` caps textures at 2048px, which is smaller than
        // a 2340x1080 phone surface — asking for it panics during configure.
        let max_dim = adapter.limits().max_texture_dimension_2d.max(1);
        let limits = adapter.limits();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("samlator"),
                required_features: wgpu::Features::empty(),
                required_limits: limits,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            }))
            .ok()?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Never configure a surface the adapter cannot back.
        let width = width.clamp(1, max_dim);
        let height = height.clamp(1, max_dim);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let egui_ctx = egui::Context::default();
        egui_ctx.set_visuals(egui::Visuals::dark());
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                ..Default::default()
            },
        );

        Some(Self {
            surface,
            device,
            queue,
            config,
            egui_ctx,
            egui_renderer,
            desktop: None,
            desktop_size: (0, 0),
            last_seq: 0,
            max_side: max_dim as usize,
            size: (width.max(1), height.max(1)),
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size = (width, height);
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn pixels_per_point(&self) -> f32 {
        self.egui_ctx.pixels_per_point()
    }

    /// Push a new desktop frame into egui, if the compositor changed it.
    pub fn upload(&mut self, frame: &Frame) {
        if frame.seq == self.last_seq {
            return;
        }
        self.last_seq = frame.seq;
        let size = (frame.width.max(1), frame.height.max(1));
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [size.0 as usize, size.1 as usize],
            &frame.rgba,
        );
        let options = egui::TextureOptions {
            magnification: egui::TextureFilter::Linear,
            minification: egui::TextureFilter::Linear,
            ..Default::default()
        };
        match self.desktop.as_mut() {
            Some(handle) if self.desktop_size == size => handle.set(image, options),
            _ => {
                self.desktop = Some(self.egui_ctx.load_texture("desktop", image, options));
                self.desktop_size = size;
            }
        }
    }

    /// One desktop frame: egui input, texture upload, single render pass.
    pub fn render(
        &mut self,
        rt: &mut Runtime,
        ui_state: &mut UiState,
        raw_input: egui::RawInput,
    ) -> Option<Action> {
        let mut action = None;
        self.paint_frame(raw_input, |_, root| {
            action = Some(ui::draw(root, rt, ui_state));
        });
        action
    }

    /// Draw with a caller-supplied painter instead of the desktop UI.
    pub fn render_raw(
        &mut self,
        raw_input: egui::RawInput,
        paint: impl FnOnce(&egui::Context, &mut egui::Ui),
    ) -> Option<()> {
        let cell = std::cell::Cell::new(Some(paint));
        self.paint_frame(raw_input, |ctx, root| {
            if let Some(p) = cell.take() {
                p(ctx, root);
            }
        })
        .map(|_| ())
    }

    fn paint_frame(
        &mut self,
        mut raw_input: egui::RawInput,
        paint: impl FnOnce(&egui::Context, &mut egui::Ui),
    ) -> Option<()> {
        // Without this egui refuses to allocate the full-screen desktop texture.
        raw_input.max_texture_side = Some(self.max_side);
        let ctx = self.egui_ctx.clone();
        let desktop = self.desktop.as_ref().map(|h| h.id());

        let cell = std::cell::Cell::new(Some(paint));
        let full = ctx.run_ui(raw_input, |root| {
            let screen = root.max_rect().size();
            if let Some(id) = desktop {
                let inner = root.ctx().clone();
                egui::Area::new(egui::Id::new("samlator-desktop".to_owned()))
                    .order(egui::Order::Background)
                    .fixed_pos(egui::Pos2::ZERO)
                    .show(&inner, |ui| {
                        ui.add(egui::Image::new((id, screen)));
                    });
            }
            if let Some(paint) = cell.take() {
                paint(&root.ctx().clone(), root);
            }
        });
        let paint_jobs = ctx.tessellate(full.shapes, full.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.0, self.size.1],
            pixels_per_point: full.pixels_per_point,
        };

        for (id, deltas) in &full.textures_delta.set {
            for delta in deltas {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }

        let surface_tex = acquire(&self.surface, &self.device, &self.config)?;
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("samlator") });

        let user_cmds = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen,
        );

        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("samlator-ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(WALLPAPER),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.egui_renderer
                .render(&mut pass.forget_lifetime(), &paint_jobs, &screen);
        }

        self.queue
            .submit(user_cmds.into_iter().chain([encoder.finish()]));
        self.queue.present(surface_tex);

        for id in &full.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        Some(())
    }
}

/// `wgpu` 30 reports surface state through an enum rather than a `Result`.
fn acquire(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> Option<wgpu::SurfaceTexture> {
    use wgpu::CurrentSurfaceTexture as C;
    match surface.get_current_texture() {
        C::Success(t) => Some(t),
        C::Suboptimal(t) => {
            surface.configure(device, config);
            Some(t)
        }
        C::Outdated | C::Lost => {
            surface.configure(device, config);
            None
        }
        C::Timeout | C::Occluded | C::Validation => None,
    }
}
