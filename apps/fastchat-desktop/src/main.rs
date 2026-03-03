use eframe::egui;
use fastchat_core::{AppPaths, ConfigRepository, WindowConfig};
use tracing_subscriber::{EnvFilter, fmt};

fn main() -> eframe::Result {
    init_tracing();

    let startup_window = load_startup_window_config();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Fast Chat")
        .with_inner_size([
            startup_window.width.max(640.0),
            startup_window.height.max(420.0),
        ]);
    if let (Some(x), Some(y)) = (startup_window.pos_x, startup_window.pos_y) {
        viewport = viewport.with_position(egui::pos2(x, y));
    }
    if startup_window.maximized {
        viewport = viewport.with_maximized(true);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Fast Chat",
        native_options,
        Box::new(|cc| {
            let app = fastchat_ui::FastChatApp::new(cc)
                .unwrap_or_else(|err| panic!("failed to initialize Fast Chat app: {err:#}"));
            Ok(Box::new(app))
        }),
    )
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

fn load_startup_window_config() -> WindowConfig {
    match AppPaths::discover() {
        Ok(paths) => {
            let repo = ConfigRepository::new(&paths);
            match repo.load_or_default() {
                Ok(config) => config.window,
                Err(_) => WindowConfig::default(),
            }
        }
        Err(_) => WindowConfig::default(),
    }
}
