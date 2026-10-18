#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;
mod midi;
mod tray;

use config::Config;
use eframe::egui;
use midi::{LogLevel, MidiEngine, SharedLogger, list_midi_in_ports, list_midi_out_ports};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tray::{TrayMenuIds, build_tray, generate_tray_icon};
use tray_icon::TrayIcon;
use tray_icon::menu::MenuEvent;

fn discover_remidi_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut search_dirs = vec![base_dir.to_path_buf(), base_dir.join("reference")];
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            search_dirs.push(exe_dir.to_path_buf());
        }
    }
    for dir in search_dirs {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("remidi") {
                    if !files.contains(&path) {
                        files.push(path);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

struct ReMidiApp {
    logger: SharedLogger,
    engine: MidiEngine,
    current_config_path: Option<PathBuf>,
    tray_icon: Option<TrayIcon>,
    tray_ids: Option<TrayMenuIds>,
    window_visible: bool,
    auto_scroll: bool,
    only_errors: bool,
    icon_has_error: bool,
}

impl ReMidiApp {
    fn new(
        logger: SharedLogger,
        engine: MidiEngine,
        initial_config: Option<PathBuf>,
        presets: Vec<PathBuf>,
    ) -> Self {
        #[cfg(target_os = "linux")]
        {
            let _ = gtk::init();
        }

        let (tray_icon, tray_ids) = match build_tray(&presets, false, false) {
            Ok((t, ids)) => (Some(t), Some(ids)),
            Err(e) => {
                logger.error(format!("Warning: Could not initialize system tray icon: {}", e));
                (None, None)
            }
        };

        let mut app = Self {
            logger,
            engine,
            current_config_path: None,
            tray_icon,
            tray_ids,
            window_visible: false,
            auto_scroll: true,
            only_errors: false,
            icon_has_error: false,
        };

        app.logger.info("----- ReMIDI -----");
        if let Some(path) = initial_config {
            app.load_config_from_path(&path);
        } else {
            app.logger
                .info("No .remidi file specified. Select one via Tray Menu or 'Load Config...'.");
        }

        // If no system tray is available on this desktop environment, keep the window visible
        if app.tray_icon.is_none() {
            app.window_visible = true;
        }

        app
    }

    fn load_config_from_path(&mut self, path: &Path) {
        self.logger
            .info(format!("Loading config '{}'...", path.display()));
        self.current_config_path = Some(path.to_path_buf());
        match Config::load_from_file(path) {
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
    }

    fn reload_current_config(&mut self) {
        self.logger.info("reloading config...");
        if let Some(path) = self.current_config_path.clone() {
            self.load_config_from_path(&path);
        } else {
            self.logger.error("No configuration file is currently selected.");
        }
    }

    fn log_midi_ports(&self) {
        let out_ports = list_midi_out_ports();
        self.logger.info("MIDI-out ports are:");
        if out_ports.is_empty() {
            self.logger.info("  (none)");
        } else {
            for p in out_ports {
                self.logger.info(format!(" - {}", p));
            }
        }
        let in_ports = list_midi_in_ports();
        if !in_ports.is_empty() {
            self.logger.info("MIDI-in ports are:");
            for p in in_ports {
                self.logger.info(format!(" - {}", p));
            }
        }
    }

    fn update_tray_icon(&mut self) {
        if let Some(tray) = &mut self.tray_icon {
            let paused = self.logger.paused.load(Ordering::Relaxed);
            let _ = tray.set_icon(Some(generate_tray_icon(self.icon_has_error, paused)));
        }
    }

    fn set_window_visible(&mut self, ctx: &egui::Context, visible: bool) {
        self.window_visible = visible;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        if visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "linux")]
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let Some(ids) = &self.tray_ids else {
                continue;
            };
            if event.id == ids.toggle_window {
                self.set_window_visible(ctx, !self.window_visible);
            } else if event.id == ids.load_file {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("ReMIDI Config", &["remidi", "toml", "json"])
                    .pick_file()
                {
                    self.load_config_from_path(&file);
                }
            } else if event.id == ids.reload_config {
                self.reload_current_config();
            } else if event.id == ids.run_tests {
                self.engine.run_tests();
            } else if event.id == ids.list_ports {
                self.log_midi_ports();
                self.set_window_visible(ctx, true);
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
            } else if let Some((_, preset_path)) =
                ids.preset_files.iter().find(|(id, _)| *id == event.id)
            {
                let path_clone = preset_path.clone();
                self.load_config_from_path(&path_clone);
            }
        }
    }
}

impl eframe::App for ReMidiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_tray_events(ctx);

        // If the user clicks the window's X button, hide to tray instead of quitting
        if ctx.input(|i| i.viewport().close_requested()) && self.tray_icon.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.set_window_visible(ctx, false);
        }

        // Automatically pop up the window if an error occurred!
        if self.logger.take_unseen_error() {
            self.icon_has_error = true;
            self.update_tray_icon();
            if !self.window_visible {
                self.set_window_visible(ctx, true);
            }
        } else if self.window_visible && self.tray_icon.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        // Keep polling tray events and incoming MIDI logs periodically even when idle
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("📂 Load Config...").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("ReMIDI Config", &["remidi", "toml", "json"])
                        .pick_file()
                    {
                        self.load_config_from_path(&file);
                    }
                }
                if ui.button("🔄 Reload (update)").clicked() {
                    self.reload_current_config();
                }
                if ui.button("▶ Run Tests (test)").clicked() {
                    self.engine.run_tests();
                }
                if ui.button("🎹 List Ports (list)").clicked() {
                    self.log_midi_ports();
                }

                ui.separator();

                let mut verbose = self.logger.verbose.load(Ordering::Relaxed);
                if ui.checkbox(&mut verbose, "Verbose").changed() {
                    self.logger.verbose.store(verbose, Ordering::Relaxed);
                    if let Some(ids) = &self.tray_ids {
                        ids.toggle_verbose.set_checked(verbose);
                    }
                    self.logger.info(if verbose {
                        "Verbose is ON"
                    } else {
                        "Verbose is OFF"
                    });
                }

                let mut paused = self.logger.paused.load(Ordering::Relaxed);
                if ui.checkbox(&mut paused, "Paused").changed() {
                    self.logger.paused.store(paused, Ordering::Relaxed);
                    if let Some(ids) = &self.tray_ids {
                        ids.toggle_pause.set_checked(paused);
                    }
                    self.update_tray_icon();
                }

                ui.checkbox(&mut self.only_errors, "Errors Only");
                ui.checkbox(&mut self.auto_scroll, "Auto-scroll");

                if ui.button("🗑 Clear").clicked() {
                    self.logger.clear();
                    self.icon_has_error = false;
                    self.update_tray_icon();
                }
            });

            if let Some(path) = &self.current_config_path {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Active Config: {}", path.display()))
                            .small()
                            .color(egui::Color32::from_rgb(160, 175, 195)),
                    );
                });
            }
        });

        egui::TopBottomPanel::bottom("bottom_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Tip: Closing this window minimizes ReMIDI to the system tray. Window pops up automatically on errors.")
                        .small()
                        .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit ReMIDI").clicked() {
                        self.engine.stop_all();
                        std::process::exit(0);
                    }
                    if self.tray_icon.is_some() && ui.button("Hide to Tray").clicked() {
                        self.set_window_visible(ctx, false);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let entries = self.logger.snapshot();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(self.auto_scroll)
                .show(ui, |ui| {
                    for entry in entries {
                        if self.only_errors && entry.level != LogLevel::Error {
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
    let args: Vec<String> = env::args().collect();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let presets = discover_remidi_files(&cwd);

    let initial_config = if args.len() > 1 {
        Some(PathBuf::from(&args[1]))
    } else {
        let sample = cwd.join("Sample.remidi");
        if sample.exists() {
            Some(sample)
        } else {
            presets.first().cloned()
        }
    };

    let logger = SharedLogger::new();
    let engine = MidiEngine::new(logger.clone());

    let viewport = egui::ViewportBuilder::default()
        .with_title("ReMIDI — Log & Error Monitor")
        .with_inner_size([760.0, 440.0])
        .with_min_inner_size([480.0, 260.0])
        .with_visible(false);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ReMIDI",
        options,
        Box::new(move |_cc| Ok(Box::new(ReMidiApp::new(logger, engine, initial_config, presets)))),
    )
}
