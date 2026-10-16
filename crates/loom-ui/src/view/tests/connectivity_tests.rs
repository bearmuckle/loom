//! Worker connectivity failures: plain-language classification, the single
//! reconnecting state, its automatic retry schedule, and the controls the
//! status banner offers for it.

use super::*;
use crate::view::connectivity::{
    CONNECTION_LOST_COPY, ConnectivityFailure, Reconnecting, RetrySchedule,
    classify_connectivity_failure, error_details,
};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};

/// The reason the browser transport reports when the socket closes, which is
/// what a killed worker produces mid-request.
const CLOSE_REASON: &str = "the connection closed (code 1006)";

fn closed_connection() -> LoomError {
    LoomError::new(ErrorCode::RequestCancelled, CLOSE_REASON, true)
}

#[gpui_kit::test]
fn transport_failures_are_classified_apart_from_operation_errors(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|_| {
        assert_eq!(
            classify_connectivity_failure(&closed_connection()),
            Some(ConnectivityFailure::ConnectionLost)
        );
        assert_eq!(
            classify_connectivity_failure(&LoomError::new(
                ErrorCode::RequestCancelled,
                "no worker is connected",
                true,
            )),
            Some(ConnectivityFailure::ConnectionLost)
        );
        assert_eq!(
            classify_connectivity_failure(&LoomError::new(
                ErrorCode::DeadlineExceeded,
                "worker did not answer within 60s; it may be stalled or unreachable",
                true,
            )),
            Some(ConnectivityFailure::TimedOut)
        );
        assert_eq!(
            classify_connectivity_failure(&LoomError::new(
                ErrorCode::ProviderUnavailable,
                "connection refused",
                true,
            )),
            Some(ConnectivityFailure::Unreachable)
        );

        // A cancellation the user asked for is not a lost worker, and neither
        // is an operation error or a client-side routing error: those keep the
        // coded banner.
        for error in [
            LoomError::new(
                ErrorCode::RequestCancelled,
                "run interrupted by the user",
                false,
            ),
            LoomError::invalid_state("model availability has not been refreshed"),
            LoomError::new(
                ErrorCode::NotFound,
                "worker node test-node is not connected",
                false,
            ),
            LoomError::new(
                ErrorCode::ProviderUnavailable,
                "the provider returned 503",
                true,
            ),
            LoomError::new(ErrorCode::Persistence, "the transcript page failed", false),
        ] {
            assert_eq!(
                classify_connectivity_failure(&error),
                None,
                "{} should stay an operation error",
                error.code
            );
        }
    });
}

#[gpui_kit::test]
fn connectivity_copy_is_plain_language_and_keeps_the_raw_detail(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|_| {
        let error = closed_connection();
        let failure = ConnectivityFailure::ConnectionLost;
        assert_eq!(failure.headline(), "Worker connection lost");
        let detail = failure.detail(&error);
        assert!(detail.starts_with(CONNECTION_LOST_COPY));
        for raw in ["request_cancelled", "1006", "(code", "closed"] {
            assert!(
                !detail.contains(raw),
                "the headline copy still carries {raw}: {detail}"
            );
        }

        // The other shapes reuse the worker-connection vocabulary, which is
        // already plain language.
        assert_eq!(
            ConnectivityFailure::TimedOut.headline(),
            "Worker is not responding"
        );
        assert!(
            ConnectivityFailure::TimedOut
                .detail(&LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    "worker did not answer within 60s",
                    true,
                ))
                .contains("timed out")
        );
        assert_eq!(
            ConnectivityFailure::Unreachable.headline(),
            "Worker is unreachable"
        );
        assert!(
            ConnectivityFailure::Unreachable
                .detail(&LoomError::new(
                    ErrorCode::ProviderUnavailable,
                    "connection refused",
                    true,
                ))
                .contains("refused the connection")
        );

        // The raw code and message stay available for the details affordance.
        assert_eq!(
            error_details(&error),
            "request_cancelled: the connection closed (code 1006)"
        );
    });
}

