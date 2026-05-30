use crate::backend::{Backend, BackendWindow};
use crate::config::{Config, GlobalConfig};
use crate::error::{Error, Result};
use crate::notification::{Manager, Notification, NOTIFICATION_MESSAGE_TEMPLATE};
use cairo::{Context as CairoContext, Format, ImageSurface};
use colorsys::ColorAlpha;
use pango::{FontDescription, Layout as PangoLayout};
use pangocairo::functions as pango_functions;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::delegate_compositor;
use smithay_client_toolkit::delegate_layer;
use smithay_client_toolkit::delegate_output;
use smithay_client_toolkit::delegate_pointer;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::delegate_seat;
use smithay_client_toolkit::delegate_shm;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::{slot::SlotPool, Shm, ShmHandler};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tera::{Result as TeraResult, Tera, Value};
use calloop::channel::{channel as calloop_channel, Event as ChannelEvent, Sender as CalloopSender};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, QueueHandle};

/// Wayland backend.
pub struct Wayland {
    connection: Connection,
}

unsafe impl Send for Wayland {}
unsafe impl Sync for Wayland {}

impl Wayland {
    /// Initialize Wayland connection.
    pub fn init() -> Result<Self> {
        let connection = Connection::connect_to_env()
            .map_err(|e| Error::Wayland(format!("connection error: {e}")))?;
        Ok(Self { connection })
    }

    /// Create a Wayland window.
    pub fn create_window(&mut self, config: &GlobalConfig) -> Result<WaylandWindow> {
        let (globals, mut event_queue) = registry_queue_init(&self.connection)
            .map_err(|e| Error::Wayland(format!("registry init error: {e}")))?;
        let qh = event_queue.handle();

        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|e| Error::Wayland(e.to_string()))?;
        let layer_shell =
            LayerShell::bind(&globals, &qh).map_err(|e| Error::Wayland(e.to_string()))?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| Error::Wayland(e.to_string()))?;

        let surface = compositor.create_surface(&qh);
        let layer =
            layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("runst"), None);
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(-1);
        layer.set_margin(
            config.geometry.y.try_into()?,
            0,
            0,
            config.geometry.x.try_into()?,
        );
        layer.set_size(config.geometry.width, config.geometry.height);
        layer.commit();

        let pool = SlotPool::new(
            (config.geometry.width * config.geometry.height * 4) as usize,
            &shm,
        )
        .map_err(|e| Error::Wayland(e.to_string()))?;

        let (event_sender, event_channel) = calloop_channel::<WaylandEvent>();

        let mut state = WaylandState {
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state: OutputState::new(&globals, &qh),
            shm,
            connection: self.connection.clone(),
            first_configure: true,
            configured: false,
            width: config.geometry.width,
            height: config.geometry.height,
            layer,
            pool,
            pointer: None,
            pending_draw: None,
            window: WindowRenderState::new(&config.template, &config.font)?,
            on_press: None,
            manager: None,
            config: None,
        };

        event_queue
            .roundtrip(&mut state)
            .map_err(|e| Error::Wayland(format!("roundtrip error: {e}")))?;

        Ok(WaylandWindow {
            event_sender,
            template: Arc::new(state.window.template.clone()),
            runtime: Mutex::new(Some(WaylandRuntime {
                event_queue,
                state,
                event_channel,
            })),
        })
    }
}

impl Backend for Wayland {
    fn create_window(&mut self, config: &GlobalConfig) -> Result<BackendWindow> {
        Ok(BackendWindow::Wayland(Arc::new(Self::create_window(
            self, config,
        )?)))
    }

    fn show_window(&self, window: &BackendWindow) -> Result<()> {
        match window {
            BackendWindow::Wayland(window) => window.show(),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Init("invalid backend window".to_string())),
        }
    }

    fn hide_window(&self, window: &BackendWindow) -> Result<()> {
        match window {
            BackendWindow::Wayland(window) => window.hide(),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Init("invalid backend window".to_string())),
        }
    }

    fn handle_events(
        &self,
        window: Arc<BackendWindow>,
        manager: Manager,
        config: Arc<Config>,
        on_press: Arc<dyn Fn(&Notification) + Send + Sync>,
    ) -> Result<()> {
        match window.as_ref() {
            BackendWindow::Wayland(window) => {
                window.handle_events(manager, config, on_press)
            }
            #[allow(unreachable_patterns)]
            _ => Err(Error::Init("invalid backend window".to_string())),
        }
    }

    fn render_message(
        &self,
        window: &BackendWindow,
        notification: &Notification,
        urgency_text: Option<String>,
        unread_count: usize,
    ) -> Result<String> {
        match window {
            BackendWindow::Wayland(window) => {
                notification.render_message(&window.template, urgency_text, unread_count)
            }
            #[allow(unreachable_patterns)]
            _ => Err(Error::Init("invalid backend window".to_string())),
        }
    }
}

