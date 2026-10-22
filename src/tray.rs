use crate::config::PipelineConfig;
use crate::midi::LogLevel;
use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub struct PipelineMenuEntry {
    pub pipeline_index: usize,
    pub toggle_enabled: CheckMenuItem,
    pub run_test: MenuId,
    pub port_items: Vec<(CheckMenuItem, String)>,
}

pub struct LogLevelMenuEntries {
    pub item_error: CheckMenuItem,
    pub item_transform: CheckMenuItem,
    pub item_info: CheckMenuItem,
    pub item_verbose: CheckMenuItem,
}

pub struct TrayMenuIds {
    pub toggle_window: MenuId,
    pub edit_config: MenuId,
    pub reload_config: MenuId,
    pub log_level_items: LogLevelMenuEntries,
    pub toggle_pause: CheckMenuItem,
    pub quit: MenuId,
    pub pipelines: Vec<PipelineMenuEntry>,
}

pub fn generate_tray_icon(has_error: bool, is_paused: bool) -> Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);

    // Pick accent color: red if error, amber if paused, teal/green if active
    let (r_acc, g_acc, b_acc) = if has_error {
        (235u8, 64u8, 52u8)
    } else if is_paused {
        (240u8, 173u8, 78u8)
    } else {
        (46u8, 204u8, 113u8)
    };

    let center = (size as f32 - 1.0) / 2.0;
    let outer_r = 14.0f32;
    let inner_r = 11.5f32;

    // 5-pin DIN MIDI connector pin coordinates (normalized around center)
    let pins: [(f32, f32); 5] = [
        (-6.0, 2.0),
        (-4.2, -3.8),
        (0.0, -5.8),
        (4.2, -3.8),
        (6.0, 2.0),
    ];

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist > outer_r {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else if dist > inner_r {
                // Outer ring
                rgba.extend_from_slice(&[r_acc, g_acc, b_acc, 255]);
            } else {
                // Check if inside one of the 5 MIDI pins
                let mut is_pin = false;
                for (px, py) in pins {
                    let pdx = dx - px;
                    let pdy = dy - py;
                    if pdx * pdx + pdy * pdy <= 2.1 * 2.1 {
                        is_pin = true;
                        break;
                    }
                }
                // Or bottom keyway notch
                let is_notch = dy > 8.5 && dx.abs() < 2.2;
                if is_pin || is_notch {
                    rgba.extend_from_slice(&[r_acc, g_acc, b_acc, 255]);
                } else {
                    rgba.extend_from_slice(&[24, 28, 36, 255]);
                }
            }
        }
    }

    Icon::from_rgba(rgba, size, size).expect("Failed to create tray icon")
}

pub fn build_tray_menu(
    pipelines: &[PipelineConfig],
    midi_out_ports: &[String],
    log_level: LogLevel,
    paused: bool,
) -> anyhow::Result<(Menu, TrayMenuIds)> {
    let menu = Menu::new();

    let item_window = MenuItem::new("Show / Hide Log & Error Window", true, None);
    menu.append(&item_window)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let mut pipeline_entries = Vec::with_capacity(pipelines.len());

    for (idx, p) in pipelines.iter().enumerate() {
        let label = format!("{} ({})", p.id, p.full_virtual_port_name());
        let sub = Submenu::new(&label, true);

        let item_enabled = CheckMenuItem::new("Enabled", true, p.enabled, None);
        sub.append(&item_enabled)?;

        let item_test = MenuItem::new("Run Test", p.enabled && !p.test.is_empty(), None);
        sub.append(&item_test)?;
        sub.append(&PredefinedMenuItem::separator())?;

        // List all available MIDI-out ports (excluding this pipeline's own virtual port if visible)
        let mut port_items = Vec::new();
        let mut found_current = false;

        for port_name in midi_out_ports {
            let is_selected =
                *port_name == p.target_port || port_name.contains(&p.target_port);
            if is_selected {
                found_current = true;
            }
            let check = CheckMenuItem::new(port_name, true, is_selected, None);
            sub.append(&check)?;
            port_items.push((check, port_name.clone()));
        }

        if !found_current && !p.target_port.is_empty() {
            let missing_label = format!("{} (disconnected)", p.target_port);
            let check = CheckMenuItem::new(&missing_label, false, true, None);
            sub.append(&check)?;
        }

        menu.append(&sub)?;
        pipeline_entries.push(PipelineMenuEntry {
            pipeline_index: idx,
            toggle_enabled: item_enabled,
            run_test: item_test.id().clone(),
            port_items,
        });
    }

    if !pipelines.is_empty() {
        menu.append(&PredefinedMenuItem::separator())?;
    }

    let item_edit = MenuItem::new("Edit Config (edit)", true, None);
    let item_reload = MenuItem::new("Reload Config (update)", true, None);

    let sub_log_level = Submenu::new("Log Level", true);
    let item_lvl_error = CheckMenuItem::new("Error", true, log_level == LogLevel::Error, None);
    let item_lvl_transform =
        CheckMenuItem::new("Transform", true, log_level == LogLevel::Transform, None);
    let item_lvl_info = CheckMenuItem::new("Info", true, log_level == LogLevel::Info, None);
    let item_lvl_verbose =
        CheckMenuItem::new("Verbose", true, log_level == LogLevel::Verbose, None);

    sub_log_level.append(&item_lvl_error)?;
    sub_log_level.append(&item_lvl_transform)?;
    sub_log_level.append(&item_lvl_info)?;
    sub_log_level.append(&item_lvl_verbose)?;

    let item_pause = CheckMenuItem::new("Pause All Pipelines", true, paused, None);
    let item_quit = MenuItem::new("Quit ReMIDI", true, None);

    menu.append(&item_edit)?;
    menu.append(&item_reload)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&sub_log_level)?;
    menu.append(&item_pause)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_quit)?;

    let ids = TrayMenuIds {
        toggle_window: item_window.id().clone(),
        edit_config: item_edit.id().clone(),
        reload_config: item_reload.id().clone(),
        log_level_items: LogLevelMenuEntries {
            item_error: item_lvl_error,
            item_transform: item_lvl_transform,
            item_info: item_lvl_info,
            item_verbose: item_lvl_verbose,
        },
        toggle_pause: item_pause,
        quit: item_quit.id().clone(),
        pipelines: pipeline_entries,
    };

    Ok((menu, ids))
}

pub fn build_tray(
    pipelines: &[PipelineConfig],
    midi_out_ports: &[String],
    log_level: LogLevel,
    paused: bool,
) -> anyhow::Result<(TrayIcon, TrayMenuIds)> {
    let (menu, ids) = build_tray_menu(pipelines, midi_out_ports, log_level, paused)?;
    let icon = generate_tray_icon(false, paused);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ReMIDI - MIDI Filter & Transformer")
        .with_icon(icon)
        .build()?;

    Ok((tray, ids))
}