#[gpui_kit::test]
fn reconnect_backoff_doubles_to_a_cap_and_then_stops(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|_| {
        let mut schedule = RetrySchedule::reconnect();
        let delays = (0..schedule.max_attempts())
            .map(|_| {
                schedule
                    .take_attempt()
                    .expect("attempt within the schedule")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delays,
            vec![500, 1000, 2000, 4000, 8000, 15_000, 15_000, 15_000]
        );
        assert!(schedule.exhausted());
        assert_eq!(schedule.take_attempt(), None);

        // A retry the user asked for starts the schedule over.
        schedule.reset();
        assert!(!schedule.exhausted());
        assert_eq!(schedule.attempts(), 0);
        assert_eq!(schedule.take_attempt(), Some(500));
        assert_eq!(schedule.attempts(), 1);
        assert_eq!(schedule.next_delay_millis(), 1_000);
    });
}

#[gpui_kit::test]
fn reconnecting_state_keeps_one_message_with_a_moving_attempt_indicator(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|_| {
        let mut state =
            Reconnecting::new(ConnectivityFailure::ConnectionLost, &closed_connection());
        assert_eq!(state.failure(), ConnectivityFailure::ConnectionLost);
        assert_eq!(
            state.progress(),
            "Reconnecting automatically · attempt 1 of 8 · next attempt in about 1s"
        );
        assert_eq!(state.retry_label(), "Retry now");
        assert!(state.text().starts_with(CONNECTION_LOST_COPY));
        assert_eq!(
            state.details(),
            "request_cancelled: the connection closed (code 1006)"
        );

        assert_eq!(state.take_attempt(), Some(500));
        assert!(state.progress().contains("attempt 2 of 8"));

        // A later failure of the same kind keeps the state and its counter: one
        // state, not a stream of coded errors.
        state.update(
            ConnectivityFailure::ConnectionLost,
            &LoomError::new(
                ErrorCode::RequestCancelled,
                "the browser connection closed before answering",
                true,
            ),
        );
        assert!(state.progress().contains("attempt 2 of 8"));
        assert!(
            state
                .details()
                .contains("the browser connection closed before answering")
        );

        while state.take_attempt().is_some() {}
        assert!(state.schedule().exhausted());
        assert_eq!(
            state.progress(),
            "Automatic reconnection stopped after 8 attempts."
        );
        assert_eq!(state.retry_label(), "Try again");
        state.reset_schedule();
        assert_eq!(state.schedule().attempts(), 0);
    });
}

