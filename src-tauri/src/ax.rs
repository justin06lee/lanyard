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
    kAXApplicationActivatedNotification, kAXApplicationDeactivatedNotification,
    kAXApplicationHiddenNotification, kAXApplicationShownNotification, kAXErrorSuccess,
    kAXFocusedApplicationAttribute, kAXFocusedWindowAttribute, kAXFocusedWindowChangedNotification,
    kAXFrontmostAttribute, kAXMainAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXSizeAttribute, kAXTitleAttribute, kAXTitleChangedNotification,
    kAXTrustedCheckOptionPrompt, kAXValueTypeCGPoint, kAXValueTypeCGSize,
    kAXWindowDeminiaturizedNotification, kAXWindowMiniaturizedNotification,
    kAXWindowMovedNotification, kAXWindowResizedNotification, kAXWindowsAttribute,
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXObserverAddNotification,
    AXObserverCreate, AXObserverGetRunLoopSource, AXObserverRef, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementGetPid,
    AXUIElementPerformAction, AXUIElementRef, AXUIElementSetAttributeValue,
    AXUIElementSetMessagingTimeout, AXValueGetValue, AXValueRef,
};
use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex};
use core_foundation_sys::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRemoveSource,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::os::raw::c_void;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

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

/// Must match `identifier` in tauri.conf.json — it names our own TCC entry.
pub const BUNDLE_ID: &str = "dev.justin06lee.lanyard";

/// True once the user has ticked Lanyard in System Settings › Privacy › Accessibility.
pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Same check, but pops the system prompt that deep-links into System Settings.
fn request_trust() -> bool {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let opts = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
    }
}

/// Requests Accessibility access in a way that always produces the prompt.
///
/// macOS keys the grant to the binary's code signature and only shows the
/// consent dialog while *no* TCC entry exists for the bundle id. An unsigned
/// app trips over that constantly: every rebuild strands the old grant, and a
/// once-dismissed prompt never returns — the request is swallowed silently and
/// the app looks broken. Clearing our own entry first (a no-op when none
/// exists) restores the prompt. A working grant is never touched: this returns
/// early when already trusted.
pub fn repair_trust() -> bool {
    if is_trusted() {
        return true;
    }
    let _ = std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", "Accessibility", BUNDLE_ID])
        .status();
    request_trust()
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

/// Explains, step by step, why `focused_window` returned what it did.
///
/// AX failures are otherwise indistinguishable from "nothing is focused", which
/// makes a missing floater impossible to diagnose from the outside.
pub fn focus_diagnostic() -> String {
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return "system-wide AX element is null".into();
        }
        let system = CfOwned(system as CFTypeRef);
        AXUIElementSetMessagingTimeout(system.0 as AXUIElementRef, 0.25);

        let attr = CFString::new(kAXFocusedApplicationAttribute);
        let mut value: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            system.0 as AXUIElementRef,
            attr.as_concrete_TypeRef(),
            &mut value,
        );
        if err != kAXErrorSuccess || value.is_null() {
            return format!("AXFocusedApplication failed (AXError {err})");
        }
        let app = CfOwned(value);

        let mut pid: libc::pid_t = 0;
        AXUIElementGetPid(app.0 as AXUIElementRef, &mut pid);

        let attr = CFString::new(kAXFocusedWindowAttribute);
        let mut window: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            app.0 as AXUIElementRef,
            attr.as_concrete_TypeRef(),
            &mut window,
        );
        if err != kAXErrorSuccess || window.is_null() {
            return format!("app pid {pid} has no AXFocusedWindow (AXError {err})");
        }
        CFRelease(window);
        format!("app pid {pid} reports a focused window")
    }
}

/// Probes a specific application's focused window. Used by `lanyard-doctor` to tell
/// a permissions problem apart from a system-wide-query problem.
pub fn probe_app(pid: i32) -> Result<String, String> {
    unsafe {
        let app = AXUIElementCreateApplication(pid as libc::pid_t);
        if app.is_null() {
            return Err("AXUIElementCreateApplication returned null".into());
        }
        let app = CfOwned(app as CFTypeRef);

        let attr = CFString::new(kAXFocusedWindowAttribute);
        let mut window: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            app.0 as AXUIElementRef,
            attr.as_concrete_TypeRef(),
            &mut window,
        );
        if err != kAXErrorSuccess || window.is_null() {
            return Err(format!("AXFocusedWindow -> AXError {err}"));
        }
        let window = CfOwned(window);
        attr_string(window.0 as AXUIElementRef, kAXTitleAttribute)
            .ok_or_else(|| "window has no AXTitle".into())
    }
}

