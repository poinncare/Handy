//! Global, privacy-preserving manual typing session detection.
//!
//! Only coarse key classes, timestamps, an opaque accessibility identity, and
//! screen bounds enter the state machine. Typed content is never reconstructed,
//! stored, emitted, or logged.

use crate::focused_input::{self, FocusedInput, ScreenRect};
use handy_keys::{Error as HandyKeysError, Key, KeyEvent, KeyboardListener, Modifiers};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FOCUS_REFRESH_INTERVAL_MS: u64 = 250;
const IDLE_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    Typing,
    Submit,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SessionState {
    Idle,
    Tracking {
        started_at: u64,
        last_input_at: u64,
        target: u64,
    },
    Shown {
        last_input_at: u64,
        target: u64,
    },
    Suppressed {
        last_input_at: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MachineAction {
    None,
    Show(ScreenRect),
    Move(ScreenRect),
    Hide,
}

#[derive(Debug)]
struct SessionMachine {
    state: SessionState,
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
        }
    }
}

impl SessionMachine {
    fn on_typing(
        &mut self,
        now_ms: u64,
        focus: Option<FocusedInput>,
        threshold_ms: u64,
    ) -> MachineAction {
        if let SessionState::Suppressed { .. } = self.state {
            self.state = SessionState::Suppressed {
                last_input_at: now_ms,
            };
            return MachineAction::None;
        }

        let Some(focus) = focus else {
            let was_shown = matches!(self.state, SessionState::Shown { .. });
            self.state = SessionState::Idle;
            return if was_shown {
                MachineAction::Hide
            } else {
                MachineAction::None
            };
        };

        match self.state {
            SessionState::Idle => {
                self.state = SessionState::Tracking {
                    started_at: now_ms,
                    last_input_at: now_ms,
                    target: focus.identity,
                };
                MachineAction::None
            }
            SessionState::Tracking {
                started_at,
                last_input_at,
                target,
            } => {
                if target != focus.identity
                    || now_ms.saturating_sub(last_input_at) >= IDLE_TIMEOUT_MS
                {
                    self.state = SessionState::Tracking {
                        started_at: now_ms,
                        last_input_at: now_ms,
                        target: focus.identity,
                    };
                    return MachineAction::None;
                }
                if now_ms.saturating_sub(started_at) >= threshold_ms {
                    self.state = SessionState::Shown {
                        last_input_at: now_ms,
                        target,
                    };
                    MachineAction::Show(focus.bounds)
                } else {
                    self.state = SessionState::Tracking {
                        started_at,
                        last_input_at: now_ms,
                        target,
                    };
                    MachineAction::None
                }
            }
            SessionState::Shown { target, .. } if target == focus.identity => {
                self.state = SessionState::Shown {
                    last_input_at: now_ms,
                    target,
                };
                MachineAction::Move(focus.bounds)
            }
            SessionState::Shown { .. } => {
                self.state = SessionState::Tracking {
                    started_at: now_ms,
                    last_input_at: now_ms,
                    target: focus.identity,
                };
                MachineAction::Hide
            }
            SessionState::Suppressed { .. } => unreachable!("handled above"),
        }
    }

    fn submit(&mut self) -> MachineAction {
        // Sending ends the current message. The very next typed character must
        // be allowed to start a fresh session even when it arrives immediately.
        self.state = SessionState::Idle;
        MachineAction::Hide
    }

    fn suppress(&mut self, now_ms: u64) -> MachineAction {
        self.state = SessionState::Suppressed {
            last_input_at: now_ms,
        };
        MachineAction::Hide
    }

    fn tick(&mut self, now_ms: u64) -> MachineAction {
        let last_input_at = match self.state {
            SessionState::Idle => return MachineAction::None,
            SessionState::Tracking { last_input_at, .. }
            | SessionState::Shown { last_input_at, .. }
            | SessionState::Suppressed { last_input_at } => last_input_at,
        };
        if now_ms.saturating_sub(last_input_at) < IDLE_TIMEOUT_MS {
            return MachineAction::None;
        }
        let was_shown = matches!(self.state, SessionState::Shown { .. });
        self.state = SessionState::Idle;
        if was_shown {
            MachineAction::Hide
        } else {
            MachineAction::None
        }
    }

    fn refresh_focus(&mut self, focus: Option<FocusedInput>) -> MachineAction {
        let SessionState::Shown { target, .. } = self.state else {
            return MachineAction::None;
        };
        match focus {
            Some(focus) if focus.identity == target => MachineAction::Move(focus.bounds),
            _ => {
                self.state = SessionState::Idle;
                MachineAction::Hide
            }
        }
    }

    fn is_shown(&self) -> bool {
        matches!(self.state, SessionState::Shown { .. })
    }

