#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod alsa_backend;
mod config;
mod engine;
mod midi;
mod tray;

use config::{AppSettings, Config, default_user_config_path, ensure_user_config_exists};
use eframe::egui;
use midi::{LogLevel, MidiEngine, SharedLogger, list_midi_out_ports};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tray::{TrayMenuIds, build_tray, build_tray_menu, generate_tray_icon};
use tray_icon::TrayIcon;
use tray_icon::menu::MenuEvent;

#[cfg(target_os = "linux")]
static ORIGINAL_WAYLAND_DISPLAY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
#[cfg(target_os = "linux")]
static ORIGINAL_WAYLAND_SOCKET: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
#[cfg(target_os = "linux")]
static ORIGINAL_GDK_BACKEND: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
fn prepare_child_command(mut cmd: Command) -> Command {
    if let Some(Some(val)) = ORIGINAL_WAYLAND_DISPLAY.get() {
        cmd.env("WAYLAND_DISPLAY", val);
    } else {
        cmd.env_remove("WAYLAND_DISPLAY");
    }
    if let Some(Some(val)) = ORIGINAL_WAYLAND_SOCKET.get() {
        cmd.env("WAYLAND_SOCKET", val);
    } else {
        cmd.env_remove("WAYLAND_SOCKET");
    }
    if let Some(Some(val)) = ORIGINAL_GDK_BACKEND.get() {
        cmd.env("GDK_BACKEND", val);
    } else {
        cmd.env_remove("GDK_BACKEND");
    }
    cmd
}

#[cfg(not(target_os = "linux"))]
fn prepare_child_command(cmd: Command) -> Command {
    cmd
}

#[cfg(target_os = "linux")]
fn is_tui_editor(bin: &str) -> bool {
    let name = Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(bin);
    matches!(
        name,
        "vi" | "vim" | "nvim" | "nano" | "micro" | "helix" | "hx" | "emacs" | "kak" | "joe" | "ne"
    )
}

#[cfg(target_os = "linux")]
fn find_linux_terminal() -> Option<&'static str> {
    const TERMINALS: &[&str] = &[
        "x-terminal-emulator",
        "kitty",
        "alacritty",
        "wezterm",
        "konsole",
        "gnome-terminal",
        "xfce4-terminal",
        "xterm",
    ];
    for &term in TERMINALS {
        if let Ok(status) = prepare_child_command(Command::new("sh"))
            .arg("-c")
            .arg(format!("command -v {} >/dev/null 2>&1", term))
            .status()
            && status.success()
        {
            return Some(term);
        }
    }
    None
}

fn spawn_editor(path: &Path, logger: &SharedLogger) -> anyhow::Result<()> {
    let editor = env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| {
            #[cfg(target_os = "windows")]
            {
                "notepad".to_string()
            }
            #[cfg(not(target_os = "windows"))]
            {
                "vi".to_string()
            }
        });

    let parts: Vec<&str> = editor.split_whitespace().collect();
    let bin = parts.first().copied().unwrap_or("vi");
    let extra_args = &parts[1..];

    #[cfg(target_os = "linux")]
    if is_tui_editor(bin)
        && let Some(term) = find_linux_terminal()
    {
        logger.info(format!(
            "Opening '{}' with '{}' (in {})...",
            path.display(),
            editor,
            term
        ));
        let mut cmd = prepare_child_command(Command::new(term));
        match term {
            "gnome-terminal" => {
                cmd.arg("--").arg(bin);
            }
            "xfce4-terminal" => {
                cmd.arg("-x").arg(bin);
            }
            "wezterm" => {
                cmd.arg("start").arg("--always-new-process").arg("--").arg(bin);
            }
            "kitty" => {
                cmd.arg("--").arg(bin);
            }
            _ => {
                cmd.arg("-e").arg(bin);
            }
        }
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.arg(path);
        cmd.spawn()?;
        return Ok(());
    }

    logger.info(format!("Opening '{}' with '{}'...", path.display(), editor));
    let mut cmd = prepare_child_command(Command::new(bin));
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg(path);
    cmd.spawn()?;
    Ok(())
}

struct ReMidiCore {
    logger: SharedLogger,
    engine: MidiEngine,
    config_path: PathBuf,
    settings_path: PathBuf,
    tray_icon: Option<TrayIcon>,
    tray_ids: Option<TrayMenuIds>,
    cached_midi_ports: Vec<String>,
    last_port_check: Instant,
    window_visible: bool,
    start_minimized: bool,
    icon_has_error: bool,
}