/// Raises the terminal window hosting `session_pid` and brings its app to the
/// front. The window is found by the Lanyard token in its title, the same
/// channel focus resolution uses, so tabbed terminals resolve to the right
/// window. Activating the app also carries macOS to that window's Space.
pub fn raise_window(app_pid: i32, session_pid: i32) -> Result<(), String> {
    unsafe {
        let app = AXUIElementCreateApplication(app_pid as libc::pid_t);
        if app.is_null() {
            return Err("AXUIElementCreateApplication returned null".into());
        }
        let app = CfOwned(app as CFTypeRef);
        let app_ref = app.0 as AXUIElementRef;
        AXUIElementSetMessagingTimeout(app_ref, 0.25);

        let Some(windows) = copy_attr(app_ref, kAXWindowsAttribute) else {
            return Err("app reports no AXWindows".into());
        };
        let array = windows.0 as core_foundation_sys::array::CFArrayRef;
        let count = CFArrayGetCount(array);
        for i in 0..count {
            let window = CFArrayGetValueAtIndex(array, i) as AXUIElementRef;
            if window.is_null() {
                continue;
            }
            let title = attr_string(window, kAXTitleAttribute).unwrap_or_default();
            if crate::title::parse_pid(&title) != Some(session_pid) {
                continue;
            }
            // Make it the app's main window, lift it, then front the app.
            let main = CFString::new(kAXMainAttribute);
            let _ = AXUIElementSetAttributeValue(
                window,
                main.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            );
            let raise = CFString::new(kAXRaiseAction);
            let _ = AXUIElementPerformAction(window, raise.as_concrete_TypeRef());
            let front = CFString::new(kAXFrontmostAttribute);
            let err = AXUIElementSetAttributeValue(
                app_ref,
                front.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            );
            if err != kAXErrorSuccess {
                return Err(format!("AXFrontmost -> AXError {err}"));
            }
            return Ok(());
        }
        Err("no window carries this session's title tag".into())
    }
}

// ---------------------------------------------------------------- observers
//
// One AXObserver per terminal app, so focus changes arrive as events instead
// of being polled for. Every notification funnels into a single wake channel;
// the tracker doesn't care *what* changed, only that its picture is stale.

/// Everything worth waking for: which app is active, which window has focus,
/// where that window is, and what its title says (the identity channel).
const OBSERVED_NOTIFICATIONS: [&str; 9] = [
    kAXApplicationActivatedNotification,
    kAXApplicationDeactivatedNotification,
    kAXApplicationHiddenNotification,
    kAXApplicationShownNotification,
    kAXFocusedWindowChangedNotification,
    kAXWindowMovedNotification,
    kAXWindowResizedNotification,
    kAXWindowMiniaturizedNotification,
    kAXWindowDeminiaturizedNotification,
];

static WAKE: OnceLock<Sender<()>> = OnceLock::new();

/// Pids that currently have a live observer, readable from any thread — the
/// tracker uses it to decide whether it can trust events or must keep polling.
fn observed() -> &'static Mutex<HashSet<i32>> {
    static SET: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn observed_pids() -> HashSet<i32> {
    observed().lock().unwrap().clone()
}

/// Wires the channel the observers wake. Call once, before any observer exists.
pub fn install_wake_sender(tx: Sender<()>) {
    let _ = WAKE.set(tx);
}

/// AXObserverRefs are main-thread objects; they live in a main-thread-only
/// thread_local and never cross threads.
struct Observer {
    observer: AXObserverRef,
    element: AXUIElementRef,
}

thread_local! {
    static OBSERVERS: RefCell<HashMap<i32, Observer>> = RefCell::new(HashMap::new());
}

