// GNU AGPL v3 License

use super::UiDirective;
use sharded_slab::Slab;
use std::{
    mem,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tracing::{field::Visit, span, Id, Level, Subscriber};
use tui::{
    style::{Color, Modifier, Style},
    text::{Span, Spans},
};

/// A subscriber that listens for events and records them for use in the UI.
pub(crate) struct UiSubscriber {
    inner: Arc<Mutex<Inner>>,
    notify: mpsc::Sender<UiDirective>,
}

pub(super) struct Inner {
    pub(super) lines: Vec<Spans<'static>>,
}

impl UiSubscriber {
    pub(crate) fn new(notify: mpsc::Sender<UiDirective>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner { lines: Vec::new() })),
            notify,
        }
    }

    pub(super) fn inner(&self) -> &Arc<Mutex<Inner>> {
        &self.inner
    }
}

impl Subscriber for UiSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &span::Attributes<'_>) -> span::Id {
        Id::from_u64(1)
    }

    fn record(&self, span: &span::Id, values: &span::Record<'_>) {}

    fn record_follows_from(&self, span: &span::Id, follows: &span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let md = event.metadata();

        let mut spans = Vec::with_capacity(2);

        // add spans for each part
        let level_span = match *md.level() {
            Level::ERROR => Span::styled(
                "ERROR ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Level::WARN => Span::styled(
                "WARN  ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Level::INFO => Span::styled("INFO  ", Style::default().fg(Color::Green)),
            Level::DEBUG => Span::styled("DEBUG ", Style::default().fg(Color::Cyan)),
            Level::TRACE => Span::styled("TRACE ", Style::default().fg(Color::Blue)),
            _ => unreachable!(),
        };

        // add a span for the event bod
        let mut buffer = String::new();
        let mut visitor = BufferVisitor {
            buffer: &mut buffer,
        };
        event.record(&mut visitor);

        spans.push(level_span);
        spans.push(Span::from(buffer));

        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.lines.push(Spans(spans));
        mem::drop(inner);

        // send a notification to the UI to refresh
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let notify = self.notify.clone();
            handle.spawn(async move {
                notify.send(UiDirective::Refresh).await.ok();
            });
        }
    }

    fn enter(&self, span: &span::Id) {}

    fn exit(&self, span: &span::Id) {}
}

struct BufferVisitor<'a> {
    buffer: &'a mut String,
}

impl<'a> Visit for BufferVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.buffer.push_str(&format!("{:?} ", value));
    }
}