impl ReMidiCore {
    fn new(logger: SharedLogger, engine: MidiEngine, config_path: PathBuf) -> Self {
        #[cfg(target_os = "linux")]
        {
            let _ = gtk::init();
        }

        let settings_path = config_path.with_file_name("settings.toml");
        let settings = AppSettings::load_from_file(&settings_path);

        logger.set_log_level(settings.log_level);
        logger.paused.store(settings.pause_all, Ordering::Relaxed);

        let cached_midi_ports = list_midi_out_ports();
        let (tray_icon, tray_ids) = match build_tray(
            &[],
            &cached_midi_ports,
            settings.log_level,
            settings.pause_all,
        ) {
            Ok((t, ids)) => (Some(t), Some(ids)),
            Err(e) => {
                logger.error(format!("Warning: Could not initialize system tray icon: {}", e));
                (None, None)
            }
        };

        let initial_window_visible = !(settings.start_minimized && tray_icon.is_some());

        let mut core = Self {
            logger,
            engine,
            config_path,
            settings_path,
            tray_icon,
            tray_ids,
            cached_midi_ports,
            last_port_check: Instant::now(),
            window_visible: initial_window_visible,
            start_minimized: settings.start_minimized,
            icon_has_error: false,
        };

        core.logger.info("----- ReMIDI -----");
        if let Err(e) = ensure_user_config_exists(&core.config_path) {
            core.logger.error(format!("{:#}", e));
        }
        core.reload_config();

        // If no system tray is available on this desktop environment, keep the window visible
        if core.tray_icon.is_none() {
            core.window_visible = true;
        }

        core.update_tray_icon();
        core
    }

    fn save_settings(&self) {
        let settings = AppSettings {
            start_minimized: self.start_minimized,
            log_level: self.logger.log_level(),
            pause_all: self.logger.paused.load(Ordering::Relaxed),
        };
        if let Err(e) = settings.save_to_file(&self.settings_path) {
            self.logger.error(format!("Failed to save settings: {:#}", e));
        }
    }

    fn set_log_level(&mut self, new_level: LogLevel) {
        if self.logger.log_level() == new_level {
            return;
        }
        self.logger.set_log_level(new_level);
        if let Some(ids) = &self.tray_ids {
            ids.log_level_items.item_error.set_checked(new_level == LogLevel::Error);
            ids.log_level_items.item_transform.set_checked(new_level == LogLevel::Transform);
            ids.log_level_items.item_info.set_checked(new_level == LogLevel::Info);
            ids.log_level_items.item_verbose.set_checked(new_level == LogLevel::Verbose);
        }
        self.logger.info(format!("Log level set to {}", new_level));
        self.save_settings();
    }

    fn rebuild_tray_menu(&mut self) {
        let Some(tray) = &mut self.tray_icon else {
            return;
        };
        let pipelines = self
            .engine
            .current_config()
            .map(|c| c.pipelines.as_slice())
            .unwrap_or(&[]);
        let log_level = self.logger.log_level();
        let paused = self.logger.paused.load(Ordering::Relaxed);
        self.cached_midi_ports = list_midi_out_ports();
        if let Ok((menu, ids)) =
            build_tray_menu(pipelines, &self.cached_midi_ports, log_level, paused)
        {
            tray.set_menu(Some(Box::new(menu)));
            self.tray_ids = Some(ids);
        }
    }

    fn reload_config(&mut self) {
        self.logger
            .info(format!("Loading config '{}'...", self.config_path.display()));
        match Config::load_from_file(&self.config_path) {
            Ok(cfg) => {
                if let Err(e) = self.engine.load_config(cfg) {
                    self.logger.error(format!("{:#}", e));
                } else {
                    self.icon_has_error = false;
                    self.update_tray_icon();
                }
            }
            Err(e) => {
                self.logger.error(format!("{:#}", e));
            }
        }
        self.rebuild_tray_menu();
    }

    fn edit_current_config(&mut self) {
        if let Err(e) = spawn_editor(&self.config_path, &self.logger) {
            self.logger.error(format!(
                "Error launching editor for '{}': {}",
                self.config_path.display(),
                e
            ));
        }
    }

