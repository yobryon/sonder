//! Terminal UI for Sonder, built on ratatui + crossterm.
//!
//! The TUI is a pure *view*: it reads [`EngineSnapshot`]s from a
//! `tokio::sync::watch` channel and sends [`UserCommand`]s back on an mpsc
//! channel. Everything it shows is derived from the latest snapshot plus a
//! small amount of client-side state ([`state::UiState`]: selection,
//! signal histories, car trails, deduped event/note buffers).
//!
//! ```no_run
//! # async fn demo(snapshot: tokio::sync::watch::Receiver<std::sync::Arc<sonder_core::EngineSnapshot>>,
//! #               commands: tokio::sync::mpsc::Sender<sonder_core::UserCommand>) -> anyhow::Result<()> {
//! let opts = sonder_tui::TuiOptions { web_url: Some("http://127.0.0.1:7878".into()), ..Default::default() };
//! sonder_tui::run(snapshot, commands, opts).await
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod format;
pub mod state;
pub mod ui;

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use sonder_core::{EngineSnapshot, LayerId, UserCommand};
use tokio::sync::{mpsc, watch};

pub use state::UiState;

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// Redraw interval in milliseconds (default 50 → 20 fps).
    pub tick_ms: u64,
    /// URL of the web UI, shown in the footer; `w` opens it in a browser.
    pub web_url: Option<String>,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            tick_ms: 50,
            web_url: None,
        }
    }
}

/// What a key press asks the UI to do. Separated from the event loop so key
/// mapping is unit-testable.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Leave the TUI.
    Quit,
    /// Send a command to the engine.
    Command(UserCommand),
    /// Move the layer selection by this many rows.
    MoveSelection(i32),
    /// Toggle mute on the selected layer.
    MuteSelected,
    /// Toggle solo on the selected layer.
    SoloSelected,
    /// Toggle the help popup.
    ToggleHelp,
    /// Open the web UI.
    OpenWeb,
    /// Nothing bound.
    None,
}

/// Map a key event to an [`Action`]. Only `Press` events count.
///
/// Solo is `Shift`+digit. Terminals report the *shifted character*, not the
/// physical key, so this is matched against the US-layout row `!@#$%^&*(`.
/// On other layouts (e.g. German `"` for Shift+2, French where digits
/// themselves need Shift) the shifted characters differ and solo-by-digit is
/// unavailable; use `↑/↓` + `s` instead, which works everywhere.
pub fn map_key(key: &KeyEvent) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => Action::Quit,
        // Empty path: the engine picks `<scene-slug>.toml` in the cwd.
        KeyCode::Char('s') if ctrl => Action::Command(UserCommand::SaveScene { path: String::new() }),
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char(' ') => Action::Command(UserCommand::TogglePause),
        KeyCode::Char('[') => Action::Command(UserCommand::NudgeTempo { delta: -2.0 }),
        KeyCode::Char(']') => Action::Command(UserCommand::NudgeTempo { delta: 2.0 }),
        KeyCode::Char('-') | KeyCode::Char('_') => {
            Action::Command(UserCommand::NudgeDensity { delta: -0.1 })
        }
        KeyCode::Char('=') | KeyCode::Char('+') => {
            Action::Command(UserCommand::NudgeDensity { delta: 0.1 })
        }
        KeyCode::Char('k') => Action::Command(UserCommand::Transpose { semitones: 1 }),
        KeyCode::Char('K') => Action::Command(UserCommand::Transpose { semitones: -1 }),
        KeyCode::Char('o') => Action::Command(UserCommand::NextMode { backwards: false }),
        KeyCode::Char('O') => Action::Command(UserCommand::NextMode { backwards: true }),
        KeyCode::Char('c') => Action::Command(UserCommand::NextChord),
        KeyCode::Char('n') => Action::Command(UserCommand::NextSection),
        KeyCode::Char('m') => Action::MuteSelected,
        KeyCode::Char('s') => Action::SoloSelected,
        KeyCode::Char('w') => Action::OpenWeb,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Up => Action::MoveSelection(-1),
        KeyCode::Down => Action::MoveSelection(1),
        KeyCode::Char(d @ '1'..='9') => Action::Command(UserCommand::ToggleMute {
            layer: LayerId(d as u16 - '1' as u16),
        }),
        KeyCode::Char(ch) => match "!@#$%^&*(".find(ch) {
            Some(i) => Action::Command(UserCommand::ToggleSolo {
                layer: LayerId(i as u16),
            }),
            None => Action::None,
        },
        _ => Action::None,
    }
}

/// Best-effort "open this URL in a browser"; never fails loudly.
pub fn open_url(url: &str) -> bool {
    let candidates: [(&str, &[&str]); 4] = [
        ("xdg-open", &[]),
        ("wslview", &[]),
        ("cmd.exe", &["/c", "start", ""]),
        ("open", &[]),
    ];
    for (bin, args) in candidates {
        let ok = std::process::Command::new(bin)
            .args(args)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok();
        if ok {
            return true;
        }
    }
    false
}

/// Restore the terminal to cooked mode on the main screen. Safe to call twice.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = io::stdout().execute(crossterm::cursor::Show);
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    if let Err(e) = stdout.execute(EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e).context("enter alternate screen");
    }
    let _ = stdout.execute(crossterm::cursor::Hide);
    let terminal = Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")?;
    Ok(terminal)
}

