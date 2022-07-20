// GNU AGPL v3 License

use tui::{
    style::{Modifier, Style},
    text::{Span, Spans},
    widgets::{Paragraph, StatefulWidget, Widget},
};

/// Rudimentary textbox widget.
#[derive(Default)]
pub(super) struct TextBox {
    header: Option<String>,
}

impl TextBox {
    pub(super) fn header(mut self, header: impl Into<String>) -> Self {
        self.header = Some(header.into());
        self
    }
}

/// State for a textbox.
#[derive(Default)]
pub(super) struct TextBoxState {
    /// The text to display.
    text: String,
    /// Whether or not the textbox is focused.
    focused: bool,
}

impl TextBoxState {
    pub(super) fn new(text: String) -> Self {
        Self {
            text,
            focused: false,
        }
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn push(&mut self, c: char) {
        self.text.push(c);
    }

    pub(super) fn pop(&mut self) {
        self.text.pop();
    }

    pub(super) fn focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub(super) fn focused(&self) -> bool {
        self.focused
    }
}

impl StatefulWidget for TextBox {
    type State = TextBoxState;

    fn render(
        self,
        area: tui::layout::Rect,
        buf: &mut tui::buffer::Buffer,
        state: &mut Self::State,
    ) {
        let spans = self
            .header
            .map(|hdr| {
                let style = Style::default().add_modifier(Modifier::BOLD);
                [Span::styled(hdr, style), Span::styled(": ", style)]
            })
            .into_iter()
            .flatten()
            .chain(Some(Span::from(state.text())))
            .chain(if state.focused {
                Some(Span::from("_"))
            } else {
                None
            })
            .collect::<Vec<_>>();

        Paragraph::new(Spans::from(spans)).render(area, buf)
    }
}
