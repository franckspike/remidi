use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use std::path::PathBuf;

pub struct TrayMenuIds {
    pub toggle_window: MenuId,
    pub load_file: MenuId,
    pub edit_config: MenuId,
    pub reload_config: MenuId,
    pub run_tests: MenuId,
    pub list_ports: MenuId,
    pub toggle_verbose: CheckMenuItem,
    pub toggle_pause: CheckMenuItem,
    pub quit: MenuId,
    pub preset_files: Vec<(MenuId, PathBuf)>,
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

pub fn build_tray(presets: &[PathBuf], verbose: bool, paused: bool) -> anyhow::Result<(TrayIcon, TrayMenuIds)> {
    let menu = Menu::new();

    let item_window = MenuItem::new("Show / Hide Log & Error Window", true, None);
    let item_load = MenuItem::new("Load .remidi Config...", true, None);

    let submenu_presets = Submenu::new("Quick Configs", !presets.is_empty());
    let mut preset_files = Vec::new();
    for path in presets {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let item = MenuItem::new(name, true, None);
            preset_files.push((item.id().clone(), path.clone()));
            let _ = submenu_presets.append(&item);
        }
    }

    let item_edit = MenuItem::new("Edit Current Config (edit)", true, None);
    let item_reload = MenuItem::new("Reload Current Config (update)", true, None);
    let item_tests = MenuItem::new("Run Test Sequence (test)", true, None);
    let item_list = MenuItem::new("List Available MIDI Ports (list)", true, None);

    let item_verbose = CheckMenuItem::new("Verbose Logging (verbose)", true, verbose, None);
    let item_pause = CheckMenuItem::new("Pause MIDI Processing", true, paused, None);
    let item_quit = MenuItem::new("Quit ReMIDI", true, None);

    menu.append(&item_window)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_load)?;
    if !presets.is_empty() {
        menu.append(&submenu_presets)?;
    }
    menu.append(&item_edit)?;
    menu.append(&item_reload)?;
    menu.append(&item_tests)?;
    menu.append(&item_list)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_verbose)?;
    menu.append(&item_pause)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&item_quit)?;

    let icon = generate_tray_icon(false, paused);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ReMIDI - MIDI Filter & Transformer")
        .with_icon(icon)
        .build()?;

    let ids = TrayMenuIds {
        toggle_window: item_window.id().clone(),
        load_file: item_load.id().clone(),
        edit_config: item_edit.id().clone(),
        reload_config: item_reload.id().clone(),
        run_tests: item_tests.id().clone(),
        list_ports: item_list.id().clone(),
        toggle_verbose: item_verbose,
        toggle_pause: item_pause,
        quit: item_quit.id().clone(),
        preset_files,
    };

    Ok((tray, ids))
}
