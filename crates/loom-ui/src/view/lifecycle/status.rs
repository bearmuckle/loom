use super::*;

impl LoomView {
    pub(crate) fn record_status(&mut self, status: impl Into<String>) {
        self.status_banner = Some(SystemNote::status(status.into()));
    }

    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        self.status_banner = Some(SystemNote {
            tone: SystemTone::Error,
            heading: Some(format!("{operation} · {}", error.code)),
            text: error.message.clone(),
            retryable: error.retryable,
        });
    }

    pub(crate) fn dismiss_status_banner(&mut self, cx: &mut Context<Self>) {
        self.status_banner = None;
        cx.notify();
    }
}