    fn toggle_pipeline_enabled(&mut self, pipeline_idx: usize) {
        let Some(cfg) = self.engine.current_config() else {
            return;
        };
        let Some(p) = cfg.pipelines.get(pipeline_idx) else {
            return;
        };
        let new_enabled = !p.enabled;
        if let Err(e) =
            Config::save_pipeline_update(&self.config_path, pipeline_idx, Some(new_enabled), None)
        {
            self.logger
                .error(format!("Failed to save pipeline state: {:#}", e));
        }
        self.reload_config();
    }

    fn select_pipeline_target_port(&mut self, pipeline_idx: usize, port_name: String) {
        if let Err(e) = Config::save_pipeline_update(
            &self.config_path,
            pipeline_idx,
            None,
            Some(&port_name),
        ) {
            self.logger
                .error(format!("Failed to save target_port to config.toml: {:#}", e));
        }
        self.engine
            .set_pipeline_target_port(pipeline_idx, port_name);
        self.rebuild_tray_menu();
    }

    fn update_tray_icon(&mut self) {
        if let Some(tray) = &mut self.tray_icon {
            let paused = self.logger.paused.load(Ordering::Relaxed);
            let _ = tray.set_icon(Some(generate_tray_icon(self.icon_has_error, paused)));
        }
    }

    fn poll_events(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let mut iter_count = 0;
            while gtk::events_pending() && iter_count < 64 {
                gtk::main_iteration_do(false);
                iter_count += 1;
            }
        }

        // Periodically check if connected MIDI-out ports changed so tray menu stays up to date
        if self.last_port_check.elapsed() >= Duration::from_secs(3) {
            self.last_port_check = Instant::now();
            let current_ports = list_midi_out_ports();
            if current_ports != self.cached_midi_ports {
                self.rebuild_tray_menu();
            }
        }

        // Automatically pop up the window if an error occurred!
        if self.logger.take_unseen_error() {
            self.icon_has_error = true;
            self.update_tray_icon();
            self.window_visible = true;
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let Some(ids) = &self.tray_ids else {
                continue;
            };
            if event.id == ids.toggle_window {
                self.window_visible = !self.window_visible;
            } else if event.id == ids.edit_config {
                self.edit_current_config();
            } else if event.id == ids.reload_config {
                self.logger.info("reloading config...");
                self.reload_config();
            } else if event.id == ids.log_level_items.item_error.id() {
                self.set_log_level(LogLevel::Error);
            } else if event.id == ids.log_level_items.item_transform.id() {
                self.set_log_level(LogLevel::Transform);
            } else if event.id == ids.log_level_items.item_info.id() {
                self.set_log_level(LogLevel::Info);
            } else if event.id == ids.log_level_items.item_verbose.id() {
                self.set_log_level(LogLevel::Verbose);
            } else if event.id == ids.toggle_pause.id() {
                let new_val = !self.logger.paused.load(Ordering::Relaxed);
                self.logger.paused.store(new_val, Ordering::Relaxed);
                ids.toggle_pause.set_checked(new_val);
                self.logger.info(if new_val {
                    "MIDI processing PAUSED"
                } else {
                    "MIDI processing RESUMED"
                });
                self.update_tray_icon();
                self.save_settings();
            } else if event.id == ids.quit {
                self.engine.stop_all();
                std::process::exit(0);
            } else {
                // Check per-pipeline menu items
                let mut toggle_idx = None;
                let mut test_idx = None;
                let mut port_change = None;

                for p_entry in &ids.pipelines {
                    if event.id == p_entry.toggle_enabled.id() {
                        toggle_idx = Some(p_entry.pipeline_index);
                        break;
                    }
                    if event.id == p_entry.run_test {
                        test_idx = Some(p_entry.pipeline_index);
                        break;
                    }
                    for (check_item, port_name) in &p_entry.port_items {
                        if event.id == check_item.id() {
                            port_change = Some((p_entry.pipeline_index, port_name.clone()));
                            break;
                        }
                    }
                    if port_change.is_some() {
                        break;
                    }
                }

                if let Some(idx) = toggle_idx {
                    self.toggle_pipeline_enabled(idx);
                } else if let Some(idx) = test_idx {
                    self.engine.run_pipeline_test(idx);
                } else if let Some((idx, port_name)) = port_change {
                    self.select_pipeline_target_port(idx, port_name);
                }
            }
        }
    }
}

