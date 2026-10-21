#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod alsa_backend;
mod config;
mod engine;
mod midi;
mod tray;

use config::{Config, default_user_config_path, ensure_user_config_exists};
use eframe::egui;
use midi::{LogLevel, MidiEngine, SharedLogger, list_midi_out_ports};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tray::{TrayMenuIds, build_tray, build_tray_menu, generate_tray_icon};
use tray_icon::TrayIcon;
use tray_icon::menu::MenuEvent;

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
        if let Ok(status) = Command::new("sh")
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

fn spawn_editor_and_wait(path: &Path, logger: &SharedLogger) -> anyhow::Result<()> {
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
            let mut cmd = Command::new(term);
            match term {
                "gnome-terminal" => {
                    cmd.arg("--wait").arg("--").arg(bin);
                }
                "xfce4-terminal" => {
                    cmd.arg("--disable-server").arg("-x").arg(bin);
                }
                "wezterm" => {
                    cmd.arg("start").arg("--always-new-process").arg("--").arg(bin);
                }
                _ => {
                    cmd.arg("-e").arg(bin);
                }
            }
            for a in extra_args {
                cmd.arg(a);
            }
            cmd.arg(path);
            let status = cmd.status()?;
            if !status.success() {
                anyhow::bail!("Editor exited with status {}", status);
            }
            return Ok(());
        }

    logger.info(format!("Opening '{}' with '{}'...", path.display(), editor));
    let mut cmd = Command::new(bin);
    for a in extra_args {
        cmd.arg(a);
    }
    let bin_name = Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(bin);
    if matches!(bin_name, "code" | "code.cmd" | "codium" | "subl" | "zed")
        && !extra_args.iter().any(|&a| a == "--wait" || a == "-w")
    {
        cmd.arg("--wait");
    }
    cmd.arg(path);
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("Editor '{}' exited with status {}", editor, status);
    }
    Ok(())
}

struct ReMidiCore {
    logger: SharedLogger,
    engine: MidiEngine,
    config_path: PathBuf,
    tray_icon: Option<TrayIcon>,
    tray_ids: Option<TrayMenuIds>,
    cached_midi_ports: Vec<String>,
    last_port_check: Instant,
    window_visible: bool,
    auto_scroll: bool,
    only_errors: bool,
    icon_has_error: bool,
    reload_pending: Arc<AtomicBool>,
    editing_in_progress: Arc<AtomicBool>,
}

impl ReMidiCore {
    fn new(logger: SharedLogger, engine: MidiEngine, config_path: PathBuf) -> Self {
        #[cfg(target_os = "linux")]
        {
            let _ = gtk::init();
        }

        let cached_midi_ports = list_midi_out_ports();
        let (tray_icon, tray_ids) = match build_tray(&[], &cached_midi_ports, false, false) {
            Ok((t, ids)) => (Some(t), Some(ids)),
            Err(e) => {
                logger.error(format!("Warning: Could not initialize system tray icon: {}", e));
                (None, None)
            }
        };

        let mut core = Self {
            logger,
            engine,
            config_path,
            tray_icon,
            tray_ids,
            cached_midi_ports,
            last_port_check: Instant::now(),
            window_visible: true,
            auto_scroll: true,
            only_errors: false,
            icon_has_error: false,
            reload_pending: Arc::new(AtomicBool::new(false)),
            editing_in_progress: Arc::new(AtomicBool::new(false)),
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

        core
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
        let verbose = self.logger.verbose.load(Ordering::Relaxed);
        let paused = self.logger.paused.load(Ordering::Relaxed);
        self.cached_midi_ports = list_midi_out_ports();
        if let Ok((menu, ids)) =
            build_tray_menu(pipelines, &self.cached_midi_ports, verbose, paused)
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
        if self.editing_in_progress.swap(true, Ordering::SeqCst) {
            self.logger
                .info("An editor session is already open for config.toml.");
            return;
        }

        let path = self.config_path.clone();
        let logger = self.logger.clone();
        let reload_pending = self.reload_pending.clone();
        let editing_in_progress = self.editing_in_progress.clone();

        thread::spawn(move || {
            match spawn_editor_and_wait(&path, &logger) {
                Ok(()) => {
                    logger.info(format!(
                        "Finished editing '{}', reloading...",
                        path.display()
                    ));
                    reload_pending.store(true, Ordering::SeqCst);
                }
                Err(e) => {
                    logger.error(format!(
                        "Error launching editor for '{}': {}",
                        path.display(),
                        e
                    ));
                }
            }
            editing_in_progress.store(false, Ordering::SeqCst);
        });
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
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }

        if self.reload_pending.swap(false, Ordering::SeqCst) {
            self.reload_config();
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
            } else if event.id == ids.toggle_verbose.id() {
                let new_val = !self.logger.verbose.load(Ordering::Relaxed);
                self.logger.verbose.store(new_val, Ordering::Relaxed);
                ids.toggle_verbose.set_checked(new_val);
                self.logger.info(if new_val {
                    "Verbose is ON"
                } else {
                    "Verbose is OFF"
                });
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
}

impl WindowApp {
    fn update_ui(&mut self, ctx: &egui::Context) {
        let mut core = self.core.borrow_mut();
        core.poll_events();

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
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }

        ctx.request_repaint_after(Duration::from_millis(80));

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.menu_button("🔀 Pipelines", |ui| {
                    render_pipelines_menu(ui, &mut core);
                });

                let is_editing = core.editing_in_progress.load(Ordering::Relaxed);
                let edit_label = if is_editing {
                    "✏ Editing..."
                } else {
                    "✏ Edit Config (edit)"
                };
                if ui
                    .add_enabled(!is_editing, egui::Button::new(edit_label))
                    .clicked()
                {
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

                let mut verbose = core.logger.verbose.load(Ordering::Relaxed);
                if ui.checkbox(&mut verbose, "Verbose").changed() {
                    core.logger.verbose.store(verbose, Ordering::Relaxed);
                    if let Some(ids) = &core.tray_ids {
                        ids.toggle_verbose.set_checked(verbose);
                    }
                    core.logger.info(if verbose {
                        "Verbose is ON"
                    } else {
                        "Verbose is OFF"
                    });
                }

                let mut paused = core.logger.paused.load(Ordering::Relaxed);
                if ui.checkbox(&mut paused, "Pause all").changed() {
                    core.logger.paused.store(paused, Ordering::Relaxed);
                    if let Some(ids) = &core.tray_ids {
                        ids.toggle_pause.set_checked(paused);
                    }
                    core.update_tray_icon();
                }

                ui.checkbox(&mut core.only_errors, "Errors Only");
                ui.checkbox(&mut core.auto_scroll, "Auto-scroll");

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
            let only_errors = core.only_errors;
            let auto_scroll = core.auto_scroll;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(auto_scroll)
                .show(ui, |ui| {
                    for entry in entries {
                        if only_errors && entry.level != LogLevel::Error {
                            continue;
                        }
                        let color = match entry.level {
                            LogLevel::Info => egui::Color32::from_rgb(210, 215, 225),
                            LogLevel::Transform => egui::Color32::from_rgb(80, 220, 130),
                            LogLevel::Verbose => egui::Color32::from_rgb(140, 150, 165),
                            LogLevel::Error => egui::Color32::from_rgb(255, 95, 85),
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
    };

    eframe::run_simple_native("ReMIDI", options, move |ctx, _frame| {
        app.update_ui(ctx);
    })
}
