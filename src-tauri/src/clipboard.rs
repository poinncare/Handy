use crate::input::{self, EnigoState};
#[cfg(target_os = "linux")]
use crate::settings::TypingTool;
use crate::settings::{get_settings, AutoSubmitKey, ClipboardHandling, PasteMethod};
use enigo::{Direction, Enigo, Key, Keyboard};
use log::{info, warn};
use std::process::Command;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[cfg(target_os = "linux")]
use crate::utils::{is_kde_wayland, is_wayland};

#[cfg(target_os = "macos")]
mod macos_paste_verification {
    use std::ffi::{c_char, c_void};
    use std::ptr;

    type AXError = i32;
    type AXUIElementRef = *const c_void;
    type CFIndex = isize;
    type CFStringEncoding = u32;
    type CFStringRef = *const c_void;
    type CFTypeID = usize;
    type CFTypeRef = *const c_void;

    const AX_ERROR_SUCCESS: AXError = 0;
    const CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AXError;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            string: *const c_char,
            encoding: CFStringEncoding,
        ) -> CFStringRef;
        fn CFStringGetTypeID() -> CFTypeID;
        fn CFStringGetLength(value: CFStringRef) -> CFIndex;
        fn CFStringGetMaximumSizeForEncoding(
            length: CFIndex,
            encoding: CFStringEncoding,
        ) -> CFIndex;
        fn CFStringGetCString(
            value: CFStringRef,
            buffer: *mut c_char,
            buffer_size: CFIndex,
            encoding: CFStringEncoding,
        ) -> bool;
        fn CFRelease(value: CFTypeRef);
    }

    struct OwnedCf(CFTypeRef);

    impl Drop for OwnedCf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: Values stored in OwnedCf come from Create/Copy functions.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn copy_attribute(element: AXUIElementRef, attribute: CFStringRef) -> Option<OwnedCf> {
        let mut value: CFTypeRef = ptr::null();
        // SAFETY: `element` and the exported attribute constants are valid CoreFoundation
        // objects. A successful Copy call returns an owned value released by OwnedCf.
        let result =
            unsafe { AXUIElementCopyAttributeValue(element, attribute, &mut value as *mut _) };
        (result == AX_ERROR_SUCCESS && !value.is_null()).then_some(OwnedCf(value))
    }

    fn attribute(name: &'static [u8]) -> Option<OwnedCf> {
        debug_assert_eq!(name.last(), Some(&0));
        // AX attribute constants are CFSTR macros rather than exported linker
        // symbols. Construct the equivalent CFString directly from the SDK name.
        let value = unsafe {
            CFStringCreateWithCString(ptr::null(), name.as_ptr().cast(), CF_STRING_ENCODING_UTF8)
        };
        (!value.is_null()).then_some(OwnedCf(value))
    }

    fn cf_string_to_rust(value: CFTypeRef) -> Option<String> {
        // SAFETY: Type IDs can be queried for any non-null CoreFoundation object.
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            return None;
        }

        let string = value as CFStringRef;
        // SAFETY: `string` was verified to be a CFString.
        let length = unsafe { CFStringGetLength(string) };
        let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) };
        let buffer_size = maximum.checked_add(1)?;
        let mut buffer = vec![0_u8; usize::try_from(buffer_size).ok()?];
        // SAFETY: The buffer is writable for `buffer_size` bytes and the encoding is UTF-8.
        if !unsafe {
            CFStringGetCString(
                string,
                buffer.as_mut_ptr().cast(),
                buffer_size,
                CF_STRING_ENCODING_UTF8,
            )
        } {
            return None;
        }
        let end = buffer.iter().position(|byte| *byte == 0)?;
        String::from_utf8(buffer[..end].to_vec()).ok()
    }

    pub(super) fn focused_text_value() -> Option<String> {
        // SAFETY: AXUIElementCreateSystemWide returns an owned AX object.
        let system_wide = OwnedCf(unsafe { AXUIElementCreateSystemWide() });
        if system_wide.0.is_null() {
            return None;
        }
        let focused_attribute = attribute(b"AXFocusedUIElement\0")?;
        let value_attribute = attribute(b"AXValue\0")?;
        let focused = copy_attribute(system_wide.0, focused_attribute.0)?;
        let value = copy_attribute(focused.0, value_attribute.0)?;
        cf_string_to_rust(value.0)
    }
}

#[cfg(target_os = "windows")]
enum WindowsPasteOutcome {
    Complete { read_confirmed: bool },
    ManualPaste(String),
}

