//! Aura Clipboard tray app: runs the node in the background, shows a tray menu
//! (status, pause, recent history) and a small window for group management.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use aura_node::{
    Config, DeviceId, EntryId, Event, GroupId, JoinTarget, Node, NodeHandle, Status,
    default_data_dir,
};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_notification::NotificationExt as _;

const TRAY_ID: &str = "aura";
const RECENT_IN_MENU: usize = 10;

type CmdResult<T = ()> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ------------------------------------------------------------------ commands

#[tauri::command]
fn get_status(node: State<'_, NodeHandle>) -> Status {
    node.status()
}

#[tauri::command]
async fn create_group(node: State<'_, NodeHandle>, name: String, passphrase: String) -> CmdResult {
    node.create_group(&name, &passphrase).await.map_err(err)
}

#[tauri::command]
async fn join_group(
    node: State<'_, NodeHandle>,
    group_id: Option<String>,
    address: Option<String>,
    passphrase: String,
) -> CmdResult {
    let target = match (group_id, address) {
        (Some(gid), _) if !gid.is_empty() => {
            JoinTarget::Group(gid.parse::<GroupId>().map_err(err)?)
        }
        (_, Some(addr)) if !addr.trim().is_empty() => {
            JoinTarget::Addr(aura_node::resolve_peer(&addr).await.map_err(err)?)
        }
        _ => return Err("pick a group or enter an address".into()),
    };
    node.join(target, &passphrase).await.map_err(err)
}

#[tauri::command]
async fn rotate_passphrase(node: State<'_, NodeHandle>, passphrase: String) -> CmdResult {
    node.rotate_passphrase(&passphrase).await.map_err(err)
}

#[tauri::command]
async fn leave_group(node: State<'_, NodeHandle>) -> CmdResult {
    node.leave().await.map_err(err)
}

#[tauri::command]
async fn remove_device(node: State<'_, NodeHandle>, device_id: String) -> CmdResult {
    node.remove_device(device_id.parse::<DeviceId>().map_err(err)?)
        .await
        .map_err(err)
}

#[tauri::command]
async fn set_paused(node: State<'_, NodeHandle>, paused: bool) -> CmdResult {
    node.set_paused(paused).await.map_err(err)
}

#[tauri::command]
async fn recopy(node: State<'_, NodeHandle>, id: String) -> CmdResult {
    let id = EntryId::from_hex(&id).ok_or("bad entry id")?;
    node.recopy(id).await.map_err(err)
}

// ------------------------------------------------------------------ window

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Aura Clipboard")
        .inner_size(440.0, 600.0)
        .min_inner_size(360.0, 420.0)
        .build();
    match built {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => tracing::error!("opening window: {e}"),
    }
}

// ------------------------------------------------------------------ tray

fn status_line(s: &Status) -> String {
    match &s.group {
        Some(g) => {
            let others = s.members.len().saturating_sub(1);
            format!(
                "{} · {} of {} online{}",
                g.name,
                s.online_peers(),
                others,
                if s.paused { " · paused" } else { "" }
            )
        }
        None => "Not in a group".to_string(),
    }
}

