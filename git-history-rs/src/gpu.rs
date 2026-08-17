//! The same browser, in a window.
//!
//! # Why a backend and not a port
//!
//! ratatui talks to the world through one narrow trait: `Backend` hands out a
//! grid size and receives painted cells. The terminal is only the usual
//! implementation of it, not the only possible one. So none of the screens in
//! `ui.rs` are rewritten here -- they are drawn a second way. What changes is
//! the surface underneath: a wgpu texture instead of a console.
//!
//! # The grid is a function of the window
//!
//! A console decides its own size and the program obeys. Here the arithmetic
//! runs the other way: columns are the window's width divided by the width of
//! one character. Drag the frame and the division is redone, ratatui relays
//! out against the new size, and the text reflows to the borders instead of
//! being clipped by them. That is the whole reason this module exists.
//!
//! # Offline, like everything else
//!
//! The face is rasterised from a font already on the machine and the shader is
//! compiled at run time from the source below. Nothing is fetched.

use crate::model::Repo;
use crate::source::Source;
use crate::llm::Narrator;
use crate::ui::App;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Rendered size of the face. Cells are derived from it, never guessed.
const FONT_PX: f32 = 17.0;
/// One texture holds every glyph the session ever asks for.
const ATLAS: u32 = 1024;

/// Faces to try, in order. All ship with Windows; the first that opens wins.
const FACES: [&str; 4] = [
    r"C:\Windows\Fonts\CascadiaMono.ttf",
    r"C:\Windows\Fonts\consola.ttf",
    r"C:\Windows\Fonts\lucon.ttf",
    r"C:\Windows\Fonts\cour.ttf",
];

// --- colour -------------------------------------------------------------

/// The xterm 256 palette, computed rather than tabulated: sixteen system
/// colours, a 6×6×6 cube, then a 24-step ramp of greys. `ui.rs` speaks almost
/// entirely in `Color::Indexed`, so this is the translation that matters.
fn indexed(i: u8) -> [f32; 4] {
    const SYS: [[u8; 3]; 16] = [
        [12, 12, 12],    [197, 15, 31],   [19, 161, 14],   [193, 156, 0],
        [0, 55, 218],    [136, 23, 152],  [58, 150, 221],  [204, 204, 204],
        [118, 118, 118], [231, 72, 86],   [22, 198, 12],   [249, 241, 165],
        [59, 120, 255],  [180, 0, 158],   [97, 214, 214],  [242, 242, 242],
    ];
    let rgb = match i {
        0..=15 => SYS[i as usize],
        16..=231 => {
            let n = i - 16;
            let step = |v: u8| if v == 0 { 0u8 } else { 55 + 40 * v };
            [step(n / 36), step((n % 36) / 6), step(n % 6)]
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            [v, v, v]
        }
    };
    rgba(rgb[0], rgb[1], rgb[2])
}