/// Run the TUI until the user quits (`q` / `Esc` / `Ctrl-C`) or the snapshot
/// sender is dropped. The terminal is restored on every exit path, including
/// panics.
pub async fn run(
    snapshot: watch::Receiver<Arc<EngineSnapshot>>,
    commands: mpsc::Sender<UserCommand>,
    opts: TuiOptions,
) -> anyhow::Result<()> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, snapshot, commands, opts).await;
    restore_terminal();
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut snapshot: watch::Receiver<Arc<EngineSnapshot>>,
    commands: mpsc::Sender<UserCommand>,
    opts: TuiOptions,
) -> anyhow::Result<()> {
    let mut state = UiState::new();
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(opts.tick_ms.max(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut current: Arc<EngineSnapshot> = snapshot.borrow_and_update().clone();
    state.ingest(&current);
    let web_url = opts.web_url.as_deref();

    loop {
        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) => match map_key(&key) {
                        Action::Quit => {
                            send_command(&commands, &mut state, UserCommand::Quit);
                            break;
                        }
                        Action::Command(cmd) => {
                            let saving = matches!(cmd, UserCommand::SaveScene { .. });
                            if send_command(&commands, &mut state, cmd) && saving {
                                state.flash("saving scene…");
                            }
                        }
                        Action::MoveSelection(d) => state.move_selection(d, current.layers.len()),
                        Action::MuteSelected => {
                            if let Some(l) = current.layers.get(state.selected) {
                                send_command(&commands, &mut state, UserCommand::ToggleMute { layer: l.id });
                            }
                        }
                        Action::SoloSelected => {
                            if let Some(l) = current.layers.get(state.selected) {
                                send_command(&commands, &mut state, UserCommand::ToggleSolo { layer: l.id });
                            }
                        }
                        Action::ToggleHelp => state.show_help = !state.show_help,
                        Action::OpenWeb => match web_url {
                            Some(url) => {
                                if open_url(url) {
                                    state.flash(format!("opened {url}"));
                                } else {
                                    state.flash(format!("could not open a browser; visit {url}"));
                                }
                            }
                            None => state.flash("web UI not running (start with --web)"),
                        },
                        Action::None => {}
                    },
                    Some(Ok(Event::Resize(_, _))) => {
                        terminal.draw(|f| ui::draw(f, &current, &mut state, web_url))?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e).context("terminal input"),
                    None => break,
                }
            }
            changed = snapshot.changed() => {
                if changed.is_err() {
                    break; // engine went away
                }
                current = snapshot.borrow_and_update().clone();
                state.ingest(&current);
            }
            _ = ticker.tick() => {
                terminal.draw(|f| ui::draw(f, &current, &mut state, web_url))?;
            }
        }
    }
    Ok(())
}

/// Send a command without blocking the UI. A full queue drops the command and
/// says so in the footer instead of failing silently; a closed channel means the
/// engine is gone and is reported once.
fn send_command(commands: &mpsc::Sender<UserCommand>, state: &mut UiState, cmd: UserCommand) -> bool {
    match commands.try_send(cmd) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            state.flash("engine busy; key ignored");
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            state.flash("engine stopped; command not sent");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn key_mapping() {
        assert_eq!(
            map_key(&press(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            map_key(&press(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('c'), KeyModifiers::NONE)),
            Action::Command(UserCommand::NextChord)
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('3'), KeyModifiers::NONE)),
            Action::Command(UserCommand::ToggleMute { layer: LayerId(2) })
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('#'), KeyModifiers::SHIFT)),
            Action::Command(UserCommand::ToggleSolo { layer: LayerId(2) })
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('('), KeyModifiers::SHIFT)),
            Action::Command(UserCommand::ToggleSolo { layer: LayerId(8) })
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('K'), KeyModifiers::SHIFT)),
            Action::Command(UserCommand::Transpose { semitones: -1 })
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('O'), KeyModifiers::SHIFT)),
            Action::Command(UserCommand::NextMode { backwards: true })
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('o'), KeyModifiers::NONE)),
            Action::Command(UserCommand::NextMode { backwards: false })
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('n'), KeyModifiers::NONE)),
            Action::Command(UserCommand::NextSection)
        );
        assert_eq!(
            map_key(&press(KeyCode::Char(' '), KeyModifiers::NONE)),
            Action::Command(UserCommand::TogglePause)
        );
        assert_eq!(
            map_key(&press(KeyCode::Up, KeyModifiers::NONE)),
            Action::MoveSelection(-1)
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('m'), KeyModifiers::NONE)),
            Action::MuteSelected
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Action::ToggleHelp
        );
        assert_eq!(
            map_key(&press(KeyCode::Char('z'), KeyModifiers::NONE)),
            Action::None
        );
        let mut release = press(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(map_key(&release), Action::None);
    }

    #[test]
    fn ctrl_s_saves_the_scene() {
        assert_eq!(
            map_key(&press(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Command(UserCommand::SaveScene { path: String::new() })
        );
        // Plain `s` is still solo.
        assert_eq!(map_key(&press(KeyCode::Char('s'), KeyModifiers::NONE)), Action::SoloSelected);
        assert!(ui::KEY_LEGEND.contains("^s save"));
    }

    #[test]
    fn dropped_commands_are_reported_in_the_footer() {
        let (tx, mut rx) = mpsc::channel::<UserCommand>(1);
        let mut state = UiState::new();
        assert!(send_command(&tx, &mut state, UserCommand::TogglePause));
        assert!(state.status.is_none());
        // Queue full: dropped, but the user is told.
        assert!(!send_command(&tx, &mut state, UserCommand::NextChord));
        assert!(state.status.as_ref().is_some_and(|(m, _)| m.contains("busy")));
        assert_eq!(rx.try_recv().unwrap(), UserCommand::TogglePause);
        drop(rx);
        state.status = None;
        assert!(!send_command(&tx, &mut state, UserCommand::NextChord));
        assert!(state.status.as_ref().is_some_and(|(m, _)| m.contains("stopped")));
    }
}