/// Wayland window wrapper.
pub struct WaylandWindow {
    event_sender: CalloopSender<WaylandEvent>,
    template: Arc<Tera>,
    runtime: Mutex<Option<WaylandRuntime>>,
}

unsafe impl Send for WaylandWindow {}
unsafe impl Sync for WaylandWindow {}

impl WaylandWindow {
    fn show(&self) -> Result<()> {
        let _ = self
            .event_sender
            .send(WaylandEvent::Show)
            .map_err(|e| Error::Wayland(format!("send error: {e}")))?;
        Ok(())
    }

    fn hide(&self) -> Result<()> {
        let _ = self
            .event_sender
            .send(WaylandEvent::Hide)
            .map_err(|e| Error::Wayland(format!("send error: {e}")))?;
        Ok(())
    }

    fn handle_events(
        &self,
        manager: Manager,
        config: Arc<Config>,
        on_press: Arc<dyn Fn(&Notification) + Send + Sync>,
    ) -> Result<()> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| Error::Init("wayland runtime lock poisoned".to_string()))?
            .take()
            .ok_or_else(|| Error::Init("wayland runtime already started".to_string()))?;
        let event_queue = runtime.event_queue;
        let mut event_loop: EventLoop<WaylandState> =
            EventLoop::try_new().map_err(|e| Error::Wayland(format!("loop error: {e}")))?;
        let loop_handle = event_loop.handle();

        WaylandSource::new(runtime.state.connection.clone(), event_queue)
            .insert(loop_handle.clone())
            .map_err(|e| Error::Wayland(format!("wayland source error: {e}")))?;

        let mut state = runtime.state;
        state.manager = Some(manager);
        state.config = Some(config);
        state.on_press = Some(on_press);

        loop_handle
            .insert_source(runtime.event_channel, |event, _, state| {
                match event {
                    ChannelEvent::Msg(msg) => {
                        let _ = state.handle_event(msg);
                    }
                    ChannelEvent::Closed => {}
                }
            })
            .map_err(|e| Error::Wayland(format!("channel error: {e}")))?;

        loop {
            event_loop
                .dispatch(std::time::Duration::from_millis(16), &mut state)
                .map_err(|e| Error::Wayland(format!("loop dispatch error: {e}")))?;
        }
    }
}

struct WaylandState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    connection: Connection,
    first_configure: bool,
    configured: bool,
    width: u32,
    height: u32,
    layer: LayerSurface,
    pool: SlotPool,
    pointer: Option<wl_pointer::WlPointer>,
    pending_draw: Option<DrawRequest>,
    window: WindowRenderState,
    on_press: Option<Arc<dyn Fn(&Notification) + Send + Sync>>,
    manager: Option<Manager>,
    config: Option<Arc<Config>>,
}

#[derive(Clone)]
struct DrawRequest {
    notification: Notification,
    unread_count: usize,
}

struct WindowRenderState {
    template: Tera,
    font: String,
}

struct WaylandRuntime {
    event_queue: wayland_client::EventQueue<WaylandState>,
    state: WaylandState,
    event_channel: calloop::channel::Channel<WaylandEvent>,
}

impl WaylandState {
    fn handle_event(&mut self, event: WaylandEvent) -> Result<()> {
        match event {
            WaylandEvent::Show => {
                self.layer.set_size(self.width, self.height);
                self.layer.commit();
                if self.configured {
                    if let Some(manager) = self.manager.as_ref() {
                        self.pending_draw = Some(DrawRequest {
                            notification: manager.get_last_unread(),
                            unread_count: manager.get_unread_count(),
                        });
                        self.layer.wl_surface().commit();
                    }
                }
            }
            WaylandEvent::Hide => {
                self.layer.set_size(1, 1);
                self.layer.commit();
            }
        }
        Ok(())
    }

