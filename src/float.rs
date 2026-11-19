use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym},
        pointer::{AxisScroll, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use crate::{
    backend::OutputInfo,
    error::{Error, Result},
    imagebuf::ImageBuffer,
    overlay::{
        Theme, ThemeSource, draw_text_scaled, fill_rounded_rect, put_pixel, text_width_scaled,
    },
    wayland::output_info,
};

const LEFT_BUTTON: u32 = 0x110;
const MIDDLE_BUTTON: u32 = 0x112;
const SHADOW: f64 = 20.0;
const RADIUS: f64 = 12.0;
const BORDER: f64 = 1.25;
const ZOOM_STEP: f64 = 0.1;
const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 4.0;
const MIN_VISIBLE: f64 = 32.0;
const DOUBLE_CLICK_MS: u64 = 400;
const DOUBLE_CLICK_DISTANCE: f64 = 5.0;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub struct FloatOptions {
    pub output: Option<String>,
    pub place_at_region: bool,
    pub theme_source: ThemeSource,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn right(self) -> f64 {
        self.x + self.width
    }

    fn bottom(self) -> f64 {
        self.y + self.height
    }

    fn intersects(self, other: Rect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }
}

struct FloatSurface {
    output: OutputInfo,
    layer: LayerSurface,
    viewport: WpViewport,
    logical_width: u32,
    logical_height: u32,
    configured: bool,
    frame_pending: bool,
    redraw_pending: bool,
    buffers: VecDeque<Buffer>,
}

struct Drag {
    output_index: usize,
    start_pointer: (f64, f64),
    start_image: (f64, f64),
}

struct WindowCache {
    scale: f64,
    zoom: f64,
    size: (u32, u32),
    pixels: Vec<u8>,
}

struct FloatState {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    viewporter: WpViewporter,
    shm: Shm,
    pool: SlotPool,
    outputs: Vec<(OutputInfo, wl_output::WlOutput)>,
    surfaces: Vec<FloatSurface>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    image: ImageBuffer,
    path: PathBuf,
    theme: Theme,
    image_pos: (f64, f64),
    zoom: f64,
    dragging: Option<Drag>,
    last_click: Option<(Instant, (f64, f64))>,
    window_caches: Vec<WindowCache>,
    running: bool,
    error: Option<Error>,
}

pub fn show(image: ImageBuffer, path: PathBuf, options: FloatOptions) -> Result<()> {
    let connection = Connection::connect_to_env()
        .map_err(|err| Error::Wayland(format!("float connect: {err}")))?;
    let (globals, mut event_queue) = registry_queue_init::<FloatState>(&connection)
        .map_err(|err| Error::Wayland(format!("float registry: {err}")))?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|_| Error::MissingProtocol("wl_compositor"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|_| Error::MissingProtocol("wlr-layer-shell-unstable-v1"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|_| Error::MissingProtocol("wl_shm"))?;
    let viewporter = globals
        .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
        .map_err(|_| Error::MissingProtocol("wp_viewporter"))?;

    let mut state = FloatState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor,
        layer_shell,
        viewporter,
        pool: SlotPool::new(4, &shm)
            .map_err(|err| Error::Wayland(format!("float shared memory: {err}")))?,
        shm,
        outputs: Vec::new(),
        surfaces: Vec::new(),
        pointer: None,
        keyboard: None,
        image,
        path,
        theme: Theme::load(options.theme_source),
        image_pos: (0.0, 0.0),
        zoom: 1.0,
        dragging: None,
        last_click: None,
        window_caches: Vec::new(),
        running: true,
        error: None,
    };
    event_queue
        .roundtrip(&mut state)
        .map_err(|err| Error::Wayland(format!("float output roundtrip: {err}")))?;

    state.outputs = state
        .output_state
        .outputs()
        .filter_map(|output| {
            let info = output_info(state.output_state.info(&output)?);
            Some((info, output))
        })
        .collect();
    state
        .outputs
        .sort_by(|(left, _), (right, _)| left.name.cmp(&right.name));
    if state.outputs.is_empty() {
        return Err(Error::NoOutputs);
    }
    state.initialize(options.output.as_deref(), options.place_at_region);
    state.ensure_surfaces(&qh);
    event_queue
        .roundtrip(&mut state)
        .map_err(|err| Error::Wayland(format!("float configure roundtrip: {err}")))?;
    state.redraw_all(&qh);

    let mut event_loop: EventLoop<FloatState> =
        EventLoop::try_new().map_err(|err| Error::Wayland(format!("float event loop: {err}")))?;
    WaylandSource::new(connection, event_queue)
        .insert(event_loop.handle())
        .map_err(|err| Error::Wayland(format!("float event source: {err}")))?;

    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_interrupt as *const () as libc::sighandler_t,
        );
    }
    while state.running && !INTERRUPTED.load(Ordering::SeqCst) {
        if let Err(err) = event_loop.dispatch(Duration::from_millis(250), &mut state) {
            if INTERRUPTED.load(Ordering::SeqCst) {
                break;
            }
            return Err(Error::Wayland(format!("float dispatch: {err}")));
        }
    }
    if let Some(error) = state.error.take() {
        return Err(error);
    }
    Ok(())
}

