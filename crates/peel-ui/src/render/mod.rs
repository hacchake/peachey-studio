//! wgpu renderer: one instanced SDF pipeline for shapes plus glyphon text,
//! interleaved per layer.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Cache, ColorMode, Resolution, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::geom::Color;
use crate::scene::{Scene, ShapeInstance};
use crate::text::TextSystem;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    scale: f32,
    _pad: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no compatible graphics adapter: {0}")]
    Adapter(String),
    #[error("could not create graphics device: {0}")]
    Device(String),
    #[error("could not create window surface: {0}")]
    Surface(String),
}

pub struct Renderer {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    atlas: TextAtlas,
    viewport: Viewport,
    swash: SwashCache,
    text_renderers: Vec<TextRenderer>,
    staging: Vec<ShapeInstance>,
    adapter_info: String,
    window: Arc<Window>,
}

const INITIAL_INSTANCES: usize = 16 * 1024;

impl Renderer {
    pub async fn new(
        window: Arc<Window>,
        display: winit::event_loop::OwnedDisplayHandle,
    ) -> Result<Renderer, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(display),
        ));
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| RenderError::Surface(e.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Adapter(e.to_string()))?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("peel device"),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer a non-sRGB format: we blend in sRGB space like a browser so
        // anti-aliased edges and text weights match design expectations.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                !f.is_srgb()
                    && matches!(
                        f,
                        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                    )
            })
            .or_else(|| caps.formats.iter().copied().find(|f| !f.is_srgb()))
            .unwrap_or(caps.formats[0]);
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Opaque);
        let size = window.inner_size();
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else {
            caps.present_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("peel shapes"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shapes.wgsl").into()),
        });
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("peel globals"),
            contents: bytemuck::bytes_of(&Globals {
                viewport: [1.0, 1.0],
                scale: 1.0,
                _pad: 0.0,
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("peel globals layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("peel globals bind"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("peel shapes layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let attrs = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4,
            4 => Float32x4, 5 => Float32x4, 6 => Float32x4
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("peel shapes"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ShapeInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("peel instances"),
            size: (INITIAL_INSTANCES * std::mem::size_of::<ShapeInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);

        Ok(Renderer {
            instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            globals,
            globals_bind,
            instances,
            instance_capacity: INITIAL_INSTANCES,
            atlas,
            viewport,
            swash: SwashCache::new(),
            text_renderers: Vec::new(),
            staging: Vec::new(),
            adapter_info,
            window,
        })
    }

    pub fn adapter_info(&self) -> &str {
        &self.adapter_info
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    /// Draws a scene. Returns false if the frame was skipped (surface lost
    /// or occluded) and should be retried.
    pub fn render(&mut self, scene: &Scene, text: &mut TextSystem, clear: Color) -> bool {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return false;
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // 受け取ったテクスチャを持ったまま configure すると wgpu の
                // 検査で落ちる（最小化から戻したときに起きる）。先に手放す。
                drop(frame);
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                match self.instance.create_surface(self.window.clone()) {
                    Ok(s) => {
                        self.surface = s;
                        self.surface.configure(&self.device, &self.config);
                    }
                    Err(e) => log::error!("surface lost and could not be recreated: {e}"),
                }
                return false;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("surface validation error");
                return false;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let (w, h) = (self.config.width, self.config.height);
        let encoder = self.encode(scene, text, clear, &view, w, h);
        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        self.queue.present(frame);
        self.atlas.trim();
        true
    }

    /// Renders a scene offscreen at the current surface size and returns
    /// tightly packed RGBA8 pixels (for screenshots and visual tests).
    pub fn capture(
        &mut self,
        scene: &Scene,
        text: &mut TextSystem,
        clear: Color,
    ) -> (u32, u32, Vec<u8>) {
        let (w, h) = (self.config.width, self.config.height);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("peel capture"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.encode(scene, text, clear, &view, w, h);
        let bytes_per_row = (w * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("peel capture readback"),
            size: (bytes_per_row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let Ok(data) = slice.get_mapped_range() else {
            return (w, h, vec![0; (w * h * 4) as usize]);
        };
        let bgra = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let start = (row * bytes_per_row) as usize;
            let line = &data[start..start + (w * 4) as usize];
            for px in line.as_chunks::<4>().0 {
                if bgra {
                    out.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        drop(data);
        buffer.unmap();
        self.atlas.trim();
        (w, h, out)
    }

    fn encode(
        &mut self,
        scene: &Scene,
        text: &mut TextSystem,
        clear: Color,
        view: &wgpu::TextureView,
        w: u32,
        h: u32,
    ) -> wgpu::CommandEncoder {
        let scale = scene.scale();
        self.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals {
                viewport: [w as f32, h as f32],
                scale,
                _pad: 0.0,
            }),
        );
        self.viewport.update(
            &self.queue,
            Resolution {
                width: w,
                height: h,
            },
        );

        // Upload every layer's shapes into one buffer.
        self.staging.clear();
        let mut ranges = Vec::with_capacity(scene.layers.len());
        for layer in &scene.layers {
            let start = self.staging.len() as u32;
            self.staging.extend_from_slice(&layer.shapes);
            ranges.push(start..self.staging.len() as u32);
        }
        if self.staging.len() > self.instance_capacity {
            self.instance_capacity = self.staging.len().next_power_of_two();
            self.instances = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("peel instances"),
                size: (self.instance_capacity * std::mem::size_of::<ShapeInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !self.staging.is_empty() {
            self.queue
                .write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.staging));
        }

        // Prepare text per layer.
        while self.text_renderers.len() < scene.layers.len() {
            self.text_renderers.push(TextRenderer::new(
                &mut self.atlas,
                &self.device,
                wgpu::MultisampleState::default(),
                None,
            ));
        }
        let (font_system, cache) = text.parts();
        for (i, layer) in scene.layers.iter().enumerate() {
            let areas: Vec<TextArea> = layer
                .texts
                .iter()
                .filter_map(|t| {
                    let buffer = cache.get(&t.key).map(|e| &e.buffer)?;
                    let c = t.color.to_rgba8();
                    let clip = t.clip;
                    Some(TextArea {
                        buffer,
                        left: (t.pos.x * scale).round(),
                        top: (t.pos.y * scale).round(),
                        scale,
                        bounds: TextBounds {
                            left: (clip.x * scale).floor().max(-1e6) as i32,
                            top: (clip.y * scale).floor().max(-1e6) as i32,
                            right: (clip.right() * scale).ceil().min(1e6) as i32,
                            bottom: (clip.bottom() * scale).ceil().min(1e6) as i32,
                        },
                        default_color: glyphon::Color::rgba(c[0], c[1], c[2], c[3]),
                        custom_glyphs: &[],
                    })
                })
                .collect();
            if let Err(e) = self.text_renderers[i].prepare(
                &self.device,
                &self.queue,
                font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash,
            ) {
                log::warn!("text prepare failed: {e}");
            }
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("peel frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("peel main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear.r as f64,
                            g: clear.g as f64,
                            b: clear.b as f64,
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
            for (i, range) in ranges.iter().enumerate() {
                if !range.is_empty() {
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &self.globals_bind, &[]);
                    pass.set_vertex_buffer(0, self.instances.slice(..));
                    pass.draw(0..4, range.clone());
                }
                if !scene.layers[i].texts.is_empty()
                    && let Err(e) =
                        self.text_renderers[i].render(&self.atlas, &self.viewport, &mut pass)
                {
                    log::warn!("text render failed: {e}");
                }
            }
        }
        encoder
    }
}