    fn draw(&mut self, qh: &QueueHandle<Self>, request: DrawRequest) -> Result<()> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| Error::Init("missing config".to_string()))?;
        let urgency_config = config.get_urgency_config(&request.notification.urgency);
        urgency_config.run_commands(&request.notification)?;

        let window = &self.window;
        let message = request.notification.render_message(
            &window.template,
            urgency_config.text.clone(),
            request.unread_count,
        )?;

        let width = self.width.max(1);
        let height = self.height.max(1);
        let stride = (width * 4) as i32;

        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wayland_client::protocol::wl_shm::Format::Argb8888,
            )
            .map_err(|e| Error::Wayland(e.to_string()))?;

        let surface = unsafe {
            ImageSurface::create_for_data_unsafe(
                canvas.as_mut_ptr(),
                Format::ARgb32,
                width as i32,
                height as i32,
                stride,
            )?
        };
        let cairo_context = CairoContext::new(&surface)?;
        let pango_context = pango_functions::create_context(&cairo_context);
        let layout = PangoLayout::new(&pango_context);
        let font_description = FontDescription::from_string(&window.font);
        pango_context.set_font_description(&font_description);

        let background_color = urgency_config.background;
        cairo_context.set_source_rgba(
            background_color.red() / 255.0,
            background_color.green() / 255.0,
            background_color.blue() / 255.0,
            background_color.alpha(),
        );
        cairo_context.fill()?;
        cairo_context.paint()?;

        let foreground_color = urgency_config.foreground;
        cairo_context.set_source_rgba(
            foreground_color.red() / 255.0,
            foreground_color.green() / 255.0,
            foreground_color.blue() / 255.0,
            foreground_color.alpha(),
        );
        cairo_context.move_to(0., 0.);
        layout.set_markup(&message);

        if config.global.wrap_content {
            let (new_width, new_height) = layout.pixel_size();
            let new_width = new_width.max(1) as u32;
            let new_height = new_height.max(1) as u32;
            self.width = new_width;
            self.height = new_height;
            self.layer.set_size(self.width, self.height);
        }

        pango_functions::show_layout(&cairo_context, &layout);

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());
        buffer
            .attach_to(self.layer.wl_surface())
            .map_err(|e| Error::Wayland(format!("buffer attach error: {e}")))?;
        self.layer.commit();
        Ok(())
    }
}

impl WindowRenderState {
    fn new(template_raw: &str, font: &str) -> Result<Self> {
        let mut template = Tera::default();
        if let Err(e) = template.add_raw_template(NOTIFICATION_MESSAGE_TEMPLATE, template_raw.trim()) {
            return if let Some(error_source) = e.source() {
                Err(Error::TemplateParse(error_source.to_string()))
            } else {
                Err(Error::Template(e))
            };
        }
        template.register_filter(
            "humantime",
            |value: &Value, _: &HashMap<String, Value>| -> TeraResult<Value> {
                let value = tera::try_get_value!("humantime_filter", "value", u64, value);
                let value = humantime::format_duration(Duration::new(value, 0)).to_string();
                Ok(tera::to_value(value)?)
            },
        );
        Ok(Self {
            template,
            font: font.to_string(),
        })
    }
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(request) = self.pending_draw.take() {
            let _ = self.draw(qh, request);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {}

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        self.configured = true;
        if self.first_configure {
            self.first_configure = false;
            if let Some(request) = self.pending_draw.take() {
                let _ = self.draw(qh, request);
            }
        }
    }
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.pointer = Some(pointer);
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
    }
}

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            if matches!(event.kind, PointerEventKind::Press { .. }) {
                if let (Some(manager), Some(on_press)) =
                    (self.manager.as_ref(), self.on_press.as_ref())
                {
                    let notification = manager.get_last_unread();
                    manager.mark_last_as_read();
                    on_press(&notification);
                }
            }
        }
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_shm!(WaylandState);
delegate_seat!(WaylandState);
delegate_pointer!(WaylandState);
delegate_layer!(WaylandState);
delegate_registry!(WaylandState);

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}

#[derive(Clone)]
enum WaylandEvent {
    Show,
    Hide,
}