impl FloatState {
    fn initialize(&mut self, requested_output: Option<&str>, place_at_region: bool) {
        let preferred = requested_output
            .filter(|name| !name.is_empty())
            .or_else(|| Some(self.image.region.output.as_str()).filter(|name| !name.is_empty()));
        let index = preferred
            .and_then(|name| {
                self.outputs
                    .iter()
                    .position(|(output, _)| output.name == name)
            })
            .unwrap_or(0);
        let output = &self.outputs[index].0;
        self.zoom = initial_zoom(&self.image, output);
        self.image_pos = initial_position(&self.image, output, self.zoom, place_at_region);
        self.window_caches.clear();
    }

    fn image_logical_size(&self) -> (f64, f64) {
        image_logical_size(&self.image)
    }

    fn image_render_size(&self) -> (u32, u32) {
        let (width, height) = self.image_logical_size();
        (
            (width * self.zoom).round().max(1.0) as u32,
            (height * self.zoom).round().max(1.0) as u32,
        )
    }

    fn image_rect(&self) -> Rect {
        let (width, height) = self.image_render_size();
        Rect {
            x: self.image_pos.0,
            y: self.image_pos.1,
            width: width as f64,
            height: height as f64,
        }
    }

    fn window_size(&self) -> (u32, u32) {
        let (width, height) = self.image_render_size();
        (
            width.saturating_add((SHADOW * 2.0) as u32),
            height.saturating_add((SHADOW * 2.0) as u32),
        )
    }

    fn window_rect(&self) -> Rect {
        let (width, height) = self.window_size();
        Rect {
            x: self.image_pos.0 - SHADOW,
            y: self.image_pos.1 - SHADOW,
            width: width as f64,
            height: height as f64,
        }
    }

    fn output_rect(&self, index: usize) -> Rect {
        let output = &self.outputs[index].0;
        Rect {
            x: output.x as f64,
            y: output.y as f64,
            width: output.logical_width as f64,
            height: output.logical_height as f64,
        }
    }

    fn workspace_bounds(&self) -> Rect {
        let mut left = f64::INFINITY;
        let mut top = f64::INFINITY;
        let mut right = f64::NEG_INFINITY;
        let mut bottom = f64::NEG_INFINITY;
        for (output, _) in &self.outputs {
            left = left.min(output.x as f64);
            top = top.min(output.y as f64);
            right = right.max(output.x as f64 + output.logical_width as f64);
            bottom = bottom.max(output.y as f64 + output.logical_height as f64);
        }
        Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }

    fn clamp_visible(&mut self) {
        let (width, height) = self.image_logical_size();
        let (width, height) = (width * self.zoom, height * self.zoom);
        let bounds = self.workspace_bounds();
        self.image_pos =
            clamp_partially_visible(self.image_pos, width, height, bounds, MIN_VISIBLE);
    }