fn rgba(r: u8, g: u8, b: u8) -> [f32; 4] {
    // sRGB values land in a surface configured as sRGB, so no gamma work here.
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

fn resolve(c: Color, fallback: [f32; 4]) -> [f32; 4] {
    match c {
        Color::Reset => fallback,
        Color::Black => indexed(0),
        Color::Red => indexed(1),
        Color::Green => indexed(2),
        Color::Yellow => indexed(3),
        Color::Blue => indexed(4),
        Color::Magenta => indexed(5),
        Color::Cyan => indexed(6),
        Color::Gray => indexed(7),
        Color::DarkGray => indexed(8),
        Color::LightRed => indexed(9),
        Color::LightGreen => indexed(10),
        Color::LightYellow => indexed(11),
        Color::LightBlue => indexed(12),
        Color::LightMagenta => indexed(13),
        Color::LightCyan => indexed(14),
        Color::White => indexed(15),
        Color::Rgb(r, g, b) => rgba(r, g, b),
        Color::Indexed(i) => indexed(i),
    }
}

// --- the glyph atlas ----------------------------------------------------

#[derive(Clone, Copy)]
struct Glyph {
    /// Where the bitmap sits in the atlas, in texels.
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    /// Where the bitmap sits inside its cell, in pixels.
    off: [f32; 2],
    size: [f32; 2],
}

/// A shelf packer. Glyphs are rasterised the first time they are asked for and
/// kept; a session's alphabet is small and stops growing almost at once.
struct Atlas {
    font: fontdue::Font,
    map: HashMap<(char, bool), Glyph>,
    pen: (u32, u32),
    shelf: u32,
    cell: (f32, f32),
    ascent: f32,
    dirty: Vec<(u32, u32, u32, u32, Vec<u8>)>,
}

impl Atlas {
    fn new() -> io::Result<Self> {
        let bytes = FACES
            .iter()
            .find_map(|p| std::fs::read(p).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no monospace face found in C:\\Windows\\Fonts",
                )
            })?;
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // A monospace face advances every glyph equally; 'M' is a safe probe.
        let m = font.metrics('M', FONT_PX);
        let line = font
            .horizontal_line_metrics(FONT_PX)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "face has no line metrics"))?;
        let cw = m.advance_width.ceil().max(1.0);
        let ch = (line.ascent - line.descent + line.line_gap).ceil().max(1.0);

        Ok(Self {
            font,
            map: HashMap::new(),
            pen: (0, 0),
            shelf: 0,
            cell: (cw, ch),
            ascent: line.ascent,
            dirty: Vec::new(),
        })
    }

    fn glyph(&mut self, ch: char, bold: bool) -> Glyph {
        if let Some(g) = self.map.get(&(ch, bold)) {
            return *g;
        }
        let (m, bitmap) = self.font.rasterize(ch, FONT_PX);
        let (w, h) = (m.width as u32, m.height as u32);

        let mut g = Glyph {
            u0: 0.0, v0: 0.0, u1: 0.0, v1: 0.0,
            off: [0.0, 0.0],
            size: [0.0, 0.0],
        };

        if w > 0 && h > 0 {
            if self.pen.0 + w + 1 >= ATLAS {
                self.pen.0 = 0;
                self.pen.1 += self.shelf + 1;
                self.shelf = 0;
            }
            // A full atlas simply stops taking new glyphs rather than
            // corrupting the ones already in it.
            if self.pen.1 + h < ATLAS {
                let (x, y) = self.pen;
                self.dirty.push((x, y, w, h, bitmap));
                self.pen.0 += w + 1;
                self.shelf = self.shelf.max(h);

                g.u0 = x as f32;
                g.v0 = y as f32;
                g.u1 = (x + w) as f32;
                g.v1 = (y + h) as f32;
                // fontdue measures ymin up from the baseline; the cell measures
                // down from its top, so the two have to be reconciled here.
                g.off = [m.xmin as f32, self.ascent - (m.height as f32 + m.ymin as f32)];
                g.size = [w as f32, h as f32];
            }
        }

        self.map.insert((ch, bold), g);
        g
    }
}

// --- the backend --------------------------------------------------------

#[derive(Clone)]
struct GCell {
    ch: char,
    fg: [f32; 4],
    bg: [f32; 4],
    bold: bool,
}

impl Default for GCell {
    fn default() -> Self {
        Self { ch: ' ', fg: indexed(7), bg: indexed(0), bold: false }
    }
}

/// Holds the painted grid. ratatui writes into this; the renderer reads it.
pub struct GpuBackend {
    cells: Vec<GCell>,
    cols: u16,
    rows: u16,
    cursor: Position,
    px: (u32, u32),
    cell: (f32, f32),
}

impl GpuBackend {
    fn new(cols: u16, rows: u16, cell: (f32, f32), px: (u32, u32)) -> Self {
        Self {
            cells: vec![GCell::default(); cols as usize * rows as usize],
            cols,
            rows,
            cursor: Position::new(0, 0),
            px,
            cell,
        }
    }

    /// Re-cut the grid to the window. Contents are dropped because ratatui
    /// repaints in full on the very next frame.
    fn resize(&mut self, cols: u16, rows: u16, px: (u32, u32)) {
        self.cols = cols;
        self.rows = rows;
        self.px = px;
        self.cells = vec![GCell::default(); cols as usize * rows as usize];
    }
}