unsafe extern "C" fn on_ax_event(
    _observer: AXObserverRef,
    _element: AXUIElementRef,
    _notification: CFStringRef,
    _refcon: *mut c_void,
) {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

unsafe fn add_observer(pid: i32) -> Option<Observer> {
    let mut observer: AXObserverRef = std::ptr::null_mut();
    if AXObserverCreate(pid, on_ax_event, &mut observer) != kAXErrorSuccess || observer.is_null() {
        return None;
    }
    let element = AXUIElementCreateApplication(pid as libc::pid_t);
    if element.is_null() {
        CFRelease(observer as CFTypeRef);
        return None;
    }
    // Registering on the application element covers its windows too. Title
    // changes are asked for but not required — some apps never deliver them,
    // and the tracker's fallback tick covers that gap.
    let mut essential = 0;
    for name in OBSERVED_NOTIFICATIONS {
        let cf = CFString::new(name);
        if AXObserverAddNotification(observer, element, cf.as_concrete_TypeRef(), std::ptr::null_mut())
            == kAXErrorSuccess
        {
            essential += 1;
        }
    }
    let title = CFString::new(kAXTitleChangedNotification);
    let _ = AXObserverAddNotification(
        observer,
        element,
        title.as_concrete_TypeRef(),
        std::ptr::null_mut(),
    );
    if essential == 0 {
        CFRelease(element as CFTypeRef);
        CFRelease(observer as CFTypeRef);
        return None;
    }
    CFRunLoopAddSource(
        CFRunLoopGetMain(),
        AXObserverGetRunLoopSource(observer),
        kCFRunLoopCommonModes,
    );
    Some(Observer { observer, element })
}

unsafe fn remove_observer(entry: &Observer) {
    CFRunLoopRemoveSource(
        CFRunLoopGetMain(),
        AXObserverGetRunLoopSource(entry.observer),
        kCFRunLoopCommonModes,
    );
    CFRelease(entry.element as CFTypeRef);
    CFRelease(entry.observer as CFTypeRef);
}

/// Reconciles the observer set with the terminal apps hosting sessions.
/// Main thread only — dispatch here via `run_on_main_thread`.
pub fn sync_observers(pids: &[i32]) {
    let want: HashSet<i32> = pids.iter().copied().collect();
    OBSERVERS.with(|cell| {
        let mut map = cell.borrow_mut();
        map.retain(|pid, entry| {
            if want.contains(pid) {
                true
            } else {
                unsafe { remove_observer(entry) };
                false
            }
        });
        for &pid in &want {
            if map.contains_key(&pid) {
                continue;
            }
            if let Some(entry) = unsafe { add_observer(pid) } {
                map.insert(pid, entry);
            }
        }
        *observed().lock().unwrap() = map.keys().copied().collect();
    });
}

unsafe fn attr_bool(element: AXUIElementRef, attribute: &str) -> Option<bool> {
    let owned = copy_attr(element, attribute)?;
    // kCFBooleanTrue is a singleton, so identity is a valid test.
    Some(owned.0 == CFBoolean::true_value().as_CFTypeRef())
}

/// Reads the focused window of whichever of `app_pids` is currently frontmost.
///
/// Deliberately *not* the system-wide `AXFocusedApplication` attribute: that
/// query returns kAXErrorCannotComplete (-25204) on this setup even with
/// Accessibility fully granted, while per-application queries work fine.
/// Scoping to known terminal apps is also exactly the semantics Lanyard wants —
/// when the user is in a browser, nothing matches and the floater hides.
///
/// Returns `None` when no listed app is frontmost or AX permission is missing.
pub fn focused_window_among(app_pids: &[i32]) -> Option<FocusedWindow> {
    for &pid in app_pids {
        unsafe {
            let app = AXUIElementCreateApplication(pid as libc::pid_t);
            if app.is_null() {
                continue;
            }
            let app = CfOwned(app as CFTypeRef);
            let app_ref = app.0 as AXUIElementRef;

            // Never let a wedged app stall the tracker loop.
            AXUIElementSetMessagingTimeout(app_ref, 0.25);

            if attr_bool(app_ref, kAXFrontmostAttribute) != Some(true) {
                continue;
            }

            let Some(window) = copy_attr(app_ref, kAXFocusedWindowAttribute) else {
                continue;
            };
            let window_ref = window.0 as AXUIElementRef;

            let title = attr_string(window_ref, kAXTitleAttribute).unwrap_or_default();
            let pos = attr_point(window_ref, kAXPositionAttribute).unwrap_or_default();
            let size = attr_size(window_ref, kAXSizeAttribute).unwrap_or_default();

            return Some(FocusedWindow {
                pid,
                title,
                x: pos.x,
                y: pos.y,
                w: size.width,
                h: size.height,
            });
        }
    }
    None
}
