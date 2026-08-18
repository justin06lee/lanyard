//! Native notifications via UNUserNotificationCenter.
//!
//! macOS 26 finally dropped the NSUserNotification API (deprecated since
//! 10.14) that the usual notification crates still post through — their
//! notifications are silently discarded, and the app never even appears in
//! System Settings › Notifications. This module talks to the modern center
//! directly: one authorization request at launch, then fire-and-forget posts.
//!
//! UNUserNotificationCenter aborts the process when there is no app bundle,
//! so everything here no-ops for unbundled runs (`cargo run`, tests).

use std::sync::atomic::{AtomicU64, Ordering};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNUserNotificationCenter,
};

fn bundled() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// Asks for notification permission. macOS shows the consent prompt once;
/// afterwards this silently reflects whatever the user decided.
pub fn init() {
    if !bundled() {
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let options = UNAuthorizationOptions::Alert
        | UNAuthorizationOptions::Sound
        | UNAuthorizationOptions::Badge;
    let done = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
    center.requestAuthorizationWithOptions_completionHandler(options, &done);
}

/// Posts a banner. Delivery is the system's business: not granted, Focus on,
/// or banners off in settings all fail silently, exactly like any other app.
pub fn post(title: &str, body: &str) {
    if !bundled() {
        return;
    }
    // Unique per post, or a later banner would replace an earlier one.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = format!("lanyard-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&id),
        &content,
        None,
    );
    let done = RcBlock::new(|_error: *mut NSError| {});
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&done));
}
