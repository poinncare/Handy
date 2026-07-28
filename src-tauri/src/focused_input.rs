//! Privacy-preserving focused editable discovery.
//!
//! The adapters intentionally return only an opaque identity and screen bounds.
//! They never request, retain, or log the value of the focused control.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ScreenRect {
    pub fn is_usable(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 1.0
            && self.height > 1.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusedInput {
    pub identity: u64,
    pub bounds: ScreenRect,
}

pub fn focused_editable() -> Option<FocusedInput> {
    platform::focused_editable()
}

pub fn probe() -> Result<(), String> {
    platform::probe()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{FocusedInput, ScreenRect};
    use std::ffi::{c_char, c_void};
    use std::ptr;

    type AXError = i32;
    type AXUIElementRef = *const c_void;
    type AXValueRef = *const c_void;
    type CFHashCode = usize;
    type CFIndex = isize;
    type CFStringEncoding = u32;
    type CFStringRef = *const c_void;
    type CFTypeID = usize;
    type CFTypeRef = *const c_void;

    const AX_ERROR_SUCCESS: AXError = 0;
    const AX_VALUE_CGPOINT_TYPE: u32 = 1;
    const AX_VALUE_CGSIZE_TYPE: u32 = 2;
    const CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;

    #[repr(C)]
    #[derive(Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        static kAXFocusedUIElementAttribute: CFStringRef;
        static kAXPositionAttribute: CFStringRef;
        static kAXRoleAttribute: CFStringRef;
        static kAXSizeAttribute: CFStringRef;
        static kAXSubroleAttribute: CFStringRef;
        static kAXValueAttribute: CFStringRef;

        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementIsAttributeSettable(
            element: AXUIElementRef,
            attribute: CFStringRef,
            settable: *mut u8,
        ) -> AXError;
        fn AXValueGetValue(value: AXValueRef, value_type: u32, value_ptr: *mut c_void) -> u8;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;
        fn CFHash(value: CFTypeRef) -> CFHashCode;
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
                // SAFETY: OwnedCf only wraps Create/Copy-rule CoreFoundation values.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn copy_attribute(element: AXUIElementRef, attribute: CFStringRef) -> Option<OwnedCf> {
        let mut value: CFTypeRef = ptr::null();
        // SAFETY: The element and exported AX attribute are valid CF objects.
        let result =
            unsafe { AXUIElementCopyAttributeValue(element, attribute, &mut value as *mut _) };
        (result == AX_ERROR_SUCCESS && !value.is_null()).then_some(OwnedCf(value))
    }

    fn cf_string(value: CFTypeRef) -> Option<String> {
        // SAFETY: CoreFoundation accepts any non-null CF object for a type query.
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            return None;
        }
        let value = value as CFStringRef;
        // SAFETY: The type check above established that this is a CFString.
        let length = unsafe { CFStringGetLength(value) };
        let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) };
        let buffer_size = maximum.checked_add(1)?;
        let mut buffer = vec![0_u8; usize::try_from(buffer_size).ok()?];
        // SAFETY: The buffer is writable for buffer_size bytes.
        if !unsafe {
            CFStringGetCString(
                value,
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

    fn point(value: CFTypeRef) -> Option<CGPoint> {
        let mut result = CGPoint::default();
        // SAFETY: AXValueGetValue validates the dynamic AXValue type before copying.
        (unsafe {
            AXValueGetValue(
                value as AXValueRef,
                AX_VALUE_CGPOINT_TYPE,
                (&mut result as *mut CGPoint).cast(),
            )
        } != 0)
            .then_some(result)
    }

    fn size(value: CFTypeRef) -> Option<CGSize> {
        let mut result = CGSize::default();
        // SAFETY: AXValueGetValue validates the dynamic AXValue type before copying.
        (unsafe {
            AXValueGetValue(
                value as AXValueRef,
                AX_VALUE_CGSIZE_TYPE,
                (&mut result as *mut CGSize).cast(),
            )
        } != 0)
            .then_some(result)
    }

    pub(super) fn focused_editable() -> Option<FocusedInput> {
        // SAFETY: AXUIElementCreateSystemWide returns a retained AX object.
        let system_wide = OwnedCf(unsafe { AXUIElementCreateSystemWide() });
        if system_wide.0.is_null() {
            return None;
        }
        let focused = copy_attribute(system_wide.0, unsafe { kAXFocusedUIElementAttribute })?;

        let role = copy_attribute(focused.0, unsafe { kAXRoleAttribute })
            .and_then(|value| cf_string(value.0))?;
        if !matches!(
            role.as_str(),
            "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
        ) {
            return None;
        }

        let subrole = copy_attribute(focused.0, unsafe { kAXSubroleAttribute })
            .and_then(|value| cf_string(value.0));
        if subrole.as_deref() == Some("AXSecureTextField") {
            return None;
        }

        let mut settable = 0_u8;
        // This checks editability without requesting the field value.
        // SAFETY: The focused AX element and exported value attribute are valid.
        if unsafe {
            AXUIElementIsAttributeSettable(focused.0, kAXValueAttribute, &mut settable as *mut u8)
        } != AX_ERROR_SUCCESS
            || settable == 0
        {
            return None;
        }

        let position = point(copy_attribute(focused.0, unsafe { kAXPositionAttribute })?.0)?;
        let size = size(copy_attribute(focused.0, unsafe { kAXSizeAttribute })?.0)?;
        let bounds = ScreenRect {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        };
        if !bounds.is_usable() {
            return None;
        }

        // SAFETY: CFHash accepts the retained AX element as a CF object.
        let identity = unsafe { CFHash(focused.0) } as u64;
        Some(FocusedInput { identity, bounds })
    }

    pub(super) fn probe() -> Result<(), String> {
        // KeyboardListener performs the authoritative Accessibility permission
        // check before the monitor thread is started.
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{FocusedInput, ScreenRect};
    use std::cell::RefCell;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED, SAFEARRAY,
    };
    use windows::Win32::System::Ole::{
        SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextEditPattern,
        IUIAutomationValuePattern, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
        UIA_TextEditPatternId, UIA_ValuePatternId,
    };

    struct AutomationContext {
        automation: IUIAutomation,
    }

    impl AutomationContext {
        fn new() -> windows::core::Result<Self> {
            // SAFETY: The monitor owns this dedicated thread and initializes COM once.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
            // SAFETY: CUIAutomation is the documented in-process UI Automation coclass.
            let automation =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
            Ok(Self { automation })
        }
    }

    impl Drop for AutomationContext {
        fn drop(&mut self) {
            // SAFETY: Paired with the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }

    thread_local! {
        static AUTOMATION: RefCell<Option<AutomationContext>> = const { RefCell::new(None) };
    }

    struct OwnedSafeArray(*mut SAFEARRAY);

    impl Drop for OwnedSafeArray {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: UI Automation returned ownership of this SAFEARRAY.
                let _ = unsafe { SafeArrayDestroy(self.0) };
            }
        }
    }

    fn runtime_identity(element: &IUIAutomationElement) -> Option<u64> {
        // SAFETY: The element is a live UI Automation proxy.
        let array = OwnedSafeArray(unsafe { element.GetRuntimeId().ok()? });
        if array.0.is_null() {
            return None;
        }
        // SAFEARRAY dimensions are 1-based.
        let lower = unsafe { SafeArrayGetLBound(array.0, 1).ok()? };
        let upper = unsafe { SafeArrayGetUBound(array.0, 1).ok()? };
        if upper < lower {
            return None;
        }

        let mut hasher = DefaultHasher::new();
        for index in lower..=upper {
            let mut value = 0_i32;
            // SAFETY: index is inside the bounds queried from this one-dimensional array.
            unsafe {
                SafeArrayGetElement(
                    array.0,
                    &index as *const i32,
                    (&mut value as *mut i32).cast(),
                )
                .ok()?
            };
            value.hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    fn query(context: &AutomationContext) -> Option<FocusedInput> {
        // SAFETY: All calls are made on the COM-initialized monitor thread.
        let element = unsafe { context.automation.GetFocusedElement().ok()? };
        let focusable = unsafe { element.CurrentIsKeyboardFocusable().ok()? }.as_bool();
        let password = unsafe { element.CurrentIsPassword().ok()? }.as_bool();
        let control_type = unsafe { element.CurrentControlType().ok()? };
        if !focusable || password {
            return None;
        }
        let editable = if control_type == UIA_EditControlTypeId {
            // ValuePattern provides a privacy-safe read-only bit; it does not
            // require requesting the current text value.
            unsafe {
                element
                    .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .ok()
                    .and_then(|pattern| pattern.CurrentIsReadOnly().ok())
                    .is_some_and(|read_only| !read_only.as_bool())
                    || element
                        .GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
                        .is_ok()
            }
        } else if control_type == UIA_DocumentControlTypeId {
            // A generic document may only be readable. TextEditPattern is the
            // semantic signal that this focused document accepts editing.
            unsafe {
                element
                    .GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
                    .is_ok()
            }
        } else {
            false
        };
        if !editable {
            return None;
        }

        let rect = unsafe { element.CurrentBoundingRectangle().ok()? };
        let bounds = ScreenRect {
            x: f64::from(rect.left),
            y: f64::from(rect.top),
            width: f64::from(rect.right - rect.left),
            height: f64::from(rect.bottom - rect.top),
        };
        if !bounds.is_usable() {
            return None;
        }
        Some(FocusedInput {
            identity: runtime_identity(&element)?,
            bounds,
        })
    }

    pub(super) fn focused_editable() -> Option<FocusedInput> {
        AUTOMATION.with(|slot| {
            if slot.borrow().is_none() {
                *slot.borrow_mut() = AutomationContext::new().ok();
            }
            slot.borrow().as_ref().and_then(query)
        })
    }

    pub(super) fn probe() -> Result<(), String> {
        // UI Automation has no consent prompt. Initialization is deliberately
        // deferred to the dedicated monitor thread to preserve COM apartment
        // ownership.
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{FocusedInput, ScreenRect};
    use atspi::proxy::accessible::ObjectRefExt;
    use atspi::proxy::proxy_ext::ProxyExt;
    use atspi::{
        AccessibilityConnection, CoordType, MatchType, ObjectMatchRule, Role, SortOrder, State,
    };
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::OnceLock;

    static CONNECTION: OnceLock<AccessibilityConnection> = OnceLock::new();

    async fn connection() -> Option<&'static AccessibilityConnection> {
        if let Some(connection) = CONNECTION.get() {
            return Some(connection);
        }
        let opened = AccessibilityConnection::new().await.ok()?;
        let _ = CONNECTION.set(opened);
        CONNECTION.get()
    }

    async fn focused_editable_async() -> Option<FocusedInput> {
        let connection = connection().await?;
        let registry_root = connection.root_accessible_on_registry().await.ok()?;
        let applications = registry_root.get_children().await.ok()?;
        let rule = ObjectMatchRule::builder()
            .states([State::Focused, State::Editable], MatchType::All)
            .build();

        for application in applications {
            let app_accessible = match application
                .as_accessible_proxy(connection.connection())
                .await
            {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
            let proxies = match app_accessible.proxies().await {
                Ok(proxies) => proxies,
                Err(_) => continue,
            };
            let collection = match proxies.collection().await {
                Ok(collection) => collection,
                Err(_) => continue,
            };
            let matches = match collection
                .get_matches(rule.clone(), SortOrder::Canonical, 1, true)
                .await
            {
                Ok(matches) => matches,
                Err(_) => continue,
            };
            let Some(object) = matches.into_iter().next() else {
                continue;
            };
            let accessible = match object.as_accessible_proxy(connection.connection()).await {
                Ok(accessible) => accessible,
                Err(_) => continue,
            };
            let role = match accessible.get_role().await {
                Ok(role) => role,
                Err(_) => continue,
            };
            if role == Role::PasswordText {
                continue;
            }
            let proxies = match accessible.proxies().await {
                Ok(proxies) => proxies,
                Err(_) => continue,
            };
            let component = match proxies.component().await {
                Ok(component) => component,
                Err(_) => continue,
            };
            let (x, y, width, height) = match component.get_extents(CoordType::Screen).await {
                Ok(bounds) => bounds,
                Err(_) => continue,
            };
            let bounds = ScreenRect {
                x: f64::from(x),
                y: f64::from(y),
                width: f64::from(width),
                height: f64::from(height),
            };
            if !bounds.is_usable() {
                continue;
            }
            let mut hasher = DefaultHasher::new();
            object.hash(&mut hasher);
            return Some(FocusedInput {
                identity: hasher.finish(),
                bounds,
            });
        }
        None
    }

    pub(super) fn focused_editable() -> Option<FocusedInput> {
        tauri::async_runtime::block_on(focused_editable_async())
    }

    pub(super) fn probe() -> Result<(), String> {
        tauri::async_runtime::block_on(connection())
            .map(|_| ())
            .ok_or_else(|| {
                "AT-SPI accessibility is unavailable; enable the desktop accessibility bus"
                    .to_string()
            })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use super::FocusedInput;

    pub(super) fn focused_editable() -> Option<FocusedInput> {
        None
    }

    pub(super) fn probe() -> Result<(), String> {
        Err("Focused editable discovery is unsupported on this platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::ScreenRect;

    #[test]
    fn rejects_empty_or_non_finite_bounds() {
        assert!(!ScreenRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 20.0,
        }
        .is_usable());
        assert!(!ScreenRect {
            x: f64::NAN,
            y: 0.0,
            width: 100.0,
            height: 20.0,
        }
        .is_usable());
    }

    #[test]
    fn accepts_normal_screen_bounds() {
        assert!(ScreenRect {
            x: -1200.0,
            y: 64.0,
            width: 420.0,
            height: 32.0,
        }
        .is_usable());
    }
}