#[cfg(target_os = "windows")]
fn paste_via_win_text_inject(
    text: &str,
    paste_method: &PasteMethod,
    paste_delay_ms: u64,
    paste_delay_after_ms: u64,
) -> Result<WindowsPasteOutcome, String> {
    use win_text_inject::{inject, Chord, Options, Outcome, Strategy, Target};

    let chord = match paste_method {
        PasteMethod::CtrlV => Chord::CtrlV,
        PasteMethod::CtrlShiftV => Chord::CtrlShiftV,
        PasteMethod::ShiftInsert => Chord::ShiftInsert,
        _ => return Err("Invalid paste method for clipboard paste".into()),
    };
    let target = Target::foreground().map_err(|error| error.to_string())?;

    match inject(
        &target,
        text,
        Options {
            strategy: Strategy::ClipboardPaste,
            chord: Some(chord),
            pre_paste: Duration::from_millis(paste_delay_ms),
            post_paste: Duration::from_millis(paste_delay_after_ms),
            // Handy owns restoration so screenshots and empty clipboards survive too.
            // win-text-inject's built-in snapshot deliberately captures text only.
            restore_clipboard: false,
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?
    {
        Outcome::Pasted {
            read_confirmed: true,
        } => Ok(WindowsPasteOutcome::Complete {
            read_confirmed: true,
        }),
        Outcome::Pasted {
            read_confirmed: false,
        } => {
            warn!("Paste was sent, but the Windows target's clipboard read was not observable");
            Ok(WindowsPasteOutcome::Complete {
                read_confirmed: false,
            })
        }
        Outcome::ClipboardOnly(_) => Ok(WindowsPasteOutcome::ManualPaste(
            "The target could not accept synthesized input; the transcript was left on the clipboard for manual paste".into(),
        )),
        Outcome::Typed => Err("Unexpected direct-typing outcome from Windows paste".into()),
    }
}

enum SavedClipboard {
    Text(String),
    Image(tauri::image::Image<'static>),
    Empty,
}

fn capture_clipboard(app_handle: &AppHandle) -> SavedClipboard {
    let clipboard = app_handle.clipboard();
    if let Ok(text) = clipboard.read_text() {
        if !text.is_empty() {
            return SavedClipboard::Text(text);
        }
    }
    match clipboard.read_image() {
        Ok(image) => SavedClipboard::Image(image.to_owned()),
        Err(_) => SavedClipboard::Empty,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardRestoreDecision {
    Restore,
    ClipboardChanged,
    TranscriptUnavailable,
}

fn clipboard_restore_decision(
    current_text: Option<&str>,
    transcript: &str,
    current_has_non_text_content: bool,
) -> ClipboardRestoreDecision {
    match current_text {
        Some(current) if current == transcript => ClipboardRestoreDecision::Restore,
        Some(_) => ClipboardRestoreDecision::ClipboardChanged,
        None if current_has_non_text_content => ClipboardRestoreDecision::ClipboardChanged,
        None => ClipboardRestoreDecision::TranscriptUnavailable,
    }
}

fn restore_saved_clipboard(app_handle: &AppHandle, saved: SavedClipboard, transcript: &str) {
    let clipboard = app_handle.clipboard();
    let current_text = clipboard.read_text().ok();
    let current_has_non_text_content = current_text.is_none() && clipboard.read_image().is_ok();
    match clipboard_restore_decision(
        current_text.as_deref(),
        transcript,
        current_has_non_text_content,
    ) {
        ClipboardRestoreDecision::ClipboardChanged => {
            info!("Clipboard changed during paste; leaving the newer user content untouched");
            return;
        }
        ClipboardRestoreDecision::TranscriptUnavailable => {
            warn!("Transcript is no longer on the clipboard; skipping restoration");
            return;
        }
        ClipboardRestoreDecision::Restore => {}
    }

    let result = match saved {
        SavedClipboard::Text(text) => {
            #[cfg(target_os = "linux")]
            if is_wayland() && is_wl_copy_available() {
                write_clipboard_via_wl_copy(&text)
            } else {
                clipboard
                    .write_text(&text)
                    .map_err(|error| error.to_string())
            }

            #[cfg(not(target_os = "linux"))]
            clipboard
                .write_text(&text)
                .map_err(|error| error.to_string())
        }
        SavedClipboard::Image(image) => clipboard
            .write_image(&image)
            .map_err(|error| error.to_string()),
        SavedClipboard::Empty => clipboard.clear().map_err(|error| error.to_string()),
    };
    if let Err(error) = result {
        warn!("Failed to restore the saved clipboard: {error}");
    }
}

#[cfg(any(target_os = "macos", test))]
fn macos_paste_observed(before: &str, after: &str, transcript: &str) -> bool {
    before != after && after.contains(transcript)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClipboardPasteOutcome {
    Confirmed,
    #[cfg(any(target_os = "linux", test))]
    DispatchedUnverified,
    Unconfirmed(String),
}

impl ClipboardPasteOutcome {
    fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Unconfirmed(message) => Some(message),
            Self::Confirmed => None,
            #[cfg(any(target_os = "linux", test))]
            Self::DispatchedUnverified => None,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxClipboardStrategy {
    VerifiedX11,
    WaylandPersistentDispatch,
}

#[cfg(any(target_os = "linux", test))]
fn linux_clipboard_strategy(is_wayland_session: bool) -> LinuxClipboardStrategy {
    if is_wayland_session {
        LinuxClipboardStrategy::WaylandPersistentDispatch
    } else {
        LinuxClipboardStrategy::VerifiedX11
    }
}

#[cfg(any(target_os = "linux", test))]
fn wayland_dispatch_outcome() -> ClipboardPasteOutcome {
    ClipboardPasteOutcome::DispatchedUnverified
}

fn should_use_direct_text_transport(is_macos: bool, paste_method: &PasteMethod) -> bool {
    is_macos
        && matches!(
            paste_method,
            PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert
        )
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxProviderState {
    Publishing,
    AwaitingConsumer,
    Confirmed,
    OwnershipLost,
    Failed,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxProviderInput {
    ArmPaste,
    TextRequested { intended_consumer: bool },
    SelectionClear,
    ProviderError,
}

#[cfg(any(target_os = "linux", test))]
fn linux_provider_transition(
    state: LinuxProviderState,
    input: LinuxProviderInput,
) -> LinuxProviderState {
    use LinuxProviderInput::*;
    use LinuxProviderState::*;

    match (state, input) {
        (Publishing, ArmPaste) => AwaitingConsumer,
        (Publishing, TextRequested { .. }) => Publishing,
        (
            AwaitingConsumer,
            TextRequested {
                intended_consumer: true,
            },
        ) => Confirmed,
        (
            AwaitingConsumer,
            TextRequested {
                intended_consumer: false,
            },
        ) => AwaitingConsumer,
        (Confirmed, SelectionClear) => Confirmed,
        (_, SelectionClear) => OwnershipLost,
        (Confirmed, ProviderError) => Confirmed,
        (_, ProviderError) => Failed,
        (state, _) => state,
    }
}

#[cfg(not(target_os = "linux"))]
fn wait_for_clipboard_publication(app_handle: &AppHandle, text: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(
            app_handle.clipboard().read_text().as_deref(),
            Ok(current) if current == text
        ) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxProviderEvent {
    PasteArmed,
    DeliveryConfirmed,
    OwnershipLost,
    ProviderFailed,
}

#[cfg(target_os = "linux")]
mod x11_paste_provider {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    };

    use x11rb::{
        connection::Connection,
        protocol::{
            xproto::{
                AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
                SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT,
            },
            Event,
        },
        rust_connection::RustConnection,
        wrapper::ConnectionExt as _,
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
    };

    use super::{
        linux_provider_transition, LinuxProviderEvent, LinuxProviderInput, LinuxProviderState,
    };

    x11rb::atom_manager! {
        pub Atoms: AtomCookies {
            CLIPBOARD,
            TARGETS,
            UTF8_STRING,
            UTF8_MIME_0: b"text/plain;charset=utf-8",
            UTF8_MIME_1: b"text/plain;charset=UTF-8",
            TEXT,
            X_KDE_PASSWORDMANAGERHINT: b"x-kde-passwordManagerHint",
            WM_CLIENT_LEADER,
            NET_ACTIVE_WINDOW: b"_NET_ACTIVE_WINDOW",
            NET_WM_PID: b"_NET_WM_PID",
        }
    }

    pub(super) fn start(
        text: String,
        sender: Sender<LinuxProviderEvent>,
        paste_armed: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let (connection, screen_number) =
            x11rb::connect(None).map_err(|error| format!("Failed to connect to X11: {error}"))?;
        let atoms = Atoms::new(&connection)
            .map_err(|error| format!("Failed to intern X11 clipboard atoms: {error}"))?
            .reply()
            .map_err(|error| format!("Failed to resolve X11 clipboard atoms: {error}"))?;
        let screen = connection
            .setup()
            .roots
            .get(screen_number)
            .ok_or("X11 has no screen")?;
        let window = connection
            .generate_id()
            .map_err(|error| format!("Failed to allocate X11 clipboard window: {error}"))?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::COPY_FROM_PARENT,
                COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .map_err(|error| format!("Failed to create X11 clipboard window: {error}"))?;
        connection
            .set_selection_owner(window, atoms.CLIPBOARD, x11rb::CURRENT_TIME)
            .map_err(|error| format!("Failed to own the X11 clipboard: {error}"))?;
        connection
            .flush()
            .map_err(|error| format!("Failed to publish the X11 clipboard: {error}"))?;
        let owner = connection
            .get_selection_owner(atoms.CLIPBOARD)
            .map_err(|error| format!("Failed to verify X11 clipboard ownership: {error}"))?
            .reply()
            .map_err(|error| format!("Failed to verify X11 clipboard ownership: {error}"))?
            .owner;
        if owner != window {
            return Err("X11 rejected clipboard ownership".into());
        }

        let root = screen.root;
        std::thread::spawn(move || {
            if serve(
                connection,
                window,
                root,
                atoms,
                text.as_bytes(),
                &sender,
                &paste_armed,
            )
            .is_err()
            {
                let _ = sender.send(LinuxProviderEvent::ProviderFailed);
            }
        });
        Ok(())
    }

    fn serve(
        connection: RustConnection,
        window: u32,
        root: u32,
        atoms: Atoms,
        text: &[u8],
        sender: &Sender<LinuxProviderEvent>,
        paste_armed: &AtomicBool,
    ) -> Result<(), ()> {
        let mut state = LinuxProviderState::Publishing;
        loop {
            let event = if state == LinuxProviderState::Publishing {
                match connection.poll_for_event().map_err(|_| ())? {
                    Some(event) => event,
                    None if paste_armed.load(Ordering::Acquire) => {
                        // All requests already queued by eager clipboard managers have been
                        // served. Acknowledge arming before the main thread sends the chord.
                        state = linux_provider_transition(state, LinuxProviderInput::ArmPaste);
                        let _ = sender.send(LinuxProviderEvent::PasteArmed);
                        continue;
                    }
                    None => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                }
            } else {
                connection.wait_for_event().map_err(|_| ())?
            };
            match event {
                Event::SelectionClear(event) if event.owner == window => {
                    state = linux_provider_transition(state, LinuxProviderInput::SelectionClear);
                    if state == LinuxProviderState::OwnershipLost {
                        let _ = sender.send(LinuxProviderEvent::OwnershipLost);
                    }
                    return Ok(());
                }
                Event::SelectionRequest(event) if event.owner == window => {
                    let property = if event.property == x11rb::NONE {
                        event.target
                    } else {
                        event.property
                    };
                    let mut delivered_text = false;
                    let success = if event.target == atoms.TARGETS {
                        connection
                            .change_property32(
                                PropMode::REPLACE,
                                event.requestor,
                                property,
                                AtomEnum::ATOM,
                                &[
                                    atoms.TARGETS,
                                    atoms.UTF8_STRING,
                                    atoms.UTF8_MIME_0,
                                    atoms.UTF8_MIME_1,
                                    atoms.TEXT,
                                    atoms.X_KDE_PASSWORDMANAGERHINT,
                                ],
                            )
                            .is_ok()
                    } else if event.target == atoms.X_KDE_PASSWORDMANAGERHINT {
                        connection
                            .change_property8(
                                PropMode::REPLACE,
                                event.requestor,
                                property,
                                event.target,
                                b"secret",
                            )
                            .is_ok()
                    } else if [
                        atoms.UTF8_STRING,
                        atoms.UTF8_MIME_0,
                        atoms.UTF8_MIME_1,
                        atoms.TEXT,
                    ]
                    .contains(&event.target)
                    {
                        delivered_text = connection
                            .change_property8(
                                PropMode::REPLACE,
                                event.requestor,
                                property,
                                event.target,
                                text,
                            )
                            .is_ok();
                        delivered_text
                    } else {
                        false
                    };

                    let notify_property = if success {
                        property
                    } else {
                        AtomEnum::NONE.into()
                    };
                    if connection
                        .send_event(
                            false,
                            event.requestor,
                            EventMask::NO_EVENT,
                            SelectionNotifyEvent {
                                response_type: SELECTION_NOTIFY_EVENT,
                                sequence: event.sequence,
                                time: event.time,
                                requestor: event.requestor,
                                selection: event.selection,
                                target: event.target,
                                property: notify_property,
                            },
                        )
                        .is_err()
                        || connection.flush().is_err()
                    {
                        return Err(());
                    }

                    if delivered_text && state != LinuxProviderState::Confirmed {
                        let intended_consumer = requestor_matches_focused_client(
                            &connection,
                            event.requestor,
                            root,
                            &atoms,
                        );
                        state = linux_provider_transition(
                            state,
                            LinuxProviderInput::TextRequested { intended_consumer },
                        );
                        if state == LinuxProviderState::Confirmed {
                            let _ = sender.send(LinuxProviderEvent::DeliveryConfirmed);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    #[derive(Default)]
    struct WindowIdentity {
        lineage: Vec<u32>,
        leader: Option<u32>,
        pid: Option<u32>,
    }

    fn requestor_matches_focused_client(
        connection: &RustConnection,
        requestor: u32,
        root: u32,
        atoms: &Atoms,
    ) -> bool {
        let focus = connection
            .get_input_focus()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.focus)
            // X11 reserves 0 for None and 1 for PointerRoot.
            .filter(|window| *window > 1)
            .or_else(|| {
                connection
                    .get_property(false, root, atoms.NET_ACTIVE_WINDOW, AtomEnum::WINDOW, 0, 1)
                    .ok()?
                    .reply()
                    .ok()?
                    .value32()?
                    .next()
            });
        let Some(focus) = focus else {
            return false;
        };

        let requestor = window_identity(connection, requestor, root, atoms);
        let focus = window_identity(connection, focus, root, atoms);

        requestor
            .lineage
            .iter()
            .any(|window| focus.lineage.contains(window))
            || requestor
                .leader
                .zip(focus.leader)
                .is_some_and(|(left, right)| left == right)
            || requestor
                .pid
                .zip(focus.pid)
                .is_some_and(|(left, right)| left == right)
    }

    fn window_identity(
        connection: &RustConnection,
        window: u32,
        root: u32,
        atoms: &Atoms,
    ) -> WindowIdentity {
        let mut identity = WindowIdentity::default();
        let mut current = window;

        for _ in 0..32 {
            if current == x11rb::NONE || current == root {
                break;
            }
            identity.lineage.push(current);
            identity.leader = identity.leader.or_else(|| {
                connection
                    .get_property(
                        false,
                        current,
                        atoms.WM_CLIENT_LEADER,
                        AtomEnum::WINDOW,
                        0,
                        1,
                    )
                    .ok()?
                    .reply()
                    .ok()?
                    .value32()?
                    .next()
            });
            identity.pid = identity.pid.or_else(|| {
                connection
                    .get_property(false, current, atoms.NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
                    .ok()?
                    .reply()
                    .ok()?
                    .value32()?
                    .next()
            });

            let Some(parent) = connection
                .query_tree(current)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.parent)
            else {
                break;
            };
            if parent == current {
                break;
            }
            current = parent;
        }

        identity
    }
}

#[cfg(target_os = "linux")]
struct LinuxPasteWaiter {
    events: std::sync::mpsc::Receiver<LinuxProviderEvent>,
    paste_armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
impl LinuxPasteWaiter {
    fn arm(&self, timeout: Duration) -> Result<(), LinuxPasteWaitOutcome> {
        self.paste_armed
            .store(true, std::sync::atomic::Ordering::Release);

        match self.events.recv_timeout(timeout) {
            Ok(LinuxProviderEvent::PasteArmed) => Ok(()),
            Ok(LinuxProviderEvent::OwnershipLost) => Err(LinuxPasteWaitOutcome::OwnershipLost),
            Ok(LinuxProviderEvent::ProviderFailed) => Err(LinuxPasteWaitOutcome::ProviderFailed),
            Ok(LinuxProviderEvent::DeliveryConfirmed) => {
                // Delivery cannot precede the paste chord. Treat it as a provider failure
                // instead of allowing an untrusted request to authorize auto-submit.
                Err(LinuxPasteWaitOutcome::ProviderFailed)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(LinuxPasteWaitOutcome::TimedOut),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(LinuxPasteWaitOutcome::ProviderFailed)
            }
        }
    }

    fn event_before_paste(&self) -> Option<LinuxProviderEvent> {
        self.events.try_recv().ok()
    }

    fn wait_until_consumed(&self, timeout: Duration) -> LinuxPasteWaitOutcome {
        match self.events.recv_timeout(timeout) {
            Ok(LinuxProviderEvent::PasteArmed) => LinuxPasteWaitOutcome::ProviderFailed,
            Ok(LinuxProviderEvent::DeliveryConfirmed) => LinuxPasteWaitOutcome::Confirmed,
            Ok(LinuxProviderEvent::OwnershipLost) => LinuxPasteWaitOutcome::OwnershipLost,
            Ok(LinuxProviderEvent::ProviderFailed) => LinuxPasteWaitOutcome::ProviderFailed,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => LinuxPasteWaitOutcome::TimedOut,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                LinuxPasteWaitOutcome::ProviderFailed
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxPasteWaitOutcome {
    Confirmed,
    OwnershipLost,
    ProviderFailed,
    TimedOut,
}

#[cfg(target_os = "linux")]
fn start_x11_paste_waiter(text: &str) -> Result<LinuxPasteWaiter, String> {
    use std::sync::{atomic::AtomicBool, mpsc, Arc};

    let (sender, events) = mpsc::channel();
    let paste_armed = Arc::new(AtomicBool::new(false));

    x11_paste_provider::start(text.to_owned(), sender, Arc::clone(&paste_armed))?;
    Ok(LinuxPasteWaiter {
        events,
        paste_armed,
    })
}

/// Pastes through a temporary clipboard offer and restores only after consumption.
///
/// Windows uses delayed rendering, X11 verifies the requesting client, and macOS
/// verifies the focused accessibility value. Wayland has no trustworthy requester
/// identity, so it keeps an app-owned copy and dispatches without claiming confirmation.
fn paste_via_clipboard(
    enigo: &mut Enigo,
    text: &str,
    app_handle: &AppHandle,
    paste_method: &PasteMethod,
    paste_delay_ms: u64,
    paste_delay_after_ms: u64,
) -> Result<ClipboardPasteOutcome, String> {
    // macOS can inject arbitrary Unicode directly through CGEvent. Using that
    // transport for the clipboard-style methods avoids touching the user's
    // pasteboard at all, so there is no publication/restoration race and no
    // accessibility-value heuristic that can turn a successful paste into an
    // error. CopyToClipboard is still applied explicitly after successful
    // delivery by `paste`.
    if should_use_direct_text_transport(cfg!(target_os = "macos"), paste_method) {
        info!("Using clipboard-independent Unicode text injection on macOS");
        input::paste_text_direct(enigo, text)?;
        return Ok(ClipboardPasteOutcome::Confirmed);
    }

    let clipboard = app_handle.clipboard();

    #[cfg(target_os = "linux")]
    if linux_clipboard_strategy(is_wayland()) == LinuxClipboardStrategy::WaylandPersistentDispatch {
        // The data-control API reports transfers but not the identity of the requesting
        // client, so an eager clipboard manager is indistinguishable from the paste target.
        // Publish through the app-owned clipboard, dispatch the chord without a temporary
        // provider, and return immediately without waiting or restoring.
        clipboard
            .write_text(text)
            .map_err(|error| format!("Failed to publish Wayland clipboard: {error}"))?;

        let key_combo_sent = try_send_key_combo_linux(paste_method)?;
        if !key_combo_sent {
            match paste_method {
                PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo)?,
                PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo)?,
                PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo)?,
                _ => return Err("Invalid paste method for clipboard paste".into()),
            }
        }
        return Ok(wayland_dispatch_outcome());
    }

    let saved_clipboard = capture_clipboard(app_handle);

    #[cfg(target_os = "windows")]
    {
        match paste_via_win_text_inject(text, paste_method, paste_delay_ms, paste_delay_after_ms) {
            Ok(WindowsPasteOutcome::Complete {
                read_confirmed: true,
            }) => {
                restore_saved_clipboard(app_handle, saved_clipboard, text);
                return Ok(ClipboardPasteOutcome::Confirmed);
            }
            Ok(WindowsPasteOutcome::Complete {
                read_confirmed: false,
            }) => {
                // Without an observed read, restoring can recreate the stale-paste race.
                // Keep the transcript available for a manual retry instead.
                return Ok(ClipboardPasteOutcome::Unconfirmed(
                    "Paste delivery could not be confirmed; the transcript remains on the clipboard for manual retry"
                        .into(),
                ));
            }
            Ok(WindowsPasteOutcome::ManualPaste(message)) => return Err(message),
            Err(error) => warn!(
                "Synchronized Windows paste failed; falling back to guarded timer paste: {error}"
            ),
        }
    }

    #[cfg(target_os = "macos")]
    let focused_text_before = macos_paste_verification::focused_text_value();

    #[cfg(target_os = "linux")]
    let x11_waiter = match start_x11_paste_waiter(text) {
        Ok(waiter) => waiter,
        Err(error) => {
            warn!("{error}; leaving a regular recovery copy without sending paste");
            clipboard
                .write_text(text)
                .map_err(|write_error| write_error.to_string())?;
            return Ok(ClipboardPasteOutcome::Unconfirmed(
                "Synchronized clipboard delivery was unavailable; the transcript remains on the clipboard for manual retry"
                    .into(),
            ));
        }
    };

    // The X11 provider already owns the selection; other platforms publish here.
    #[cfg(target_os = "linux")]
    let write_result: Result<(), String> = Ok(());

    #[cfg(not(target_os = "linux"))]
    let write_result = clipboard
        .write_text(text)
        .map_err(|e| format!("Failed to write to clipboard: {}", e));

    write_result?;

    #[cfg(not(target_os = "linux"))]
    if !wait_for_clipboard_publication(app_handle, text, Duration::from_millis(500)) {
        return Err("Clipboard did not publish the transcription before paste".into());
    }

    std::thread::sleep(Duration::from_millis(paste_delay_ms));

    #[cfg(target_os = "linux")]
    if let Some(event) = x11_waiter.event_before_paste() {
        return Ok(ClipboardPasteOutcome::Unconfirmed(format!(
            "Clipboard ownership was lost before paste ({event:?}); the current clipboard was left untouched"
        )));
    }

    #[cfg(target_os = "linux")]
    if let Err(outcome) = x11_waiter.arm(Duration::from_millis(500)) {
        return Ok(ClipboardPasteOutcome::Unconfirmed(format!(
            "Clipboard provider could not arm paste ({outcome:?}); the current clipboard was left untouched"
        )));
    }

    // Send paste key combo
    #[cfg(target_os = "linux")]
    let key_combo_sent = try_send_key_combo_linux(paste_method)?;

    #[cfg(not(target_os = "linux"))]
    let key_combo_sent = false;

    // Fall back to enigo if no native tool handled it
    if !key_combo_sent {
        match paste_method {
            PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo)?,
            PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo)?,
            PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo)?,
            _ => return Err("Invalid paste method for clipboard paste".into()),
        }
    }

    #[cfg(target_os = "macos")]
    let mut consumption_confirmed = false;

    #[cfg(target_os = "windows")]
    {
        std::thread::sleep(Duration::from_millis(paste_delay_after_ms));
        return Ok(ClipboardPasteOutcome::Unconfirmed(
            "The synchronized Windows paste path failed and fallback delivery could not be confirmed; the transcript remains on the clipboard"
                .into(),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        let wait_timeout = Duration::from_millis(paste_delay_after_ms.max(2_000));
        match x11_waiter.wait_until_consumed(wait_timeout) {
            LinuxPasteWaitOutcome::Confirmed => {}
            LinuxPasteWaitOutcome::OwnershipLost => {
                warn!("Linux clipboard ownership changed before delivery was confirmed");
                return Ok(ClipboardPasteOutcome::Unconfirmed(
                    "Clipboard ownership changed before paste could be confirmed; the newer clipboard was left untouched"
                        .into(),
                ));
            }
            LinuxPasteWaitOutcome::ProviderFailed => {
                warn!("The Linux clipboard provider failed before delivery was confirmed");
                return Ok(ClipboardPasteOutcome::Unconfirmed(
                    "Clipboard delivery could not be confirmed; the current clipboard was left untouched"
                        .into(),
                ));
            }
            LinuxPasteWaitOutcome::TimedOut => {
                warn!(
                    "The Linux clipboard provider did not confirm the intended paste consumer; keeping the transcript available"
                );
                return Ok(ClipboardPasteOutcome::Unconfirmed(
                    "Paste delivery could not be confirmed; the transcript remains on the clipboard for manual retry"
                        .into(),
                ));
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(before) = focused_text_before.as_deref() {
            let deadline = Instant::now() + Duration::from_millis(paste_delay_after_ms.max(2_000));
            while Instant::now() < deadline {
                if let Some(after) = macos_paste_verification::focused_text_value() {
                    if macos_paste_observed(before, &after, text) {
                        consumption_confirmed = true;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            if !consumption_confirmed {
                warn!(
                    "The macOS focused text value did not confirm the paste; keeping the transcript available"
                );
                return Ok(ClipboardPasteOutcome::Unconfirmed(
                    "Paste delivery could not be confirmed; the transcript remains on the clipboard for manual retry"
                        .into(),
                ));
            }
        } else {
            // Some targets (notably secure fields) do not expose AXValue. Restoring on a
            // timer here would recreate the original race, so retain a recovery copy.
            warn!(
                "The macOS target does not expose a verifiable text value; keeping the transcript available"
            );
            return Ok(ClipboardPasteOutcome::Unconfirmed(
                "Paste delivery could not be confirmed; the transcript remains on the clipboard for manual retry"
                    .into(),
            ));
        }
    }

    restore_saved_clipboard(app_handle, saved_clipboard, text);

    Ok(ClipboardPasteOutcome::Confirmed)
}

/// Attempts to send a key combination using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_send_key_combo_linux(paste_method: &PasteMethod) -> Result<bool, String> {
    if is_wayland() {
        // Wayland: prefer wtype (but not on KDE), then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        if !is_kde_wayland() && is_wtype_available() {
            info!("Using wtype for key combo");
            send_key_combo_via_wtype(paste_method)?;
            return Ok(true);
        }
        if is_dotool_available() {
            info!("Using dotool for key combo");
            send_key_combo_via_dotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available() {
            info!("Using xdotool for key combo");
            send_key_combo_via_xdotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Attempts to type text directly using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_direct_typing_linux(text: &str, preferred_tool: TypingTool) -> Result<bool, String> {
    // If user specified a tool, try only that one
    if preferred_tool != TypingTool::Auto {
        return match preferred_tool {
            TypingTool::Wtype if is_wtype_available() => {
                info!("Using user-specified wtype");
                type_text_via_wtype(text)?;
                Ok(true)
            }
            TypingTool::Kwtype if is_kwtype_available() => {
                info!("Using user-specified kwtype");
                type_text_via_kwtype(text)?;
                Ok(true)
            }
            TypingTool::Dotool if is_dotool_available() => {
                info!("Using user-specified dotool");
                type_text_via_dotool(text)?;
                Ok(true)
            }
            TypingTool::Ydotool if is_ydotool_available() => {
                info!("Using user-specified ydotool");
                type_text_via_ydotool(text)?;
                Ok(true)
            }
            TypingTool::Xdotool if is_xdotool_available() => {
                info!("Using user-specified xdotool");
                type_text_via_xdotool(text)?;
                Ok(true)
            }
            _ => Err(format!(
                "Typing tool {:?} is not available on this system",
                preferred_tool
            )),
        };
    }

    // Auto mode - existing fallback chain
    if is_wayland() {
        // KDE Wayland: prefer kwtype (uses KDE Fake Input protocol, supports umlauts)
        if is_kde_wayland() && is_kwtype_available() {
            info!("Using kwtype for direct text input on KDE Wayland");
            type_text_via_kwtype(text)?;
            return Ok(true);
        }
        // Wayland: prefer wtype, then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        if !is_kde_wayland() && is_wtype_available() {
            info!("Using wtype for direct text input");
            type_text_via_wtype(text)?;
            return Ok(true);
        }
        if is_dotool_available() {
            info!("Using dotool for direct text input");
            type_text_via_dotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available() {
            info!("Using xdotool for direct text input");
            type_text_via_xdotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available() {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Returns the list of available typing tools on this system.
/// Always includes "auto" as the first entry.
#[cfg(target_os = "linux")]
pub fn get_available_typing_tools() -> Vec<String> {
    let mut tools = vec!["auto".to_string()];
    if is_wtype_available() {
        tools.push("wtype".to_string());
    }
    if is_kwtype_available() {
        tools.push("kwtype".to_string());
    }
    if is_dotool_available() {
        tools.push("dotool".to_string());
    }
    if is_ydotool_available() {
        tools.push("ydotool".to_string());
    }
    if is_xdotool_available() {
        tools.push("xdotool".to_string());
    }
    tools
}

/// Check if wtype is available (Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_wtype_available() -> bool {
    Command::new("which")
        .arg("wtype")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if dotool is available (another Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_dotool_available() -> bool {
    Command::new("which")
        .arg("dotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if ydotool is available (uinput-based, works on both Wayland and X11)
#[cfg(target_os = "linux")]
fn is_ydotool_available() -> bool {
    Command::new("which")
        .arg("ydotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn is_xdotool_available() -> bool {
    Command::new("which")
        .arg("xdotool")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if kwtype is available (KDE Wayland virtual keyboard input tool)
#[cfg(target_os = "linux")]
fn is_kwtype_available() -> bool {
    Command::new("which")
        .arg("kwtype")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if wl-copy is available (Wayland clipboard tool)
#[cfg(target_os = "linux")]
fn is_wl_copy_available() -> bool {
    Command::new("which")
        .arg("wl-copy")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Type text directly via wtype on Wayland.
#[cfg(target_os = "linux")]
fn type_text_via_wtype(text: &str) -> Result<(), String> {
    let output = Command::new("wtype")
        .arg("--") // Protect against text starting with -
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via xdotool on X11.
#[cfg(target_os = "linux")]
fn type_text_via_xdotool(text: &str) -> Result<(), String> {
    let output = Command::new("xdotool")
        .arg("type")
        .arg("--clearmodifiers")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via dotool (works on both Wayland and X11 via uinput).
#[cfg(target_os = "linux")]
fn type_text_via_dotool(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("dotool")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn dotool: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        // dotool uses "type <text>" command
        writeln!(stdin, "type {}", text)
            .map_err(|e| format!("Failed to write to dotool stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for dotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("dotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via ydotool (uinput-based, requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn type_text_via_ydotool(text: &str) -> Result<(), String> {
    let output = Command::new("ydotool")
        .arg("type")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via kwtype (KDE Wayland virtual keyboard, uses KDE Fake Input protocol).
#[cfg(target_os = "linux")]
fn type_text_via_kwtype(text: &str) -> Result<(), String> {
    let output = Command::new("kwtype")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute kwtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kwtype failed: {}", stderr));
    }

    Ok(())
}

/// Write text to clipboard via wl-copy (Wayland clipboard tool).
/// Uses Stdio::null() to avoid blocking on repeated calls — wl-copy forks a
/// daemon that inherits piped fds, causing read_to_end to hang indefinitely.
#[cfg(target_os = "linux")]
fn write_clipboard_via_wl_copy(text: &str) -> Result<(), String> {
    use std::process::Stdio;
    let status = Command::new("wl-copy")
        .arg("--")
        .arg(text)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute wl-copy: {}", e))?;

    if !status.success() {
        return Err("wl-copy failed".into());
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via wtype on Wayland.
#[cfg(target_os = "linux")]
fn send_key_combo_via_wtype(paste_method: &PasteMethod) -> Result<(), String> {
    let args: Vec<&str> = match paste_method {
        PasteMethod::CtrlV => vec!["-M", "ctrl", "-k", "v"],
        PasteMethod::ShiftInsert => vec!["-M", "shift", "-k", "Insert"],
        PasteMethod::CtrlShiftV => vec!["-M", "ctrl", "-M", "shift", "-k", "v"],
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("wtype")
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via dotool.
#[cfg(target_os = "linux")]
fn send_key_combo_via_dotool(paste_method: &PasteMethod) -> Result<(), String> {
    let command;
    match paste_method {
        PasteMethod::CtrlV => command = "echo key ctrl+v | dotool",
        PasteMethod::ShiftInsert => command = "echo key shift+insert | dotool",
        PasteMethod::CtrlShiftV => command = "echo key ctrl+shift+v | dotool",
        _ => return Err("Unsupported paste method".into()),
    }
    use std::process::Stdio;
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to execute dotool: {}", e))?;
    if !status.success() {
        return Err("dotool failed".into());
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via ydotool (requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn send_key_combo_via_ydotool(paste_method: &PasteMethod) -> Result<(), String> {
    // ydotool uses Linux input event keycodes with format <keycode>:<pressed>
    // where pressed is 1 for down, 0 for up. Keycodes: ctrl=29, shift=42, v=47, insert=110
    let args: Vec<&str> = match paste_method {
        PasteMethod::CtrlV => vec!["key", "29:1", "47:1", "47:0", "29:0"],
        PasteMethod::ShiftInsert => vec!["key", "42:1", "110:1", "110:0", "42:0"],
        PasteMethod::CtrlShiftV => vec!["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"],
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("ydotool")
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via xdotool on X11.
#[cfg(target_os = "linux")]
fn send_key_combo_via_xdotool(paste_method: &PasteMethod) -> Result<(), String> {
    let key_combo = match paste_method {
        PasteMethod::CtrlV => "ctrl+v",
        PasteMethod::CtrlShiftV => "ctrl+shift+v",
        PasteMethod::ShiftInsert => "shift+Insert",
        _ => return Err("Unsupported paste method".into()),
    };

    let output = Command::new("xdotool")
        .arg("key")
        .arg("--clearmodifiers")
        .arg(key_combo)
        .output()
        .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Pastes text by invoking an external script.
/// The script receives the text to paste as a single argument.
fn paste_via_external_script(text: &str, script_path: &str) -> Result<(), String> {
    info!("Pasting via external script: {}", script_path);

    let output = Command::new(script_path)
        .arg(text)
        .output()
        .map_err(|e| format!("Failed to execute external script '{}': {}", script_path, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "External script '{}' failed with exit code {:?}. stderr: {}, stdout: {}",
            script_path,
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        ));
    }

    Ok(())
}

/// Types text directly by simulating individual key presses.
fn paste_direct(
    enigo: &mut Enigo,
    text: &str,
    #[cfg(target_os = "linux")] typing_tool: TypingTool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if try_direct_typing_linux(text, typing_tool)? {
            return Ok(());
        }
        info!("Falling back to enigo for direct text input");
    }

    input::paste_text_direct(enigo, text)
}

fn send_return_key(enigo: &mut Enigo, key_type: AutoSubmitKey) -> Result<(), String> {
    match key_type {
        AutoSubmitKey::Enter => {
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
        }
        AutoSubmitKey::CtrlEnter => {
            enigo
                .key(Key::Control, Direction::Press)
                .map_err(|e| format!("Failed to press Control key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Control, Direction::Release)
                .map_err(|e| format!("Failed to release Control key: {}", e))?;
        }
        AutoSubmitKey::CmdEnter => {
            enigo
                .key(Key::Meta, Direction::Press)
                .map_err(|e| format!("Failed to press Meta/Cmd key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Meta, Direction::Release)
                .map_err(|e| format!("Failed to release Meta/Cmd key: {}", e))?;
        }
    }

    Ok(())
}

fn should_send_auto_submit(
    auto_submit: bool,
    paste_method: PasteMethod,
    outcome: &ClipboardPasteOutcome,
) -> bool {
    auto_submit
        && paste_method != PasteMethod::None
        && matches!(outcome, ClipboardPasteOutcome::Confirmed)
}

fn should_write_post_paste_clipboard(
    copy_to_clipboard: bool,
    outcome: &ClipboardPasteOutcome,
) -> bool {
    copy_to_clipboard && matches!(outcome, ClipboardPasteOutcome::Confirmed)
}

pub fn paste(text: String, app_handle: AppHandle) -> Result<(), String> {
    let settings = get_settings(&app_handle);
    let paste_method = settings.paste_method;
    let paste_delay_ms = settings.paste_delay_ms;
    let paste_delay_after_ms = settings.paste_delay_after_ms;

    // Append trailing space if setting is enabled
    let text = if settings.append_trailing_space {
        format!("{} ", text)
    } else {
        text
    };

    info!(
        "Using paste method: {:?}, delay before: {}ms, delay after: {}ms",
        paste_method, paste_delay_ms, paste_delay_after_ms
    );

    // Get the managed Enigo instance
    let enigo_state = app_handle
        .try_state::<EnigoState>()
        .ok_or("Enigo state not initialized")?;
    let mut enigo = enigo_state
        .0
        .lock()
        .map_err(|e| format!("Failed to lock Enigo: {}", e))?;

    // Perform the paste operation
    let paste_outcome = match paste_method {
        PasteMethod::None => {
            info!("PasteMethod::None selected - skipping paste action");
            ClipboardPasteOutcome::Confirmed
        }
        PasteMethod::Direct => {
            paste_direct(
                &mut enigo,
                &text,
                #[cfg(target_os = "linux")]
                settings.typing_tool,
            )?;
            ClipboardPasteOutcome::Confirmed
        }
        PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert => {
            paste_via_clipboard(
                &mut enigo,
                &text,
                &app_handle,
                &paste_method,
                paste_delay_ms,
                paste_delay_after_ms,
            )?
        }
        PasteMethod::ExternalScript => {
            let script_path = settings
                .external_script_path
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or("External script path is not configured")?;
            paste_via_external_script(&text, script_path)?;
            ClipboardPasteOutcome::Confirmed
        }
    };

    if let Some(message) = paste_outcome.failure_message() {
        return Err(message.to_owned());
    }

    if should_send_auto_submit(settings.auto_submit, paste_method, &paste_outcome) {
        std::thread::sleep(Duration::from_millis(50));
        send_return_key(&mut enigo, settings.auto_submit_key)?;
    }

    // After pasting, optionally copy to clipboard based on settings
    if should_write_post_paste_clipboard(
        settings.clipboard_handling == ClipboardHandling::CopyToClipboard,
        &paste_outcome,
    ) {
        let clipboard = app_handle.clipboard();
        clipboard
            .write_text(&text)
            .map_err(|e| format!("Failed to copy to clipboard: {}", e))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_submit_requires_setting_enabled() {
        assert!(!should_send_auto_submit(
            false,
            PasteMethod::CtrlV,
            &ClipboardPasteOutcome::Confirmed
        ));
        assert!(!should_send_auto_submit(
            false,
            PasteMethod::Direct,
            &ClipboardPasteOutcome::Confirmed
        ));
    }

    #[test]
    fn macos_clipboard_methods_never_use_the_clipboard_as_transport() {
        for method in [
            PasteMethod::CtrlV,
            PasteMethod::CtrlShiftV,
            PasteMethod::ShiftInsert,
        ] {
            assert!(should_use_direct_text_transport(true, &method));
            assert!(!should_use_direct_text_transport(false, &method));
        }

        assert!(!should_use_direct_text_transport(
            true,
            &PasteMethod::Direct
        ));
        assert!(!should_use_direct_text_transport(true, &PasteMethod::None));
    }

    #[test]
    fn auto_submit_skips_none_paste_method() {
        assert!(!should_send_auto_submit(
            true,
            PasteMethod::None,
            &ClipboardPasteOutcome::Confirmed
        ));
    }

    #[test]
    fn auto_submit_runs_for_active_paste_methods() {
        for method in [
            PasteMethod::CtrlV,
            PasteMethod::Direct,
            PasteMethod::CtrlShiftV,
            PasteMethod::ShiftInsert,
        ] {
            assert!(should_send_auto_submit(
                true,
                method,
                &ClipboardPasteOutcome::Confirmed
            ));
        }
    }

    #[test]
    fn auto_submit_is_blocked_for_unconfirmed_delivery() {
        let outcome = ClipboardPasteOutcome::Unconfirmed("manual retry".into());
        assert!(!should_send_auto_submit(true, PasteMethod::CtrlV, &outcome));
        assert_eq!(outcome.failure_message(), Some("manual retry"));
    }

    #[test]
    fn wayland_dispatches_without_wait_restore_or_error() {
        assert_eq!(
            linux_clipboard_strategy(true),
            LinuxClipboardStrategy::WaylandPersistentDispatch
        );
        assert_eq!(
            linux_clipboard_strategy(false),
            LinuxClipboardStrategy::VerifiedX11
        );

        let outcome = wayland_dispatch_outcome();
        assert_eq!(outcome, ClipboardPasteOutcome::DispatchedUnverified);
        assert_eq!(outcome.failure_message(), None);
        assert!(!should_send_auto_submit(true, PasteMethod::CtrlV, &outcome));
        assert!(!should_write_post_paste_clipboard(true, &outcome));
        assert!(should_write_post_paste_clipboard(
            true,
            &ClipboardPasteOutcome::Confirmed
        ));
    }

    #[test]
    fn restore_only_replaces_our_transcript() {
        assert_eq!(
            clipboard_restore_decision(Some("new transcript"), "new transcript", false),
            ClipboardRestoreDecision::Restore
        );
        assert_eq!(
            clipboard_restore_decision(Some("user copied this"), "new transcript", false),
            ClipboardRestoreDecision::ClipboardChanged
        );
    }

    #[test]
    fn missing_or_non_text_clipboard_is_never_overwritten_during_restore() {
        assert_eq!(
            clipboard_restore_decision(None, "new transcript", false),
            ClipboardRestoreDecision::TranscriptUnavailable
        );
        assert_eq!(
            clipboard_restore_decision(None, "new transcript", true),
            ClipboardRestoreDecision::ClipboardChanged
        );
    }

    #[test]
    fn eager_clipboard_manager_requests_do_not_confirm_or_terminate_provider() {
        let state = linux_provider_transition(
            LinuxProviderState::Publishing,
            LinuxProviderInput::TextRequested {
                intended_consumer: false,
            },
        );
        assert_eq!(state, LinuxProviderState::Publishing);

        let state = linux_provider_transition(state, LinuxProviderInput::ArmPaste);
        let state = linux_provider_transition(
            state,
            LinuxProviderInput::TextRequested {
                intended_consumer: false,
            },
        );
        assert_eq!(state, LinuxProviderState::AwaitingConsumer);
    }

    #[test]
    fn only_intended_post_arm_consumer_confirms_linux_delivery() {
        let state =
            linux_provider_transition(LinuxProviderState::Publishing, LinuxProviderInput::ArmPaste);
        let state = linux_provider_transition(
            state,
            LinuxProviderInput::TextRequested {
                intended_consumer: true,
            },
        );
        assert_eq!(state, LinuxProviderState::Confirmed);
    }

    #[test]
    fn selection_clear_before_confirmation_is_not_delivery() {
        for state in [
            LinuxProviderState::Publishing,
            LinuxProviderState::AwaitingConsumer,
        ] {
            assert_eq!(
                linux_provider_transition(state, LinuxProviderInput::SelectionClear),
                LinuxProviderState::OwnershipLost
            );
        }
        assert_eq!(
            linux_provider_transition(
                LinuxProviderState::AwaitingConsumer,
                LinuxProviderInput::ProviderError
            ),
            LinuxProviderState::Failed
        );
    }

    #[test]
    fn macos_verification_requires_an_observable_text_change() {
        assert!(macos_paste_observed(
            "existing text",
            "existing textnew transcript",
            "new transcript"
        ));
        assert!(!macos_paste_observed(
            "new transcript",
            "new transcript",
            "new transcript"
        ));
        assert!(!macos_paste_observed(
            "existing text",
            "different edit",
            "new transcript"
        ));
    }
}
