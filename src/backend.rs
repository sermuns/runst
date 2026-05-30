use crate::config::{Config, GlobalConfig};
use crate::error::Result;
use crate::notification::{Manager, Notification};
use std::sync::Arc;

/// Rendering and event backend interface.
pub trait Backend: Send + Sync {
    /// Create a backend window.
    fn create_window(&mut self, config: &GlobalConfig) -> Result<BackendWindow>;
    /// Show the backend window.
    fn show_window(&self, window: &BackendWindow) -> Result<()>;
    /// Hide the backend window.
    fn hide_window(&self, window: &BackendWindow) -> Result<()>;
    /// Run the backend event loop.
    fn handle_events(
        &self,
        window: Arc<BackendWindow>,
        manager: Manager,
        config: Arc<Config>,
        on_press: Arc<dyn Fn(&Notification) + Send + Sync>,
    ) -> Result<()>;
    /// Render a notification message for read-time estimation.
    fn render_message(
        &self,
        window: &BackendWindow,
        notification: &Notification,
        urgency_text: Option<String>,
        unread_count: usize,
    ) -> Result<String>;
}

/// Backend window variants.
pub enum BackendWindow {
    #[cfg(feature = "x11")]
    /// X11 window wrapper.
    X11(Arc<crate::x11::X11Window>),
    #[cfg(feature = "wayland")]
    /// Wayland window wrapper.
    Wayland(Arc<crate::wayland::WaylandWindow>),
}