    fn ensure_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let window = self.window_rect();
        let missing: Vec<usize> = (0..self.outputs.len())
            .filter(|&index| {
                window.intersects(self.output_rect(index))
                    && !self
                        .surfaces
                        .iter()
                        .any(|surface| surface.output.name == self.outputs[index].0.name)
            })
            .collect();
        for index in missing {
            let (info, output) = self.outputs[index].clone();
            let wl_surface = self.compositor.create_surface(qh);
            let viewport = self.viewporter.get_viewport(&wl_surface, qh, ());
            let layer = self.layer_shell.create_layer_surface(
                qh,
                wl_surface,
                Layer::Overlay,
                Some("dmsshot-rs-float"),
                Some(&output),
            );
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            layer.commit();
            self.surfaces.push(FloatSurface {
                output: info,
                layer,
                viewport,
                logical_width: 0,
                logical_height: 0,
                configured: false,
                frame_pending: false,
                redraw_pending: true,
                buffers: VecDeque::new(),
            });
        }
    }

    fn geometry_changed(&mut self, qh: &QueueHandle<Self>) {
        self.ensure_surfaces(qh);
        self.redraw_all(qh);
    }

    fn redraw_all(&mut self, qh: &QueueHandle<Self>) {
        for index in 0..self.surfaces.len() {
            self.redraw(index, qh);
        }
    }

    fn ensure_window_cache(&mut self, scale: f64) -> usize {
        if let Some(index) = self.window_caches.iter().position(|cache| {
            (cache.scale - scale).abs() < 0.001 && (cache.zoom - self.zoom).abs() < f64::EPSILON
        }) {
            return index;
        }
        self.window_caches
            .retain(|cache| (cache.zoom - self.zoom).abs() < f64::EPSILON);
        let (image_width, image_height) = render_image_size(&self.image, self.zoom, scale);
        let shadow = shadow_pixels(scale);
        let width = image_width.saturating_add(shadow * 2);
        let height = image_height.saturating_add(shadow * 2);
        let pixels = self.render_window(width, height, image_width, image_height, scale);
        if self.window_caches.len() >= 4 {
            self.window_caches.remove(0);
        }
        self.window_caches.push(WindowCache {
            scale,
            zoom: self.zoom,
            size: (width, height),
            pixels,
        });
        self.window_caches.len() - 1
    }

    fn render_window(
        &self,
        width: u32,
        height: u32,
        image_width: u32,
        image_height: u32,
        scale: f64,
    ) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        let shadow = shadow_pixels(scale) as f64;
        let image_x = shadow;
        let image_y = shadow;
        let radius = RADIUS * scale;
        let border = BORDER * scale;
        let [bg_r, bg_g, bg_b, _] = self.theme.background;
        for y in 0..height {
            let py = y as f64 + 0.5;
            for x in 0..width {
                let px = x as f64 + 0.5;
                let distance = rounded_rect_sdf(
                    px,
                    py,
                    image_x,
                    image_y,
                    image_width as f64,
                    image_height as f64,
                    radius,
                );
                if distance > 0.0 {
                    if distance <= shadow {
                        let falloff = 1.0 - distance / shadow;
                        let alpha = (falloff * falloff * 140.0).round() as u8;
                        put_pixel(&mut data, width, x, y, [0, 0, 0, alpha]);
                    }
                    continue;
                }
                if distance > -border {
                    put_pixel(&mut data, width, x, y, self.theme.accent);
                    continue;
                }
                let local_x = x as i64 - image_x as i64;
                let local_y = y as i64 - image_y as i64;
                if local_x < 0
                    || local_y < 0
                    || local_x >= image_width as i64
                    || local_y >= image_height as i64
                {
                    let mut background = self.theme.background;
                    background[3] = 255;
                    put_pixel(&mut data, width, x, y, background);
                    continue;
                }
                let source_x = (local_x as f64 / image_width as f64 * self.image.width as f64)
                    .floor()
                    .clamp(0.0, self.image.width.saturating_sub(1) as f64)
                    as usize;
                let source_y = (local_y as f64 / image_height as f64 * self.image.height as f64)
                    .floor()
                    .clamp(0.0, self.image.height.saturating_sub(1) as f64)
                    as usize;
                let offset = (source_y * self.image.width as usize + source_x) * 4;
                let [red, green, blue, alpha] = [
                    self.image.pixels[offset],
                    self.image.pixels[offset + 1],
                    self.image.pixels[offset + 2],
                    self.image.pixels[offset + 3],
                ];
                let source_alpha = alpha as f64 / 255.0;
                let pixel = if alpha == 255 {
                    [red, green, blue, 255]
                } else {
                    [
                        (red as f64 * source_alpha + bg_r as f64 * (1.0 - source_alpha)) as u8,
                        (green as f64 * source_alpha + bg_g as f64 * (1.0 - source_alpha)) as u8,
                        (blue as f64 * source_alpha + bg_b as f64 * (1.0 - source_alpha)) as u8,
                        255,
                    ]
                };
                put_pixel(&mut data, width, x, y, pixel);
            }
        }
        self.draw_zoom_badge(&mut data, width, height, image_width, image_height, scale);
        data
    }

    fn draw_zoom_badge(
        &self,
        data: &mut [u8],
        width: u32,
        height: u32,
        w: u32,
        h: u32,
        scale: f64,
    ) {
        if (self.zoom - 1.0).abs() < 0.005 || !self.image.scale.is_finite() {
            return;
        }
        let label = format!("{}%", (self.zoom * 100.0).round() as i64);
        let padding = (12.0 * scale).round() as u32;
        let inset = (8.0 * scale).round() as u32;
        let badge_width = text_width_scaled(&label, scale).saturating_add(padding);
        let badge_height = (15.0 * scale).round().max(1.0) as u32;
        if w <= badge_width + inset * 2 || h <= badge_height + inset * 2 {
            return;
        }
        let shadow = shadow_pixels(scale);
        let x = shadow + w - badge_width - inset;
        let y = shadow + h - badge_height - inset;
        let mut background = self.theme.background;
        background[3] = 255;
        fill_rounded_rect(
            data,
            width,
            height,
            (x, y, badge_width, badge_height),
            (5.0 * scale).round().max(1.0) as u32,
            background,
        );
        draw_text_scaled(
            data,
            width,
            height,
            x + (6.0 * scale).round() as u32,
            y + (4.0 * scale).round() as u32,
            &label,
            self.theme.text,
            scale,
        );
    }

    fn redraw(&mut self, index: usize, qh: &QueueHandle<Self>) {
        if !self.surfaces[index].configured {
            self.surfaces[index].redraw_pending = true;
            return;
        }
        if self.surfaces[index].frame_pending {
            self.surfaces[index].redraw_pending = true;
            return;
        }
        let output = self.surfaces[index].output.clone();
        let scale = output_scale(&output);
        let out_width = self.surfaces[index].logical_width.max(1);
        let out_height = self.surfaces[index].logical_height.max(1);
        let out_pixel_width = scaled_extent(out_width, scale);
        let out_pixel_height = scaled_extent(out_height, scale);
        let cache_index = self.ensure_window_cache(scale);
        let mut rgba = vec![0u8; out_pixel_width as usize * out_pixel_height as usize * 4];
        {
            let cache = &self.window_caches[cache_index];
            let (window_width, window_height) = cache.size;
            let shadow = shadow_pixels(scale) as i64;
            let window_x = ((self.image_pos.0 - output.x as f64) * scale).round() as i64 - shadow;
            let window_y = ((self.image_pos.1 - output.y as f64) * scale).round() as i64 - shadow;
            let left = window_x.max(0);
            let top = window_y.max(0);
            let right = (window_x + window_width as i64).min(out_pixel_width as i64);
            let bottom = (window_y + window_height as i64).min(out_pixel_height as i64);
            if right > left && bottom > top {
                let copy_width = (right - left) as usize;
                for row in top..bottom {
                    let source_row = (row - window_y) as usize;
                    let source_x = (left - window_x) as usize;
                    let source = (source_row * window_width as usize + source_x) * 4;
                    let target = (row as usize * out_pixel_width as usize + left as usize) * 4;
                    rgba[target..target + copy_width * 4]
                        .copy_from_slice(&cache.pixels[source..source + copy_width * 4]);
                }
            }
        }

        let image = self.image_rect();
        let local_x = image.x.round() as i64 - output.x as i64;
        let local_y = image.y.round() as i64 - output.y as i64;
        let input = clip_input_rect(
            local_x,
            local_y,
            image.width.round().max(1.0) as i64,
            image.height.round().max(1.0) as i64,
            out_width as i64,
            out_height as i64,
        );
        let region = self.compositor.wl_compositor().create_region(qh, ());
        if let Some((x, y, width, height)) = input {
            region.add(x, y, width as i32, height as i32);
        }
        self.surfaces[index].layer.set_input_region(Some(&region));
        region.destroy();

        let Ok((buffer, canvas)) = self.pool.create_buffer(
            out_pixel_width as i32,
            out_pixel_height as i32,
            out_pixel_width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) else {
            self.error = Some(Error::Wayland("unable to allocate float buffer".into()));
            self.running = false;
            return;
        };
        for (source, target) in rgba.chunks_exact(4).zip(canvas.chunks_exact_mut(4)) {
            let alpha = source[3] as u16;
            target.copy_from_slice(&[
                (source[2] as u16 * alpha / 255) as u8,
                (source[1] as u16 * alpha / 255) as u8,
                (source[0] as u16 * alpha / 255) as u8,
                source[3],
            ]);
        }
        let surface = &mut self.surfaces[index];
        surface.layer.wl_surface().damage_buffer(
            0,
            0,
            out_pixel_width as i32,
            out_pixel_height as i32,
        );
        surface
            .viewport
            .set_destination(out_width as i32, out_height as i32);
        if let Err(error) = buffer.attach_to(surface.layer.wl_surface()) {
            self.error = Some(Error::Wayland(format!(
                "unable to attach float buffer: {error}"
            )));
            self.running = false;
            return;
        }
        surface
            .layer
            .wl_surface()
            .frame(qh, FrameCallbackData(surface.layer.wl_surface().clone()));
        surface.layer.commit();
        surface.frame_pending = true;
        surface.redraw_pending = false;
        surface.buffers.push_back(buffer);
        while surface.buffers.len() > 3 {
            surface.buffers.pop_front();
        }
    }

    fn pointer_global(&self, index: usize, position: (f64, f64)) -> (f64, f64) {
        let output = &self.outputs[index].0;
        (output.x as f64 + position.0, output.y as f64 + position.1)
    }

    fn zoom_by(&mut self, steps: f64, qh: &QueueHandle<Self>) {
        let zoom = (self.zoom + steps * ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - self.zoom).abs() < f64::EPSILON {
            return;
        }
        let (width, height) = self.image_logical_size();
        let center = (
            self.image_pos.0 + width * self.zoom / 2.0,
            self.image_pos.1 + height * self.zoom / 2.0,
        );
        self.zoom = zoom;
        self.image_pos = (
            center.0 - width * zoom / 2.0,
            center.1 - height * zoom / 2.0,
        );
        self.clamp_visible();
        self.window_caches.clear();
        self.geometry_changed(qh);
    }
}