    fn is_suppressed(&self) -> bool {
        matches!(self.state, SessionState::Suppressed { .. })
    }
}

struct MonitorInner {
    running: AtomicBool,
    threshold_secs: AtomicU64,
    injection_suppression: AtomicUsize,
    suppression_epoch: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

pub struct TypingMonitor {
    inner: Arc<MonitorInner>,
}

impl Default for TypingMonitor {
    fn default() -> Self {
        Self {
            inner: Arc::new(MonitorInner {
                running: AtomicBool::new(false),
                threshold_secs: AtomicU64::new(5),
                injection_suppression: AtomicUsize::new(0),
                suppression_epoch: AtomicU64::new(0),
                thread: Mutex::new(None),
            }),
        }
    }
}

impl TypingMonitor {
    fn set_threshold(&self, seconds: u64) {
        self.inner.threshold_secs.store(
            crate::settings::clamp_memory_training_threshold(seconds),
            Ordering::Relaxed,
        );
    }

    fn start(&self, app: &AppHandle) -> Result<(), String> {
        if self
            .inner
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }

        let listener = match KeyboardListener::new() {
            Ok(listener) => listener,
            Err(error) => {
                self.inner.running.store(false, Ordering::SeqCst);
                return Err(format!("Unable to monitor global typing: {error}"));
            }
        };
        if let Err(error) = focused_input::probe() {
            self.inner.running.store(false, Ordering::SeqCst);
            return Err(error);
        }

        let inner = Arc::clone(&self.inner);
        let app = app.clone();
        let handle = thread::Builder::new()
            .name("memory-typing-monitor".into())
            .spawn(move || monitor_loop(listener, app, Arc::clone(&inner)))
            .map_err(|error| {
                self.inner.running.store(false, Ordering::SeqCst);
                format!("Unable to start the typing monitor thread: {error}")
            })?;
        *self
            .inner
            .thread
            .lock()
            .map_err(|_| "Typing monitor thread lock was poisoned".to_string())? = Some(handle);
        log::info!("Memory-training typing monitor started");
        Ok(())
    }

    fn stop(&self, app: &AppHandle) {
        self.inner.running.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = self.inner.thread.lock() {
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
        }
        crate::overlay::hide_memory_training_overlays(app);
        log::info!("Memory-training typing monitor stopped");
    }

    fn request_suppression(&self, app: &AppHandle) {
        self.inner.suppression_epoch.fetch_add(1, Ordering::SeqCst);
        crate::overlay::hide_memory_training_overlays(app);
    }
}

pub fn register(app: &AppHandle) {
    if app.try_state::<TypingMonitor>().is_none() {
        app.manage(TypingMonitor::default());
    }
}

pub fn initialize_after_shortcuts(app: &AppHandle) {
    register(app);
    let settings = crate::settings::get_settings(app);
    let monitor = app.state::<TypingMonitor>();
    monitor.set_threshold(settings.memory_training_threshold_secs);
    if settings.memory_training_enabled {
        if let Err(error) = monitor.start(app) {
            // Memory training is optional and must never make successfully
            // initialized recording shortcuts appear broken at startup.
            log::warn!("Memory-training monitor could not start: {error}");
        }
    }
}

pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    register(app);
    let monitor = app.state::<TypingMonitor>();
    if enabled {
        let settings = crate::settings::get_settings(app);
        monitor.set_threshold(settings.memory_training_threshold_secs);
        monitor.start(app)
    } else {
        monitor.stop(app);
        Ok(())
    }
}

pub fn set_threshold(app: &AppHandle, seconds: u64) {
    register(app);
    app.state::<TypingMonitor>().set_threshold(seconds);
}

pub fn suppress_for_recording(app: &AppHandle) {
    if let Some(monitor) = app.try_state::<TypingMonitor>() {
        monitor.request_suppression(app);
    } else {
        crate::overlay::hide_memory_training_overlays(app);
    }
}

pub struct InjectionSuppressionGuard {
    inner: Option<Arc<MonitorInner>>,
}

