use super::*;

impl LoomView {
    pub(crate) fn record_status(&mut self, status: impl Into<String>) {
        self.timeline
            .push(TimelineItem::System(SystemNote::status(status.into())));
    }

    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.timeline.push(TimelineItem::System(SystemNote {
            tone: SystemTone::Error,
            heading: Some(format!("{operation} · {}", error.code)),
            text: error.message.clone(),
            retryable: error.retryable,
        }));
    }
}