impl Backend for GpuBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            if x >= self.cols || y >= self.rows {
                continue;
            }
            let idx = y as usize * self.cols as usize + x as usize;
            let mods = cell.modifier;
            let mut fg = resolve(cell.fg, indexed(7));
            let bg = resolve(cell.bg, indexed(0));
            if mods.contains(Modifier::DIM) {
                for c in fg.iter_mut().take(3) {
                    *c *= 0.55;
                }
            }
            let (fg, bg) = if mods.contains(Modifier::REVERSED) { (bg, fg) } else { (fg, bg) };
            self.cells[idx] = GCell {
                ch: cell.symbol().chars().next().unwrap_or(' '),
                fg,
                bg,
                bold: mods.contains(Modifier::BOLD),
            };
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> { Ok(()) }
    fn show_cursor(&mut self) -> Result<(), Self::Error> { Ok(()) }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> { Ok(self.cursor) }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> Result<(), Self::Error> {
        self.cursor = p.into();
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.cells.iter_mut().for_each(|c| *c = GCell::default());
        Ok(())
    }

    fn clear_region(&mut self, _t: ClearType) -> Result<(), Self::Error> {
        self.clear()
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(Size::new(self.cols, self.rows))
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(WindowSize {
            columns_rows: Size::new(self.cols, self.rows),
            pixels: Size::new(self.px.0 as u16, self.px.1 as u16),
        })
    }

    fn flush(&mut self) -> Result<(), Self::Error> { Ok(()) }
}

// --- the shader ---------------------------------------------------------

const SHADER: &str = r#"
struct Uniforms {
    screen : vec2<f32>,
    cell   : vec2<f32>,
    atlas  : vec2<f32>,
    pad    : vec2<f32>,
};
@group(0) @binding(0) var<uniform> U : Uniforms;
@group(0) @binding(1) var tex : texture_2d<f32>;
@group(0) @binding(2) var smp : sampler;

struct Inst {
    @location(0) at    : vec2<f32>,
    @location(1) uv0   : vec2<f32>,
    @location(2) uv1   : vec2<f32>,
    @location(3) fg    : vec4<f32>,
    @location(4) bg    : vec4<f32>,
    @location(5) goff  : vec2<f32>,
    @location(6) gsize : vec2<f32>,
};

struct VOut {
    @builtin(position) pos : vec4<f32>,
    @location(0) local : vec2<f32>,
    @location(1) uv0   : vec2<f32>,
    @location(2) uv1   : vec2<f32>,
    @location(3) fg    : vec4<f32>,
    @location(4) bg    : vec4<f32>,
    @location(5) goff  : vec2<f32>,
    @location(6) gsize : vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi : u32, inst : Inst) -> VOut {
    var corner = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0)
    );
    let c = corner[vi];
    let local = c * U.cell;
    let px = inst.at * U.cell + local;

    var o : VOut;
    // Pixels to clip space, with y running down the screen as a grid expects.
    o.pos = vec4<f32>(
        (px.x / U.screen.x) * 2.0 - 1.0,
        1.0 - (px.y / U.screen.y) * 2.0,
        0.0, 1.0
    );
    o.local = local;
    o.uv0 = inst.uv0;
    o.uv1 = inst.uv1;
    o.fg = inst.fg;
    o.bg = inst.bg;
    o.goff = inst.goff;
    o.gsize = inst.gsize;
    return o;
}

@fragment
fn fs(i : VOut) -> @location(0) vec4<f32> {
    // Cells with no bitmap (spaces, and glyphs the atlas had no room for)
    // fall through to the background, which is the correct picture anyway.
    if (i.gsize.x <= 0.0 || i.gsize.y <= 0.0) {
        return i.bg;
    }
    let g = i.local - i.goff;
    if (g.x < 0.0 || g.y < 0.0 || g.x >= i.gsize.x || g.y >= i.gsize.y) {
        return i.bg;
    }
    let t = g / i.gsize;
    let uv = mix(i.uv0, i.uv1, t) / U.atlas;
    let cov = textureSample(tex, smp, uv).r;
    return vec4<f32>(mix(i.bg.rgb, i.fg.rgb, cov), 1.0);
}
"#;

// --- key translation ----------------------------------------------------

