#[deny(missing_docs)]

use egui::{epaint::ClippedMesh, Context, CtxRef, Event, Output, Pos2, RawInput, Rect, Vec2};
use egui_glow::{
    glow::{self, Context as GlowContext, HasContext},
    painter::Painter,
};

use smithay::{
    backend::{
        input::{Device, DeviceCapability, MouseButton},
        renderer::gles2::{Gles2Frame, Gles2Renderer},
    },
    utils::{Logical, Physical, Rectangle, Size},
    wayland::seat::{Keysym, ModifiersState},
};

#[cfg(feature = "render_element")]
use smithay::{
    backend::renderer::gles2::{Gles2Error, Gles2Texture},
    desktop::{space::RenderElement, Space},
    utils::Point,
    wayland::output::Output as WlOutput,
};

#[cfg(feature = "render_element")]
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

mod types;
pub use self::types::{convert_button, convert_key, convert_modifiers};

#[cfg(feature = "render_element")]
static EGUI_ID: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "render_element")]
lazy_static::lazy_static! {
    static ref EGUI_IDS: Mutex<HashSet<usize>> = Mutex::new(HashSet::new());
}
#[cfg(feature = "render_element")]
fn next_id() -> usize {
    let mut ids = EGUI_IDS.lock().unwrap();
    debug_assert!(!ids.len() == usize::MAX);
    let mut id = EGUI_ID.fetch_add(1, Ordering::SeqCst);
    while ids.iter().any(|k| *k == id) {
        id = EGUI_ID.fetch_add(1, Ordering::SeqCst);
    }

    ids.insert(id);
    id
}

/// Global smithay-egui state
pub struct EguiState {
    id: usize,
    ctx: CtxRef,
    pointers: usize,
    last_pointer_position: Point<i32, Logical>,
    events: Vec<Event>,
}

/// A single rendered egui interface frame
pub struct EguiFrame {
    state_id: usize,
    ctx: CtxRef,
    _output: Output,
    mesh: Vec<ClippedMesh>,
    scale: f64,
    area: Rect,
    size: Size<i32, Physical>,
}

impl EguiState {
    /// Creates a new `EguiState`
    pub fn new() -> EguiState {
        EguiState {
            id: next_id(),
            ctx: CtxRef::default(),
            pointers: 0,
            last_pointer_position: (0, 0).into(),
            events: Vec::new(),
        }
    }

    /// Retrieve the underlying [`egui::Context`]
    pub fn context(&self) -> &Context {
        &*self.ctx
    }

    /// If true, egui is currently listening on text input (e.g. typing text in a TextEdit).
    pub fn wants_keyboard(&self) -> bool {
        self.ctx.wants_keyboard_input()
    }

    /// True if egui is currently interested in the pointer (mouse or touch).
    /// Could be the pointer is hovering over a Window or the user is dragging a widget.
    /// If false, the pointer is outside of any egui area and so you may want to forward it to other clients as usual.
    /// Returns false if a drag started outside of egui and then moved over an egui area.
    pub fn wants_pointer(&self) -> bool {
        self.ctx.wants_pointer_input()
    }

    /// Pass new input devices to `EguiState` for internal tracking
    pub fn handle_device_added(&mut self, device: &impl Device) {
        if device.has_capability(DeviceCapability::Pointer) {
            self.pointers += 1;
        }
    }

    /// Remove input devices to `EguiState` for internal tracking
    pub fn handle_device_removed(&mut self, device: &impl Device) {
        if device.has_capability(DeviceCapability::Pointer) {
            self.pointers -= 1;
        }
        if self.pointers == 0 {
            self.events.push(Event::PointerGone);
        }
    }

    /// Pass keyboard events into `EguiState`.
    /// 
    /// You do not want to pass in events, egui should not react to, but you need to make sure they add up.
    /// So for every pressed event, you want to send a released one.
    /// 
    /// You likely want to use the filter-closure of [`smithay::wayland::seat::KeyboardHandle::input`] to optain these values.
    /// Use [`smithay::wayland::seat::KeysymHandle::raw_syms`] and the provided [`smithay::wayland::seat::ModifiersState`].
    pub fn handle_keyboard(
        &mut self,
        raw_syms: &[Keysym],
        pressed: bool,
        modifiers: ModifiersState,
    ) {
        if let Some(key) = dbg!(convert_key(raw_syms.iter().copied())) {
            self.events.push(Event::Key {
                key,
                pressed,
                modifiers: convert_modifiers(modifiers),
            });
        }
    }

    /// Pass new pointer coordinates to `EguiState`
    pub fn handle_pointer_motion(&mut self, position: Point<i32, Logical>) {
        self.last_pointer_position = position;
        self.events.push(Event::PointerMoved(Pos2::new(
            position.x as f32,
            position.y as f32,
        )))
    }