fn render_pipelines_menu(ui: &mut egui::Ui, core: &mut ReMidiCore) {
    let Some(cfg) = core.engine.current_config().cloned() else {
        ui.label(egui::RichText::new("No configuration loaded").weak());
        return;
    };
    if cfg.pipelines.is_empty() {
        ui.label(egui::RichText::new("No pipelines configured").weak());
        return;
    }

    let mut toggle_idx = None;
    let mut test_idx = None;
    let mut port_change = None;

    for (idx, p) in cfg.pipelines.iter().enumerate() {
        let label = format!("{} ({})", p.id, p.full_virtual_port_name());
        ui.menu_button(&label, |ui| {
            let mut enabled = p.enabled;
            if ui.checkbox(&mut enabled, "Enabled").changed() {
                ui.close_menu();
                toggle_idx = Some(idx);
            }

            let can_test = p.enabled && !p.test.is_empty();
            if ui
                .add_enabled(can_test, egui::Button::new("Run Test"))
                .clicked()
            {
                ui.close_menu();
                test_idx = Some(idx);
            }

            ui.separator();
            ui.label(egui::RichText::new("Target Output Port:").small().weak());

            let mut found_current = false;
            for port_name in &core.cached_midi_ports {
                let is_selected =
                    *port_name == p.target_port || port_name.contains(&p.target_port);
                if is_selected {
                    found_current = true;
                }
                let mut checked = is_selected;
                if ui.checkbox(&mut checked, port_name).clicked() {
                    ui.close_menu();
                    port_change = Some((idx, port_name.clone()));
                }
            }

            if !found_current && !p.target_port.is_empty() {
                let missing_label = format!("{} (disconnected)", p.target_port);
                let mut checked = true;
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(&mut checked, missing_label),
                );
            }
        });
    }

    if let Some(idx) = toggle_idx {
        core.toggle_pipeline_enabled(idx);
    }
    if let Some(idx) = test_idx {
        core.engine.run_pipeline_test(idx);
    }
    if let Some((idx, port)) = port_change {
        core.select_pipeline_target_port(idx, port);
    }
}

struct WindowApp {
    core: std::rc::Rc<std::cell::RefCell<ReMidiCore>>,
    last_visible: bool,
    startup_hidden_ticks: usize,
}