impl Drop for InjectionSuppressionGuard {
    fn drop(&mut self) {
        if let Some(inner) = &self.inner {
            inner.injection_suppression.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

pub fn suspend_injected_input(app: &AppHandle) -> InjectionSuppressionGuard {
    let inner = app.try_state::<TypingMonitor>().map(|monitor| {
        monitor
            .inner
            .injection_suppression
            .fetch_add(1, Ordering::SeqCst);
        monitor.request_suppression(app);
        Arc::clone(&monitor.inner)
    });
    InjectionSuppressionGuard { inner }
}

fn monitor_loop(listener: KeyboardListener, app: AppHandle, inner: Arc<MonitorInner>) {
    let epoch = Instant::now();
    let mut machine = SessionMachine::default();
    let mut pressed_keys = HashSet::new();
    let mut observed_suppression_epoch = inner.suppression_epoch.load(Ordering::SeqCst);
    let mut last_focus_refresh_ms = 0_u64;

    while inner.running.load(Ordering::SeqCst) {
        let event = listener.recv_timeout(POLL_INTERVAL);
        let now_ms = epoch.elapsed().as_millis() as u64;

        let current_suppression_epoch = inner.suppression_epoch.load(Ordering::SeqCst);
        if current_suppression_epoch != observed_suppression_epoch
            || (inner.injection_suppression.load(Ordering::SeqCst) > 0 && !machine.is_suppressed())
        {
            observed_suppression_epoch = current_suppression_epoch;
            apply_action(&app, machine.suppress(now_ms));
        }

        match event {
            Ok(event) => {
                if let Some((key, first_keydown)) = update_pressed_keys(&mut pressed_keys, event) {
                    if first_keydown && inner.injection_suppression.load(Ordering::SeqCst) == 0 {
                        let action = match classify_key(key, event.modifiers) {
                            KeyKind::Submit => machine.submit(),
                            KeyKind::Typing => machine.on_typing(
                                now_ms,
                                focused_input::focused_editable(),
                                inner.threshold_secs.load(Ordering::Relaxed) * 1_000,
                            ),
                            KeyKind::Other => MachineAction::None,
                        };
                        apply_action(&app, action);
                    }
                }
            }
            Err(HandyKeysError::Timeout) => {}
            Err(error) => {
                log::error!("Memory-training keyboard listener stopped: {error}");
                break;
            }
        }

        apply_action(&app, machine.tick(now_ms));
        if machine.is_shown()
            && now_ms.saturating_sub(last_focus_refresh_ms) >= FOCUS_REFRESH_INTERVAL_MS
        {
            last_focus_refresh_ms = now_ms;
            apply_action(
                &app,
                machine.refresh_focus(focused_input::focused_editable()),
            );
        }
    }

    inner.running.store(false, Ordering::SeqCst);
    crate::overlay::hide_memory_training_overlays(&app);
}

fn apply_action(app: &AppHandle, action: MachineAction) {
    match action {
        MachineAction::None => {}
        MachineAction::Show(bounds) => {
            crate::overlay::show_memory_tip_overlay(app);
            crate::overlay::show_memory_focus_overlay(app, bounds);
        }
        MachineAction::Move(bounds) => {
            crate::overlay::show_memory_focus_overlay(app, bounds);
        }
        MachineAction::Hide => crate::overlay::hide_memory_training_overlays(app),
    }
}

fn update_pressed_keys(pressed_keys: &mut HashSet<Key>, event: KeyEvent) -> Option<(Key, bool)> {
    let key = event.key?;
    if event.is_key_down {
        Some((key, pressed_keys.insert(key)))
    } else {
        pressed_keys.remove(&key);
        Some((key, false))
    }
}

fn classify_key(key: Key, modifiers: Modifiers) -> KeyKind {
    if matches!(key, Key::Return | Key::KeypadEnter) {
        if modifiers.contains(Modifiers::SHIFT) && !has_shortcut_modifiers(modifiers) {
            return KeyKind::Typing;
        }
        return KeyKind::Submit;
    }
    if has_shortcut_modifiers(modifiers) {
        return KeyKind::Other;
    }
    if matches!(
        key,
        Key::A
            | Key::B
            | Key::C
            | Key::D
            | Key::E
            | Key::F
            | Key::G
            | Key::H
            | Key::I
            | Key::J
            | Key::K
            | Key::L
            | Key::M
            | Key::N
            | Key::O
            | Key::P
            | Key::Q
            | Key::R
            | Key::S
            | Key::T
            | Key::U
            | Key::V
            | Key::W
            | Key::X
            | Key::Y
            | Key::Z
            | Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
            | Key::Space
            | Key::Delete
            | Key::ForwardDelete
            | Key::Minus
            | Key::Equal
            | Key::LeftBracket
            | Key::RightBracket
            | Key::Backslash
            | Key::Semicolon
            | Key::Quote
            | Key::Comma
            | Key::Period
            | Key::Slash
            | Key::Grave
            | Key::Section
            | Key::JisYen
            | Key::JisUnderscore
            | Key::JisEisu
            | Key::JisKana
            | Key::Keypad0
            | Key::Keypad1
            | Key::Keypad2
            | Key::Keypad3
            | Key::Keypad4
            | Key::Keypad5
            | Key::Keypad6
            | Key::Keypad7
            | Key::Keypad8
            | Key::Keypad9
            | Key::KeypadDecimal
            | Key::KeypadMultiply
            | Key::KeypadPlus
            | Key::KeypadDivide
            | Key::KeypadMinus
            | Key::KeypadEquals
            | Key::KeypadComma
    ) {
        KeyKind::Typing
    } else {
        KeyKind::Other
    }
}

fn has_shortcut_modifiers(modifiers: Modifiers) -> bool {
    if modifiers.intersects(Modifiers::CMD | Modifiers::CTRL | Modifiers::FN) {
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        modifiers.intersects(Modifiers::OPT)
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Right Alt is AltGr on common Windows/Linux layouts and is part of
        // ordinary character entry. Other Alt combinations are shortcuts.
        let option = modifiers & Modifiers::OPT;
        !option.is_empty() && option != Modifiers::OPT_RIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus(identity: u64) -> FocusedInput {
        FocusedInput {
            identity,
            bounds: ScreenRect {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 40.0,
            },
        }
    }

    #[test]
    fn sustained_threshold_requires_a_later_real_key() {
        let mut machine = SessionMachine::default();
        assert_eq!(
            machine.on_typing(0, Some(focus(1)), 5_000),
            MachineAction::None
        );
        assert_eq!(machine.tick(5_000), MachineAction::None);
        assert_eq!(machine.state, SessionState::Idle);

        assert_eq!(
            machine.on_typing(6_000, Some(focus(1)), 5_000),
            MachineAction::None
        );
        assert_eq!(
            machine.on_typing(10_000, Some(focus(1)), 5_000),
            MachineAction::None
        );
        assert_eq!(
            machine.on_typing(11_100, Some(focus(1)), 5_000),
            MachineAction::Show(focus(1).bounds)
        );
    }

    #[test]
    fn shown_session_emits_show_only_once() {
        let mut machine = SessionMachine::default();
        machine.on_typing(0, Some(focus(1)), 1_000);
        assert!(matches!(
            machine.on_typing(1_000, Some(focus(1)), 1_000),
            MachineAction::Show(_)
        ));
        assert!(matches!(
            machine.on_typing(1_100, Some(focus(1)), 1_000),
            MachineAction::Move(_)
        ));
    }

    #[test]
    fn enter_hides_and_next_message_starts_fresh() {
        let mut machine = SessionMachine::default();
        machine.on_typing(0, Some(focus(1)), 1_000);
        machine.on_typing(1_000, Some(focus(1)), 1_000);
        assert_eq!(machine.submit(), MachineAction::Hide);
        assert_eq!(machine.state, SessionState::Idle);
        assert_eq!(
            machine.on_typing(2_000, Some(focus(1)), 1_000),
            MachineAction::None
        );
        assert!(matches!(machine.state, SessionState::Tracking { .. }));
        assert_eq!(
            machine.on_typing(3_000, Some(focus(1)), 1_000),
            MachineAction::Show(focus(1).bounds)
        );
    }

    #[test]
    fn target_change_hides_and_restarts_tracking() {
        let mut machine = SessionMachine::default();
        machine.on_typing(0, Some(focus(1)), 1_000);
        machine.on_typing(1_000, Some(focus(1)), 1_000);
        assert_eq!(
            machine.on_typing(1_100, Some(focus(2)), 1_000),
            MachineAction::Hide
        );
        assert!(matches!(
            machine.state,
            SessionState::Tracking { target: 2, .. }
        ));
    }

    #[test]
    fn held_key_repeat_is_ignored_until_release() {
        let mut pressed = HashSet::new();
        let down = KeyEvent {
            modifiers: Modifiers::empty(),
            key: Some(Key::A),
            is_key_down: true,
            changed_modifier: None,
        };
        assert_eq!(
            update_pressed_keys(&mut pressed, down),
            Some((Key::A, true))
        );
        assert_eq!(
            update_pressed_keys(&mut pressed, down),
            Some((Key::A, false))
        );
        let up = KeyEvent {
            is_key_down: false,
            ..down
        };
        assert_eq!(update_pressed_keys(&mut pressed, up), Some((Key::A, false)));
        assert_eq!(
            update_pressed_keys(&mut pressed, down),
            Some((Key::A, true))
        );
    }

    #[test]
    fn classifier_ignores_shortcuts_but_submit_always_wins() {
        assert_eq!(classify_key(Key::A, Modifiers::CMD), KeyKind::Other);
        assert_eq!(classify_key(Key::Return, Modifiers::CMD), KeyKind::Submit);
        assert_eq!(classify_key(Key::Return, Modifiers::SHIFT), KeyKind::Typing);
        assert_eq!(classify_key(Key::A, Modifiers::SHIFT), KeyKind::Typing);
    }
}