fn image_logical_size(image: &ImageBuffer) -> (f64, f64) {
    let scale = if image.scale.is_finite() && image.scale > 0.0 {
        image.scale
    } else {
        1.0
    };
    (
        (image.width as f64 / scale).max(1.0),
        (image.height as f64 / scale).max(1.0),
    )
}

fn output_scale(output: &OutputInfo) -> f64 {
    if output.scale.is_finite() && output.scale > 0.0 {
        output.scale
    } else {
        1.0
    }
}

fn scaled_extent(logical: u32, scale: f64) -> u32 {
    (logical as f64 * scale).round().max(1.0) as u32
}

fn shadow_pixels(scale: f64) -> u32 {
    (SHADOW * scale).round().max(1.0) as u32
}

fn render_image_size(image: &ImageBuffer, zoom: f64, scale: f64) -> (u32, u32) {
    let (width, height) = image_logical_size(image);
    (
        (width * zoom * scale).round().max(1.0) as u32,
        (height * zoom * scale).round().max(1.0) as u32,
    )
}

fn initial_zoom(image: &ImageBuffer, output: &OutputInfo) -> f64 {
    let (width, height) = image_logical_size(image);
    let fit_x = output.logical_width as f64 * 0.9 / width;
    let fit_y = output.logical_height as f64 * 0.9 / height;
    fit_x.min(fit_y).min(1.0).clamp(MIN_ZOOM, MAX_ZOOM)
}