    /// Pass pointer button presses to `EguiState`
    /// 
    /// Note: If you are unsure about *which* PointerButtonEvents to send to smithay-egui
    ///       instead of normal clients, check [`EguiState::wants_pointer`] to figure out,
    ///       if there is an egui-element below your pointer.
    pub fn handle_pointer_button(
        &mut self,
        button: MouseButton,
        pressed: bool,
        modifiers: ModifiersState,
    ) {
        if let Some(button) = convert_button(button) {
            self.events.push(Event::PointerButton {
                pos: Pos2::new(
                    self.last_pointer_position.x as f32,
                    self.last_pointer_position.y as f32,
                ),
                button,
                pressed,
                modifiers: convert_modifiers(modifiers),
            })
        }
    }

    /// Pass a pointer axis scrolling to `EguiState`
    /// 
    /// Note: If you are unsure about *which* PointerAxisEvents to send to smithay-egui
    ///       instead of normal clients, check [`EguiState::wants_pointer`] to figure out,
    ///       if there is an egui-element below your pointer.
    pub fn handle_pointer_axis(&mut self, x_amount: f64, y_amount: f64) {
        self.events.push(Event::Scroll(Vec2 {
            x: x_amount as f32,
            y: y_amount as f32,
        }))
    }

    // TODO: touch inputs

    /// Produce a new frame of egui to draw onto your output buffer.
    /// 
    /// - `ui` is your drawing function
    /// - `area` limits the space egui will be using.
    /// - `size` has to be the total size of the buffer the ui will be displayed in
    /// - `scale` is the scale egui should render in
    /// - `start_time` need to be a fixed point in time before the first `run` call to measure animation-times and the like.
    /// - `modifiers` should be the current state of modifiers pressed on the keyboards.
    pub fn run(
        &mut self,
        ui: impl FnOnce(&CtxRef),
        area: Rectangle<i32, Logical>,
        size: Size<i32, Physical>,
        scale: f64,
        start_time: &std::time::Instant,
        modifiers: ModifiersState,
    ) -> EguiFrame {
        let area = area.to_f64().to_physical(scale).to_i32_round::<i32>();
        let input = RawInput {
            screen_rect: Some(Rect {
                min: Pos2 {
                    x: area.loc.x as f32,
                    y: area.loc.y as f32,
                },
                max: Pos2 {
                    x: area.loc.x as f32 + area.size.w as f32,
                    y: area.loc.y as f32 + area.size.h as f32,
                },
            }),
            pixels_per_point: Some(scale as f32),
            time: Some(start_time.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / 60.0,
            modifiers: convert_modifiers(modifiers),
            events: self.events.drain(..).collect(),
            hovered_files: Vec::with_capacity(0),
            dropped_files: Vec::with_capacity(0),
        };

        let (_output, shapes) = self.ctx.run(input, ui);
        EguiFrame {
            state_id: self.id,
            ctx: self.ctx.clone(),
            _output,
            mesh: self.ctx.tessellate(shapes),
            scale,
            area: self.ctx.used_rect(),
            size,
        }
    }
}

impl EguiFrame {
    /// Draw this frame in the currently active GL-context
    pub unsafe fn draw(&self) -> Result<(), String> {
        // TODO: cache this somehow
        let context = GlowContext::from_loader_function(|sym| smithay::backend::egl::get_proc_address(sym));
        let mut painter = Painter::new(&context, None, "")?;
        painter.upload_egui_texture(&context, &*self.ctx.font_image());

        painter.paint_meshes(
            &context,
            [self.size.w as u32, self.size.h as u32],
            self.scale as f32,
            self.mesh.clone(),
        );

        context.disable(glow::SCISSOR_TEST);
        context.disable(glow::BLEND);
        painter.destroy(&context);

        Ok(())
    }
}

#[cfg(feature = "render_element")]
impl RenderElement<Gles2Renderer, Gles2Frame, Gles2Error, Gles2Texture> for EguiFrame {
    fn id(&self) -> usize {
        self.state_id
    }

    fn geometry(&self) -> Rectangle<i32, Logical> {
        Rectangle::<f64, Physical>::from_extemities(
            (self.area.min.x as f64, self.area.min.y as f64),
            (self.area.max.x as f64, self.area.max.y as f64),
        )
        .to_logical(self.scale)
        .to_i32_round()
    }

    fn accumulated_damage(
        &self,
        _for_values: Option<(&Space, &WlOutput)>,
    ) -> Vec<Rectangle<i32, Logical>> {
        vec![Rectangle::from_loc_and_size((0, 0), (i32::MAX, i32::MAX))]
    }

    fn draw(
        &self,
        _renderer: &mut Gles2Renderer,
        _frame: &mut Gles2Frame,
        _scale: f64,
        _location: Point<i32, Logical>,
        _damage: &[Rectangle<i32, Logical>],
        log: &slog::Logger,
    ) -> Result<(), Gles2Error> {
        if let Err(err) = unsafe { EguiFrame::draw(self) } {
            slog::error!(log, "egui rendering error: {}", err);
        }
        Ok(())
    }
}
