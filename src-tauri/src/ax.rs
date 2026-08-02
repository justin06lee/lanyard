//! Thin, safe-ish wrapper over the macOS Accessibility API.
//!
//! We only need three things from AX:
//!   1. which application currently owns keyboard focus,
//!   2. the title of its focused window (our session identity channel), and
//!   3. that window's on-screen rect (so the floater can sit on top of it).
//!
//! AX deliberately only reports windows on the *active* Space, which is exactly
//! the semantics we want: whatever desktop the user is looking at.

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedApplicationAttribute, kAXFocusedWindowAttribute,
    kAXPositionAttribute, kAXSizeAttribute, kAXTitleAttribute, kAXTrustedCheckOptionPrompt,
    kAXValueTypeCGPoint, kAXValueTypeCGSize, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
    AXUIElementCopyAttributeValue, AXUIElementCreateSystemWide, AXUIElementGetPid, AXUIElementRef,
    AXUIElementSetMessagingTimeout, AXValueGetValue, AXValueRef,
};
use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use std::os::raw::c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgSize {
    width: f64,
    height: f64,
}

/// The window that currently has keyboard focus, in global top-left-origin points.
#[derive(Debug, Clone)]
pub struct FocusedWindow {
    pub pid: i32,
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Releases a CoreFoundation ref obtained under the "create" rule.
struct CfOwned(CFTypeRef);

impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

/// True once the user has ticked gru in System Settings › Privacy › Accessibility.
pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Same check, but pops the system prompt that deep-links into System Settings.
pub fn request_trust() -> bool {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let opts = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
    }
}

/// Copies an AX attribute, returning an owned CF ref.
unsafe fn copy_attr(element: AXUIElementRef, attribute: &str) -> Option<CfOwned> {
    if element.is_null() {
        return None;
    }
    let name = CFString::new(attribute);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    Some(CfOwned(value))
}

unsafe fn attr_string(element: AXUIElementRef, attribute: &str) -> Option<String> {
    let owned = copy_attr(element, attribute)?;
    if CFGetTypeID(owned.0) != CFString::type_id() {
        return None;
    }
    Some(CFString::wrap_under_get_rule(owned.0 as CFStringRef).to_string())
}

unsafe fn attr_point(element: AXUIElementRef, attribute: &str) -> Option<CgPoint> {
    let owned = copy_attr(element, attribute)?;
    let mut out = CgPoint::default();
    let ok = AXValueGetValue(
        owned.0 as AXValueRef,
        kAXValueTypeCGPoint,
        &mut out as *mut _ as *mut c_void,
    );
    ok.then_some(out)
}

unsafe fn attr_size(element: AXUIElementRef, attribute: &str) -> Option<CgSize> {
    let owned = copy_attr(element, attribute)?;
    let mut out = CgSize::default();
    let ok = AXValueGetValue(
        owned.0 as AXValueRef,
        kAXValueTypeCGSize,
        &mut out as *mut _ as *mut c_void,
    );
    ok.then_some(out)
}

/// Reads the currently focused window across the whole system.
///
/// Returns `None` when AX permission is missing, nothing is focused, or the
/// focused app exposes no focused window (common for menu-bar-only agents).
pub fn focused_window() -> Option<FocusedWindow> {
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return None;
        }
        let system = CfOwned(system as CFTypeRef);

        // Never let a wedged app stall the tracker loop.
        AXUIElementSetMessagingTimeout(system.0 as AXUIElementRef, 0.25);

        let app = copy_attr(system.0 as AXUIElementRef, kAXFocusedApplicationAttribute)?;
        let app_ref = app.0 as AXUIElementRef;

        let mut pid: libc::pid_t = 0;
        if AXUIElementGetPid(app_ref, &mut pid) != kAXErrorSuccess {
            return None;
        }

        let window = copy_attr(app_ref, kAXFocusedWindowAttribute)?;
        let window_ref = window.0 as AXUIElementRef;

        let title = attr_string(window_ref, kAXTitleAttribute).unwrap_or_default();
        let pos = attr_point(window_ref, kAXPositionAttribute).unwrap_or_default();
        let size = attr_size(window_ref, kAXSizeAttribute).unwrap_or_default();

        Some(FocusedWindow {
            pid: pid as i32,
            title,
            x: pos.x,
            y: pos.y,
            w: size.width,
            h: size.height,
        })
    }
}