/// winit speaks in its own key vocabulary; `ui.rs` reads crossterm's. The
/// screens stay untouched by translating here rather than there.
fn to_crossterm(key: &Key, mods: ModifiersState) -> Option<KeyEvent> {
    let code = match key {
        Key::Named(NamedKey::Enter) => KeyCode::Enter,
        Key::Named(NamedKey::Escape) => KeyCode::Esc,
        Key::Named(NamedKey::Backspace) => KeyCode::Backspace,
        Key::Named(NamedKey::Tab) => KeyCode::Tab,
        Key::Named(NamedKey::Space) => KeyCode::Char(' '),
        Key::Named(NamedKey::ArrowUp) => KeyCode::Up,
        Key::Named(NamedKey::ArrowDown) => KeyCode::Down,
        Key::Named(NamedKey::ArrowLeft) => KeyCode::Left,
        Key::Named(NamedKey::ArrowRight) => KeyCode::Right,
        Key::Named(NamedKey::PageUp) => KeyCode::PageUp,
        Key::Named(NamedKey::PageDown) => KeyCode::PageDown,
        Key::Named(NamedKey::Home) => KeyCode::Home,
        Key::Named(NamedKey::End) => KeyCode::End,
        Key::Named(NamedKey::Delete) => KeyCode::Delete,
        Key::Character(s) => KeyCode::Char(s.chars().next()?),
        _ => return None,
    };

    let mut m = KeyModifiers::NONE;
    if mods.control_key() { m |= KeyModifiers::CONTROL; }
    if mods.alt_key() { m |= KeyModifiers::ALT; }
    if mods.shift_key() { m |= KeyModifiers::SHIFT; }

    Some(KeyEvent {
        code,
        modifiers: m,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

// --- the window ---------------------------------------------------------

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    texture: wgpu::Texture,
    instances: wgpu::Buffer,
    capacity: u64,
}

/// Which adapter family to ask for. `--vulkan` forces Vulkan; otherwise wgpu
/// picks what the machine prefers, which on Windows is usually DX12.
pub struct Prefs {
    pub vulkan: bool,
}

pub struct Harness {
    app: App,
    terminal: Option<Terminal<GpuBackend>>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    atlas: Atlas,
    mods: ModifiersState,
    prefs: Prefs,
    error: Option<String>,
}

impl Harness {
    pub fn new(
        source: Box<dyn Source>,
        repos: Vec<Repo>,
        narrator: Option<Narrator>,
        prefs: Prefs,
    ) -> io::Result<Self> {
        Ok(Self {
            app: App::new(source, repos, narrator),
            terminal: None,
            window: None,
            gpu: None,
            atlas: Atlas::new()?,
            mods: ModifiersState::empty(),
            prefs,
            error: None,
        })
    }

    /// Columns and rows are whatever fits. This single division is what makes
    /// the text reflow to the frame instead of being cropped by it.
    fn grid(&self, w: u32, h: u32) -> (u16, u16) {
        let (cw, ch) = self.atlas.cell;
        let cols = ((w as f32 / cw).floor() as u16).max(8);
        let rows = ((h as f32 / ch).floor() as u16).max(4);
        (cols, rows)
    }

    fn build(&mut self, window: Arc<Window>) -> Result<(), String> {
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        let backends = if self.prefs.vulkan {
            wgpu::Backends::VULKAN
        } else {
            wgpu::Backends::PRIMARY
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("no surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| {
            let which = if self.prefs.vulkan { "Vulkan" } else { "a GPU" };
            format!("{which} adapter unavailable on this machine")
        })?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("git-history"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| format!("no device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w,
            height: h,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d { width: ATLAS, height: ATLAS, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            // Nearest keeps stems crisp; the atlas is sampled at native size.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        // 18 floats per cell: position, atlas rect, colours, glyph placement.
        let stride = 18 * 4;
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: stride,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 16, shader_location: 2 },
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 24, shader_location: 3 },
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 40, shader_location: 4 },
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 56, shader_location: 5 },
                        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 64, shader_location: 6 },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let capacity = 8192u64;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cells"),
            size: capacity * stride,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.gpu = Some(Gpu {
            surface, device, queue, config, pipeline, bind,
            uniforms, texture, instances, capacity,
        });

        let (cols, rows) = self.grid(w, h);
        let backend = GpuBackend::new(cols, rows, self.atlas.cell, (w, h));
        self.terminal = Some(
            Terminal::new(backend).map_err(|e| format!("no terminal: {e}"))?,
        );
        self.window = Some(window);
        Ok(())
    }

    fn render(&mut self) {
        let (Some(gpu), Some(terminal)) = (self.gpu.as_mut(), self.terminal.as_mut()) else {
            return;
        };

        // Any glyph first seen this frame is uploaded before it is sampled.
        for (x, y, w, h, bytes) in self.atlas.dirty.drain(..) {
            gpu.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &gpu.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &bytes,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }

        let backend = terminal.backend();
        let (cols, rows) = (backend.cols, backend.rows);
        let mut data: Vec<u8> = Vec::with_capacity(cols as usize * rows as usize * 72);
        let mut push = |v: f32, d: &mut Vec<u8>| d.extend_from_slice(&v.to_ne_bytes());

        let cells = backend.cells.clone();
        let mut count = 0u32;
        for row in 0..rows {
            for col in 0..cols {
                let c = &cells[row as usize * cols as usize + col as usize];
                let g = self.atlas.glyph(c.ch, c.bold);
                push(col as f32, &mut data);
                push(row as f32, &mut data);
                push(g.u0, &mut data);
                push(g.v0, &mut data);
                push(g.u1, &mut data);
                push(g.v1, &mut data);
                for v in c.fg { push(v, &mut data); }
                for v in c.bg { push(v, &mut data); }
                push(g.off[0], &mut data);
                push(g.off[1], &mut data);
                push(g.size[0], &mut data);
                push(g.size[1], &mut data);
                count += 1;
            }
        }

        // Glyphs discovered during that walk still need uploading.
        for (x, y, w, h, bytes) in self.atlas.dirty.drain(..) {
            gpu.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &gpu.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &bytes,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }

        if count as u64 > gpu.capacity {
            gpu.capacity = count as u64 * 2;
            gpu.instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cells"),
                size: gpu.capacity * 72,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.bind = gpu.bind.clone();
        }
        gpu.queue.write_buffer(&gpu.instances, 0, &data);

        let (cw, ch) = self.atlas.cell;
        let mut u: Vec<u8> = Vec::with_capacity(32);
        for v in [
            gpu.config.width as f32, gpu.config.height as f32,
            cw, ch,
            ATLAS as f32, ATLAS as f32,
            0.0, 0.0,
        ] {
            u.extend_from_slice(&v.to_ne_bytes());
        }
        gpu.queue.write_buffer(&gpu.uniforms, 0, &u);

        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&gpu.pipeline);
            pass.set_bind_group(0, &gpu.bind, &[]);
            pass.set_vertex_buffer(0, gpu.instances.slice(..));
            pass.draw(0..6, 0..count);
        }
        gpu.queue.submit(Some(enc.finish()));
        frame.present();
    }
}

