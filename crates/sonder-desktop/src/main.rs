//! Sonder desktop: the native engine (native audio, MIDI, local sources) + the embedded web
//! UI, shown in a Tauri window. On launch the engine starts on a background thread while
//! the splash (`ui/index.html`) is up; the window then navigates to the local web UI, which
//! runs in `ws` mode exactly as it does for `sonder play --web`.
//!
//! Menu / tray: scene picker (built-ins), open a scene file, record the data log, native-
//! audio toggle, open in a browser, quit.

#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod engine;

use sonder_engine::AudioBackendChoice;
use tauri::menu::{CheckMenuItem, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use engine::Desktop;

const DEFAULT_SCENE: &str = "night-freeway";
const MAIN: &str = "main";

/// Menu items the handlers update.
struct Items {
    native_audio: CheckMenuItem<Wry>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let desktop = Desktop::new().expect("tokio runtime");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(desktop)
        .setup(|app| {
            let scenes = engine::builtin_scenes();

            // --- menu bar ---------------------------------------------------------------
            let mut scene_menu = SubmenuBuilder::new(app, "Scene");
            for (name, title, _) in &scenes {
                scene_menu = scene_menu.item(&MenuItemBuilder::with_id(format!("scene:{name}"), title).build(app)?);
            }
            let scene_menu = scene_menu
                .separator()
                .item(&MenuItemBuilder::with_id("open-scene", "Open scene file…").accelerator("CmdOrCtrl+O").build(app)?)
                .build()?;

            let native_audio = tauri::menu::CheckMenuItemBuilder::with_id("native-audio", "Native audio").checked(true).build(app)?;
            let engine_menu = SubmenuBuilder::new(app, "Engine")
                .item(&MenuItemBuilder::with_id("record-data", "Record data…").build(app)?)
                .separator()
                .item(&native_audio)
                .item(&MenuItemBuilder::with_id("open-browser", "Open in browser").build(app)?)
                .separator()
                .item(&PredefinedMenuItem::quit(app, Some("Quit Sonder"))?)
                .build()?;
            let menu = MenuBuilder::new(app).items(&[&scene_menu, &engine_menu]).build()?;
            app.set_menu(menu)?;
            app.manage(Items { native_audio });

            // --- tray --------------------------------------------------------------------
            let tray_menu = MenuBuilder::new(app)
                .item(&MenuItemBuilder::with_id("show", "Show Sonder").build(app)?)
                .item(&MenuItemBuilder::with_id("open-browser", "Open in browser").build(app)?)
                .separator()
                .item(&PredefinedMenuItem::quit(app, Some("Quit Sonder"))?)
                .build()?;
            let mut tray = TrayIconBuilder::new().menu(&tray_menu).tooltip("Sonder").show_menu_on_left_click(true);
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // --- engine: start on a thread so the splash paints, then navigate -------------
            let handle = app.handle().clone();
            let (_, _, toml) = scenes.into_iter().find(|(n, _, _)| *n == DEFAULT_SCENE).expect("default scene is built in");
            std::thread::Builder::new().name("sonder-launch".into()).spawn(move || {
                let desktop = handle.state::<Desktop>();
                match desktop.launch(toml, AudioBackendChoice::Auto) {
                    Ok(url) => navigate(&handle, &url),
                    Err(e) => {
                        tracing::error!("engine failed to start: {e:#}");
                        if let Some(w) = handle.get_webview_window(MAIN) {
                            let msg = format!("{e:#}").replace('`', "'").replace('\\', "\\\\");
                            let _ = w.eval(format!("document.getElementById('msg').textContent='the engine failed to start';document.getElementById('err').textContent=`{msg}`;"));
                        }
                    }
                }
            })?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            let id = event.id().0.as_str();
            if let Err(e) = on_menu(app, id) {
                tracing::error!(id, "menu: {e:#}");
                app.dialog().message(format!("{e:#}")).title("Sonder").kind(MessageDialogKind::Error).show(|_| {});
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building the Sonder app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                app.state::<Desktop>().stop();
            }
        });
}

/// Point the main window at the engine's web UI.
fn navigate(app: &AppHandle, url: &str) {
    match (app.get_webview_window(MAIN), tauri::Url::parse(url)) {
        (Some(w), Ok(u)) => {
            if let Err(e) = w.navigate(u) {
                tracing::error!("navigate: {e}");
            }
        }
        (None, _) => tracing::error!("no main window"),
        (_, Err(e)) => tracing::error!("bad url {url}: {e}"),
    }
}

fn on_menu(app: &AppHandle, id: &str) -> anyhow::Result<()> {
    let desktop = app.state::<Desktop>();
    if let Some(name) = id.strip_prefix("scene:") {
        let (_, _, toml) = engine::builtin_scenes().into_iter().find(|(n, _, _)| *n == name).ok_or_else(|| anyhow::anyhow!("unknown scene {name}"))?;
        return desktop.load_scene_toml(toml.to_string());
    }
    match id {
        "open-scene" => {
            let app = app.clone();
            app.dialog().file().set_title("Open a Sonder scene").add_filter("Sonder scene (TOML)", &["toml"]).pick_file(move |picked| {
                let Some(file) = picked else { return };
                let result = file
                    .into_path()
                    .map_err(anyhow::Error::from)
                    .and_then(|p| std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display())))
                    .and_then(|toml| app.state::<Desktop>().load_scene_toml(toml));
                if let Err(e) = result {
                    app.dialog().message(format!("{e:#}")).title("Sonder").kind(MessageDialogKind::Error).show(|_| {});
                }
            });
        }
        "record-data" => {
            // The data log starts with the engine, so this restarts it (same scene, same
            // backend) and the window follows the web UI to its new port.
            let app = app.clone();
            app.dialog().file().set_title("Record the data log to…").add_filter("Sonder data log", &["sonder"]).set_file_name("session.sonder").save_file(move |picked| {
                let Some(file) = picked else { return };
                let result = file.into_path().map_err(anyhow::Error::from).and_then(|path| app.state::<Desktop>().record_to(path));
                match result {
                    Ok(url) => navigate(&app, &url),
                    Err(e) => app.dialog().message(format!("{e:#}")).title("Sonder").kind(MessageDialogKind::Error).show(|_| {}),
                }
            });
        }
        "native-audio" => {
            // Restart the engine with the other backend; the web server moves to a new port.
            let next = match desktop.backend() {
                Some(AudioBackendChoice::Null) => AudioBackendChoice::Auto,
                Some(_) => AudioBackendChoice::Null,
                None => anyhow::bail!("the engine is still starting"),
            };
            let url = desktop.restart(next)?;
            app.state::<Items>().native_audio.set_checked(next != AudioBackendChoice::Null)?;
            navigate(app, &url);
        }
        "open-browser" => {
            let url = desktop.url().ok_or_else(|| anyhow::anyhow!("the engine is still starting"))?;
            sonder_web::open_browser(&url);
        }
        "show" => {
            if let Some(w) = app.get_webview_window(MAIN) {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        _ => {}
    }
    Ok(())
}
