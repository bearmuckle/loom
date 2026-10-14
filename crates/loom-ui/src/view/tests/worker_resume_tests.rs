//! Tests for what the browser client does when the page becomes visible again.

use super::{BrowserResumption, browser_resumption};

#[test]
fn a_demo_page_never_resumes_a_worker() {
    assert_eq!(
        browser_resumption(true, false, false, false),
        BrowserResumption::Ignore
    );
    assert_eq!(
        browser_resumption(true, false, true, true),
        BrowserResumption::Ignore
    );
    assert_eq!(
        browser_resumption(true, true, false, true),
        BrowserResumption::Ignore
    );
}

#[test]
fn a_connected_page_verifies_the_connection_it_still_trusts() {
    assert_eq!(
        browser_resumption(false, true, false, true),
        BrowserResumption::Verify
    );
    assert_eq!(
        browser_resumption(false, true, false, false),
        BrowserResumption::Verify
    );
    assert_eq!(
        browser_resumption(false, true, true, true),
        BrowserResumption::Verify
    );
}

#[test]
fn a_recorded_loss_reconnects_even_without_saved_settings() {
    assert_eq!(
        browser_resumption(false, false, true, false),
        BrowserResumption::Reconnect
    );
    assert_eq!(
        browser_resumption(false, false, true, true),
        BrowserResumption::Reconnect
    );
}

#[test]
fn saved_settings_without_a_connection_reconnect() {
    assert_eq!(
        browser_resumption(false, false, false, true),
        BrowserResumption::Reconnect
    );
}

#[test]
fn an_unconfigured_page_without_a_recorded_loss_is_left_alone() {
    assert_eq!(
        browser_resumption(false, false, false, false),
        BrowserResumption::Ignore
    );
}