fn initial_position(
    image: &ImageBuffer,
    output: &OutputInfo,
    zoom: f64,
    place_at_region: bool,
) -> (f64, f64) {
    let (width, height) = image_logical_size(image);
    let (width, height) = (width * zoom, height * zoom);
    let (mut x, mut y) = if place_at_region && !image.region.is_empty() {
        (
            image.region.x as f64 + image.region.width as f64 / 2.0 - width / 2.0,
            image.region.y as f64 + image.region.height as f64 / 2.0 - height / 2.0,
        )
    } else {
        (
            output.x as f64 + (output.logical_width as f64 - width) / 2.0,
            output.y as f64 + (output.logical_height as f64 - height) / 2.0,
        )
    };
    let right = output.x as f64 + output.logical_width as f64 - width;
    let bottom = output.y as f64 + output.logical_height as f64 - height;
    x = if right < output.x as f64 {
        output.x as f64 + (output.logical_width as f64 - width) / 2.0
    } else {
        x.clamp(output.x as f64, right)
    };
    y = if bottom < output.y as f64 {
        output.y as f64 + (output.logical_height as f64 - height) / 2.0
    } else {
        y.clamp(output.y as f64, bottom)
    };
    (x, y)
}

fn clamp_partially_visible(
    position: (f64, f64),
    width: f64,
    height: f64,
    bounds: Rect,
    visible: f64,
) -> (f64, f64) {
    let clamp_axis = |value: f64, size: f64, start: f64, extent: f64| {
        let low = start - size + visible;
        let high = start + extent - visible;
        if low > high {
            start + (extent - size) / 2.0
        } else {
            value.clamp(low, high)
        }
    };
    (
        clamp_axis(position.0, width, bounds.x, bounds.width),
        clamp_axis(position.1, height, bounds.y, bounds.height),
    )
}