impl ApplicationHandler for Harness {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("git history")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 720.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(format!("no window: {e}"));
                el.exit();
                return;
            }
        };
        if let Err(e) = self.build(window) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),

            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1), size.height.max(1));
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.config.width = w;
                    gpu.config.height = h;
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
                // The grid is recut here, and ratatui lays out against the new
                // size on the next draw. This is the reflow.
                let (cols, rows) = self.grid(w, h);
                if let Some(t) = self.terminal.as_mut() {
                    t.backend_mut().resize(cols, rows, (w, h));
                    let _ = t.clear();
                }
                if let Some(win) = self.window.as_ref() {
                    win.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let Some(key) = to_crossterm(&event.logical_key, self.mods) else {
                    return;
                };
                let action = self.app.handle(key);
                if let Some(t) = self.terminal.as_mut() {
                    if let Err(e) = self.app.act(action, t) {
                        self.app.status = Some(e.to_string());
                    }
                }
                if !self.app.running || self.app.stack.is_empty() {
                    el.exit();
                    return;
                }
                if let Some(win) = self.window.as_ref() {
                    win.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                if let Some(t) = self.terminal.as_mut() {
                    let app = &mut self.app;
                    let _ = t.draw(|f| app.draw(f));
                }
                self.render();
            }

            _ => {}
        }
    }
}

/// Open the browser in a window. Returns only when the window closes.
pub fn run(
    source: Box<dyn Source>,
    repos: Vec<Repo>,
    narrator: Option<Narrator>,
    prefs: Prefs,
) -> io::Result<()> {
    let mut harness = Harness::new(source, repos, narrator, prefs)?;
    let el = EventLoop::new().map_err(|e| io::Error::other(e.to_string()))?;
    el.set_control_flow(ControlFlow::Wait);
    el.run_app(&mut harness)
        .map_err(|e| io::Error::other(e.to_string()))?;
    if let Some(e) = harness.error {
        return Err(io::Error::other(e));
    }
    Ok(())
}