fn build_menu(app: &AppHandle, s: &Status) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(
        app,
        "status",
        status_line(s),
        false,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    if s.group.is_some() {
        let recent = Submenu::with_id(app, "recent", "Recent", !s.history.is_empty())?;
        for item in s.history.iter().take(RECENT_IN_MENU) {
            let label = format!("{}  ({})", item.preview, item.origin);
            recent.append(&MenuItem::with_id(
                app,
                format!("recopy:{}", item.id),
                label,
                true,
                None::<&str>,
            )?)?;
        }
        menu.append(&recent)?;
        menu.append(&CheckMenuItem::with_id(
            app,
            "pause",
            "Pause sync",
            true,
            s.paused,
            None::<&str>,
        )?)?;
        menu.append(&MenuItem::with_id(
            app,
            "open",
            "Devices & settings…",
            true,
            None::<&str>,
        )?)?;
    } else {
        menu.append(&MenuItem::with_id(
            app,
            "open",
            "Create or join a group…",
            true,
            None::<&str>,
        )?)?;
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    menu.append(&CheckMenuItem::with_id(
        app,
        "autostart",
        "Start at login",
        true,
        autostart,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit Aura",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

fn refresh_tray(app: &AppHandle, s: &Status) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_menu(app, s) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(e) => tracing::warn!("building tray menu: {e}"),
    }
    let _ = tray.set_tooltip(Some(format!("Aura Clipboard: {}", status_line(s))));
}

fn on_menu(app: &AppHandle, id: &str) {
    let node = app.state::<NodeHandle>().inner().clone();
    match id {
        "open" => show_window(app),
        "quit" => {
            tauri::async_runtime::block_on(node.shutdown());
            app.exit(0);
        }
        "pause" => {
            let paused = !node.status().paused;
            tauri::async_runtime::spawn(async move { node.set_paused(paused).await });
        }
        "autostart" => {
            let al = app.autolaunch();
            let res = if al.is_enabled().unwrap_or(false) {
                al.disable()
            } else {
                al.enable()
            };
            if let Err(e) = res {
                notify(app, "Start at login", &e.to_string());
            }
            refresh_tray(app, &node.status());
        }
        other => {
            if let Some(id) = other.strip_prefix("recopy:").and_then(EntryId::from_hex) {
                tauri::async_runtime::spawn(async move { node.recopy(id).await });
            }
        }
    }
}

fn build_tray(app: &AppHandle, s: &Status) -> tauri::Result<TrayIcon> {
    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Aura Clipboard")
        .menu(&build_menu(app, s)?)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, ev| on_menu(app, ev.id().as_ref()))
        .build(app)
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Turns node events into desktop notifications (only the ones worth interrupting for).
fn notify_event(app: &AppHandle, ev: &Event) {
    match ev {
        Event::GroupJoined { name } => {
            notify(app, "Aura Clipboard", &format!("Joined group \"{name}\""))
        }
        Event::MemberJoined { name } => {
            notify(app, "New device", &format!("{name} joined the group"))
        }
        Event::MemberRemoved { name } => notify(
            app,
            "Device removed",
            &format!(
                "{name} left the group. If that PC was lost or shared, change the group passphrase."
            ),
        ),
        Event::GroupLeft => notify(app, "Aura Clipboard", "This PC is no longer in a group"),
        Event::PassphraseChanged { by } => notify(
            app,
            "Group passphrase changed",
            &format!("Changed by {by}. New PCs need the new passphrase to join."),
        ),
        Event::JoinRejected { from_addr } => notify(
            app,
            "Join attempt rejected",
            &format!("Wrong passphrase from {from_addr}"),
        ),
        Event::Warning { message } => notify(app, "Aura Clipboard", message),
        _ => {}
    }
}

// ------------------------------------------------------------------ main

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aura_node=info,aura_tray=info,warn".into()),
        )
        .init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_status,
            create_group,
            join_group,
            rotate_passphrase,
            leave_group,
            remove_device,
            set_paused,
            recopy
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let data_dir = std::env::var_os("AURA_DATA_DIR")
                .map(Into::into)
                .unwrap_or_else(default_data_dir);
            let cfg = Config::load_or_init(&data_dir)?;
            let node = tauri::async_runtime::block_on(Node::start(
                &data_dir,
                cfg,
                aura_node::system_clipboard(),
            ))?;
            app.manage(node.clone());

            let handle = app.handle().clone();
            let status = node.status();
            build_tray(&handle, &status)?;
            if status.group.is_none() {
                show_window(&handle);
            }

            // Status → tray menu + window.
            let mut status_rx = node.watch_status();
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                while status_rx.changed().await.is_ok() {
                    let s = status_rx.borrow_and_update().clone();
                    let _ = h.emit("status", &s);
                    let h2 = h.clone();
                    let _ = h.run_on_main_thread(move || refresh_tray(&h2, &s));
                }
            });

            // Events → notifications.
            let mut events = node.events();
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(ev) => notify_event(&h, &ev),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window just hides it; the app lives in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("building the Tauri app");

    app.run(|_app, event| {
        if let RunEvent::ExitRequested {
            api, code: None, ..
        } = event
        {
            api.prevent_exit();
        }
    });
}