fn clip_input_rect(
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    surface_width: i64,
    surface_height: i64,
) -> Option<(i32, i32, u32, u32)> {
    let left = x.max(0);
    let top = y.max(0);
    let right = (x + width).min(surface_width);
    let bottom = (y + height).min(surface_height);
    (right > left && bottom > top).then(|| {
        (
            left as i32,
            top as i32,
            (right - left) as u32,
            (bottom - top) as u32,
        )
    })
}

fn rounded_rect_sdf(px: f64, py: f64, x: f64, y: f64, width: f64, height: f64, radius: f64) -> f64 {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let center_x = px - (x + width / 2.0);
    let center_y = py - (y + height / 2.0);
    let qx = center_x.abs() - (width / 2.0 - radius);
    let qy = center_y.abs() - (height / 2.0 - radius);
    let outside_x = qx.max(0.0);
    let outside_y = qy.max(0.0);
    (outside_x * outside_x + outside_y * outside_y).sqrt() + qx.max(qy).min(0.0) - radius
}

fn axis_steps(scroll: &AxisScroll) -> f64 {
    if scroll.value120 != 0 {
        -(scroll.value120 as f64) / 120.0
    } else if scroll.discrete != 0 {
        -(scroll.discrete as f64)
    } else if scroll.absolute != 0.0 {
        -scroll.absolute.signum()
    } else {
        0.0
    }
}

impl CompositorHandler for FloatState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let Some(index) = self
            .surfaces
            .iter()
            .position(|item| item.layer.wl_surface() == surface)
        else {
            return;
        };
        self.surfaces[index].frame_pending = false;
        if self.surfaces[index].redraw_pending {
            self.redraw(index, qh);
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for FloatState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.running = false;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(index) = self
            .surfaces
            .iter()
            .position(|surface| surface.layer.wl_surface() == layer.wl_surface())
        {
            if configure.new_size.0 > 0 {
                self.surfaces[index].logical_width = configure.new_size.0;
            }
            if configure.new_size.1 > 0 {
                self.surfaces[index].logical_height = configure.new_size.1;
            }
            self.surfaces[index].configured = true;
            self.redraw(index, qh);
        }
    }
}

impl SeatHandler for FloatState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer.take();
        }
        if capability == Capability::Keyboard {
            self.keyboard.take();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for FloatState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(index) = self
                .surfaces
                .iter()
                .position(|surface| surface.layer.wl_surface() == &event.surface)
            else {
                continue;
            };
            if matches!(&event.kind, PointerEventKind::Leave { .. }) {
                if self.dragging.is_some() {
                    continue;
                }
                continue;
            }
            match &event.kind {
                PointerEventKind::Press { button, .. } => match *button {
                    LEFT_BUTTON => {
                        self.dragging = Some(Drag {
                            output_index: index,
                            start_pointer: self.pointer_global(index, event.position),
                            start_image: self.image_pos,
                        });
                    }
                    MIDDLE_BUTTON => self.running = false,
                    _ => {}
                },
                PointerEventKind::Motion { .. } => {
                    if let Some(drag) = &self.dragging {
                        let global = self.pointer_global(drag.output_index, event.position);
                        self.image_pos = (
                            drag.start_image.0 + global.0 - drag.start_pointer.0,
                            drag.start_image.1 + global.1 - drag.start_pointer.1,
                        );
                        self.clamp_visible();
                        self.geometry_changed(qh);
                    }
                }
                PointerEventKind::Release { button, .. } if *button == LEFT_BUTTON => {
                    self.dragging = None;
                    let now = Instant::now();
                    let double_click = self.last_click.is_some_and(|(time, position)| {
                        now.duration_since(time) <= Duration::from_millis(DOUBLE_CLICK_MS)
                            && (position.0 - event.position.0).abs() <= DOUBLE_CLICK_DISTANCE
                            && (position.1 - event.position.1).abs() <= DOUBLE_CLICK_DISTANCE
                    });
                    if double_click {
                        self.running = false;
                    } else {
                        self.last_click = Some((now, event.position));
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    let steps = axis_steps(vertical);
                    if steps != 0.0 {
                        self.zoom_by(steps, qh);
                    }
                }
                _ => {}
            }
        }
    }
}