#[gpui_kit::test]
fn a_connectivity_failure_shows_one_plain_language_reconnecting_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());

        // What a killed worker produces mid-request: a closed transport.
        view.record_backend_error("session list refresh", closed_connection());
        let banner = view
            .status_banner
            .as_ref()
            .expect("a reconnecting notice is shown");
        assert_eq!(banner.tone, SystemTone::Error);
        assert_eq!(banner.heading.as_deref(), Some("Worker connection lost"));
        assert!(banner.text.starts_with(CONNECTION_LOST_COPY));
        assert!(
            !banner.text.contains("1006") && !banner.text.contains("request_cancelled"),
            "the notice must not show the raw transport text: {}",
            banner.text
        );
        assert_eq!(
            banner.details.as_deref(),
            Some("request_cancelled: the connection closed (code 1006)")
        );
        assert!(
            banner
                .progress
                .as_deref()
                .is_some_and(|progress| progress.contains("attempt 1 of 8"))
        );
        // The manual path is a control, not a sentence.
        assert!(banner.retryable);
        assert!(
            view.reconnect_progress()
                .is_some_and(|progress| progress.contains("attempt 1 of 8"))
        );

        // Repeated failures while the worker stays down update that one state
        // instead of replacing it with another coded message.
        view.record_backend_error(
            "session event stream",
            LoomError::new(
                ErrorCode::DeadlineExceeded,
                "worker request timed out",
                true,
            ),
        );
        let banner = view
            .status_banner
            .as_ref()
            .expect("the notice is still shown");
        assert_eq!(banner.heading.as_deref(), Some("Worker is not responding"));
        let note_count = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::System(_)))
            .count();
        assert_eq!(note_count, 0, "the notice stays out of the timeline");
        assert_eq!(
            view.reconnecting
                .as_ref()
                .map(Reconnecting::schedule)
                .map(|schedule| schedule.attempts()),
            Some(0),
            "a repeat failure does not restart the attempt counter"
        );

        // The automatic schedule advances one attempt at a time.
        view.schedule_connectivity_retry(cx);
        assert_eq!(
            view.reconnecting
                .as_ref()
                .map(Reconnecting::schedule)
                .map(|schedule| schedule.attempts()),
            Some(1)
        );
        // A second call while one attempt is waiting on its timer does not
        // stack a second attempt.
        view.schedule_connectivity_retry(cx);
        assert_eq!(
            view.reconnecting
                .as_ref()
                .map(Reconnecting::schedule)
                .map(|schedule| schedule.attempts()),
            Some(1)
        );

        // A retry the user asks for runs an attempt immediately and restarts
        // the schedule, so a spent one can run again.
        if let Some(state) = view.reconnecting.as_mut() {
            state.reset_schedule();
        }
        view.retry_status_action(cx);
        assert_eq!(
            view.reconnecting
                .as_ref()
                .map(Reconnecting::schedule)
                .map(|schedule| schedule.attempts()),
            Some(1)
        );

        // An answer from the worker ends the state and says so.
        view.note_worker_answered(cx);
        assert!(view.reconnecting.is_none());
        assert!(view.reconnect_progress().is_none());
        assert!(
            view.status_banner
                .as_ref()
                .is_some_and(|note| note.text == "Worker connection restored")
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn an_operation_error_keeps_the_coded_banner_and_promises_no_retry(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.record_backend_error(
            "session rename",
            LoomError::invalid_state("the session name is empty"),
        );
        let banner = view.status_banner.as_ref().expect("the error is shown");
        assert_eq!(
            banner.heading.as_deref(),
            Some("session rename · invalid_state")
        );
        assert_eq!(banner.text, "the session name is empty");
        assert_eq!(
            banner.details.as_deref(),
            Some("invalid_state: the session name is empty")
        );
        assert!(!banner.retryable);
        assert!(banner.progress.is_none());
        assert!(view.reconnecting.is_none());
        assert_eq!(view.poll_delay_millis(250), 250);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn polls_back_off_while_the_worker_is_unreachable(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.record_backend_error("session event stream", closed_connection());
        // A transient failure keeps the poll's own cadence until the backoff
        // passes it, and then the poll waits the reconnect delay.
        assert_eq!(view.poll_delay_millis(250), 500);
        assert_eq!(view.poll_delay_millis(1_000), 1_000);

        // Each attempt the schedule consumes pushes the next poll's wait out,
        // and a second call while one attempt is waiting on its timer adds
        // nothing.
        view.schedule_connectivity_retry(cx);
        view.schedule_connectivity_retry(cx);
        assert_eq!(view.poll_delay_millis(250), 1_000);
        view.connectivity_attempt_in_flight = false;
        view.schedule_connectivity_retry(cx);
        assert_eq!(view.poll_delay_millis(250), 2_000);

        // Dismissing the notice stops the automatic attempts.
        view.dismiss_status_banner(cx);
        assert!(view.status_banner.is_none());
        assert!(view.reconnecting.is_none());
        assert_eq!(view.poll_delay_millis(250), 250);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn the_reconnecting_banner_offers_working_details_and_retry_controls(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        // The empty-session overlay would cover the composer.
        view.sessions = vec![view.active_session.clone()];
        view.record_backend_error("session list refresh", closed_connection());
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        let banner = window.find("status-banner");
        assert!(banner.visible(), "the reconnecting notice is on screen");
        assert!(window.find("status-banner-details").visible());
        assert!(window.find("status-banner-copy-details").visible());
        assert!(
            window.find("status-banner-retry").visible(),
            "the notice must offer a real retry control"
        );
        assert_eq!(
            window.find("status-banner-details").label(),
            Some("Details")
        );
        assert_eq!(
            window.find("status-banner-retry").label(),
            Some("Retry now")
        );

        // The control behind the sentence that used to be there.
        window.click("status-banner-details", cx);
        window.click("status-banner-retry", cx);
    })
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = window
            .root::<LoomView>()
            .flatten()
            .expect("the view is mounted");
        view.read(cx)
            .status_banner
            .as_ref()
            .expect("the notice is still shown")
            .details
            .as_deref()
            .expect("the raw detail is kept");
        assert!(
            view.read(cx).status_details_open,
            "the details control expands the raw text"
        );
        assert_eq!(
            window.find("status-banner-details").label(),
            Some("Hide details")
        );
        // Leave no automatic attempt running behind the test.
        view.update(cx, |view, cx| view.dismiss_status_banner(cx));
    })
    .unwrap()
}