impl WindowApp {
    fn update_ui(&mut self, ctx: &egui::Context) {
        let mut core = self.core.borrow_mut();
        core.poll_events();

        if self.startup_hidden_ticks > 0 {
            self.startup_hidden_ticks -= 1;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            if core.tray_icon.is_none() {
                core.engine.stop_all();
                std::process::exit(0);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            core.window_visible = false;
            self.last_visible = false;
        }

        if core.window_visible != self.last_visible {
            self.last_visible = core.window_visible;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(core.window_visible));
            if core.window_visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        if !core.window_visible {
            ctx.request_repaint_after(Duration::from_millis(80));
            return;
        }

        ctx.request_repaint_after(Duration::from_millis(80));

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.menu_button("🔀 Pipelines", |ui| {
                    render_pipelines_menu(ui, &mut core);
                });

                if ui.button("✏ Edit Config (edit)").clicked() {
                    core.edit_current_config();
                }
                if ui.button("🔄 Reload (update)").clicked() {
                    core.logger.info("reloading config...");
                    core.reload_config();
                }

                if ui.button("🧪 Test all").clicked() {
                    core.engine.run_all_pipeline_tests();
                }

                ui.separator();

                let mut start_minimized = core.start_minimized;
                if ui.checkbox(&mut start_minimized, "Start minimized").changed() {
                    core.start_minimized = start_minimized;
                    core.save_settings();
                }

                let mut paused = core.logger.paused.load(Ordering::Relaxed);
                if ui.checkbox(&mut paused, "Pause all").changed() {
                    core.logger.paused.store(paused, Ordering::Relaxed);
                    if let Some(ids) = &core.tray_ids {
                        ids.toggle_pause.set_checked(paused);
                    }
                    core.update_tray_icon();
                    core.save_settings();
                }

                ui.label("Log level:");
                let mut current_lvl = core.logger.log_level();
                egui::ComboBox::from_id_salt("log_level_select")
                    .selected_text(current_lvl.as_str())
                    .show_ui(ui, |ui| {
                        for &lvl in &LogLevel::ALL {
                            if ui.selectable_value(&mut current_lvl, lvl, lvl.as_str()).clicked() {
                                core.set_log_level(lvl);
                            }
                        }
                    });

                if ui.button("🗑 Clear").clicked() {
                    core.logger.clear();
                    core.icon_has_error = false;
                    core.update_tray_icon();
                }

                ui.separator();

                if ui.button("Quit").clicked() {
                    core.engine.stop_all();
                    std::process::exit(0);
                }
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("Config: {}", core.config_path.display()))
                        .small()
                        .color(egui::Color32::from_rgb(160, 175, 195)),
                );
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let entries = core.logger.snapshot();
            let filter_level = core.logger.log_level();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for entry in entries {
                        if entry.level > filter_level {
                            continue;
                        }
                        let color = match entry.level {
                            LogLevel::Error => egui::Color32::from_rgb(255, 95, 85),
                            LogLevel::Transform => egui::Color32::from_rgb(80, 220, 130),
                            LogLevel::Info => egui::Color32::from_rgb(210, 215, 225),
                            LogLevel::Verbose => egui::Color32::from_rgb(140, 150, 165),
                        };
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("[{}]", entry.timestamp))
                                    .monospace()
                                    .color(egui::Color32::from_rgb(110, 120, 135)),
                            );
                            ui.label(
                                egui::RichText::new(&entry.message)
                                    .monospace()
                                    .color(color),
                            );
                        });
                    }
                });
        });
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "linux")]
    let use_x11 = {
        ORIGINAL_WAYLAND_DISPLAY.get_or_init(|| std::env::var("WAYLAND_DISPLAY").ok());
        ORIGINAL_WAYLAND_SOCKET.get_or_init(|| std::env::var("WAYLAND_SOCKET").ok());
        ORIGINAL_GDK_BACKEND.get_or_init(|| std::env::var("GDK_BACKEND").ok());

        // On Linux under Wayland, winit's Wayland backend cannot hide/unmap windows
        // (set_visible is an intentional no-op in winit on Wayland).
        // By forcing X11/XWayland when an X display/socket is available, the window
        // can be cleanly unmapped (hidden to tray) and re-mapped (shown from tray).
        // Note: WINIT_UNIX_BACKEND was removed in winit 0.29; we unset WAYLAND_DISPLAY
        // and WAYLAND_SOCKET, set GDK_BACKEND=x11, and call builder.with_x11().
        let has_x11 = std::env::var("DISPLAY").map(|s| !s.is_empty()).unwrap_or(false)
            || std::path::Path::new("/tmp/.X11-unix/X0").exists()
            || std::path::Path::new("/tmp/.X11-unix/X1").exists();

        if has_x11 {
            unsafe {
                if std::env::var("DISPLAY").map(|s| s.is_empty()).unwrap_or(true) {
                    std::env::set_var("DISPLAY", ":0");
                }
                std::env::set_var("GDK_BACKEND", "x11");
                std::env::remove_var("WAYLAND_DISPLAY");
                std::env::remove_var("WAYLAND_SOCKET");
            }
        }
        has_x11
    };

    let args: Vec<String> = env::args().collect();
    let config_path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        default_user_config_path().unwrap_or_else(|_| PathBuf::from("config.toml"))
    };

    let logger = SharedLogger::new();
    let engine = MidiEngine::new(logger.clone());
    let core = std::rc::Rc::new(std::cell::RefCell::new(ReMidiCore::new(
        logger,
        engine,
        config_path,
    )));

    let initial_visible = core.borrow().window_visible;

    let viewport = egui::ViewportBuilder::default()
        .with_title("ReMIDI — Log & Error Monitor")
        .with_inner_size([760.0, 440.0])
        .with_min_inner_size([480.0, 260.0])
        .with_visible(initial_visible);

    #[cfg(target_os = "linux")]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = if use_x11 {
        Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }))
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let event_loop_builder = None;

    let options = eframe::NativeOptions {
        viewport,
        event_loop_builder,
        ..Default::default()
    };

    let core_clone = core.clone();
    let mut app = WindowApp {
        core: core_clone,
        last_visible: initial_visible,
        startup_hidden_ticks: if initial_visible { 0 } else { 2 },
    };

    eframe::run_simple_native("ReMIDI", options, move |ctx, _frame| {
        app.update_ui(ctx);
    })
}