impl KeyboardHandler for FloatState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.keysym {
            Keysym::Escape => self.running = false,
            Keysym::o | Keysym::O => open_path(&self.path),
            Keysym::f | Keysym::F => {
                if let Some(parent) = self.path.parent() {
                    open_path(parent);
                }
            }
            _ => {}
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: smithay_client_toolkit::seat::keyboard::Modifiers,
        _: smithay_client_toolkit::seat::keyboard::RawModifiers,
        _: u32,
    ) {
    }
}

fn open_path(path: &Path) {
    let _ = Command::new("xdg-open").arg(path).spawn();
}

impl OutputHandler for FloatState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for FloatState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for FloatState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(FloatState);
smithay_client_toolkit::delegate_dispatch2!(FloatState);
wayland_client::delegate_noop!(FloatState: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(FloatState: ignore WpViewporter);
wayland_client::delegate_noop!(FloatState: ignore WpViewport);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imagebuf::{ImageBuffer, Region};

    fn output(name: &str, x: i32, width: u32, height: u32) -> OutputInfo {
        OutputInfo {
            id: 0,
            name: name.into(),
            physical_width: width,
            physical_height: height,
            x,
            y: 0,
            logical_width: width,
            logical_height: height,
            scale: 1.0,
            transform: 0,
        }
    }

    #[test]
    fn initial_zoom_fits_large_images_and_keeps_small_ones() {
        let output = output("DP-1", 0, 1000, 800);
        let mut image = ImageBuffer::new(2000, 800).unwrap();
        image.scale = 1.0;
        assert!((initial_zoom(&image, &output) - 0.45).abs() < 0.001);
        let image = ImageBuffer::new(200, 200).unwrap();
        assert!((initial_zoom(&image, &output) - 1.0).abs() < 0.001);
    }

    #[test]
    fn rendered_pixels_follow_the_output_scale() {
        let mut image = ImageBuffer::new(300, 150).unwrap();
        image.scale = 1.5;
        assert_eq!(render_image_size(&image, 1.0, 1.5), (300, 150));
        assert_eq!(scaled_extent(1280, 1.5), 1920);
        assert_eq!(render_image_size(&image, 1.0, 1.0), (200, 100));
    }

    #[test]
    fn region_position_uses_logical_center_and_clamps() {
        let output = output("DP-1", 100, 1000, 800);
        let mut image = ImageBuffer::new(200, 100).unwrap();
        image.scale = 2.0;
        image.region = Region {
            x: 150,
            y: 50,
            width: 100,
            height: 50,
            output: "DP-1".into(),
        };
        let position = initial_position(&image, &output, 1.0, true);
        assert!((position.0 - 150.0).abs() < 0.001);
        assert!((position.1 - 50.0).abs() < 0.001);
    }

    #[test]
    fn partial_visibility_keeps_a_clickable_margin() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 2000.0,
            height: 1000.0,
        };
        assert_eq!(
            clamp_partially_visible((-500.0, 990.0), 300.0, 200.0, bounds, 32.0),
            (-268.0, 968.0)
        );
    }

    #[test]
    fn input_region_is_clipped_to_the_surface() {
        assert_eq!(clip_input_rect(-5, 5, 20, 20, 10, 10), Some((0, 5, 10, 5)));
        assert_eq!(clip_input_rect(20, 20, 5, 5, 10, 10), None);
    }

    #[test]
    fn wheel_direction_maps_to_positive_zoom() {
        let scroll = AxisScroll {
            value120: -120,
            ..AxisScroll::default()
        };
        assert_eq!(axis_steps(&scroll), 1.0);
        let scroll = AxisScroll {
            discrete: 1,
            ..AxisScroll::default()
        };
        assert_eq!(axis_steps(&scroll), -1.0);
    }
}
