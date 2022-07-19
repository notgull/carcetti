// GNU AGPL v3 License

use anyhow::{anyhow, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io::{self, Write},
    process,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt;
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

mod subscriber;

/// Run the UI thread.
///
/// Communicates with the rest of the program via a channel.
pub(crate) async fn ui_thread(
    send_data: mpsc::Receiver<UiDirective>,
    sender: mpsc::Sender<UiDirective>,
    ui_data: broadcast::Sender<UiMessage>,
) -> Result<()> {
    // establish our backend
    let mut terminal = tokio::task::spawn_blocking(|| {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        anyhow::Ok(terminal)
    })
    .await??;

    let mut cleanup = Cleanup {
        terminal: &mut terminal,
        cleaned: false,
    };

    run_ui(cleanup.terminal, send_data, sender, ui_data).await?;

    // restore the terminal that was there before
    tokio::task::block_in_place(move || cleanup.cleanup())?;

    Ok(())
}

struct Cleanup<'a, W: Write> {
    terminal: &'a mut Terminal<CrosstermBackend<W>>,
    cleaned: bool,
}

impl<'a, W: Write> Cleanup<'a, W> {
    fn cleanup(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;
        self.cleaned = true;

        anyhow::Ok(())
    }
}

impl<'a, W: Write> Drop for Cleanup<'a, W> {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

async fn run_ui<B: Backend + Send + 'static>(
    terminal: &mut Terminal<B>,
    mut send_data: mpsc::Receiver<UiDirective>,
    sender: mpsc::Sender<UiDirective>,
    ui_data: broadcast::Sender<UiMessage>,
) -> Result<()> {
    // set up an event stream
    let mut event_stream = EventStream::new();
    let last_directive = UiDirective::DisplayText("Loading...".to_string());

    // open up a tracing channel
    let sub = subscriber::UiSubscriber::new(sender);
    let span_events = sub.inner().clone();
    tracing::subscriber::set_global_default(sub)?;

    // state for the UI drawing
    let mut state = DrawState {
        last_directive,
        tracing_events: span_events,
    };

    let mut running = true;
    loop {
        // block in place while we draw the UI
        tokio::task::block_in_place(|| terminal.draw(|frame| draw_ui(frame, &mut state)))?;

        // wait for an event from either the UI or the channel from the main program
        tokio::select! {
            directive = send_data.recv() => {
                match directive {
                    Some(UiDirective::Stop) | None => {
                        running = false;
                        state.last_directive = UiDirective::Stop;
                    },
                    Some(UiDirective::Refresh) => {},
                    Some(dir) => if running { state.last_directive = dir; },
                }
            },
            event = event_stream.next() => {
                let event = event.unwrap()?;

                // TODO: handle keyboard events
                //tracing::debug!("{:?}", event);
                if let Event::Key(key) = event {
                    if !running {
                        return Ok(());
                    }

                    // if the user hits ctrl-c, stop the program
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        ui_data.send(UiMessage::Halt).ok();

                        // spawn a task that exits the process after 5 seconds if something
                        // is hanging
                        tokio::spawn(async {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            process::exit(1);
                        });

                        return Err(anyhow!("User requested exit"));
                    }
                }
            }
        }
    }
}

fn draw_ui(frame: &mut Frame<'_, impl Backend>, state: &mut DrawState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([Constraint::Percentage(80), Constraint::Percentage(20)].as_ref())
        .split(frame.size());

    let mut do_string = |s: &str| {
        let spans = Spans::from(Span::styled(
            s,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let p = Paragraph::new(spans)
            .alignment(Alignment::Left)
            .block(Block::default().borders(Borders::ALL).title("Message"));
        frame.render_widget(p, chunks[0]);
    };

    match &state.last_directive {
        UiDirective::Stop => {
            do_string("Press any key to continue...");
        }
        UiDirective::DisplayText(text) => {
            do_string(text);
        }
        UiDirective::Refresh => {}
    }

    let height = chunks[1].height - 2;
    let text = &state.tracing_events.lock().unwrap().lines;
    let text = text
        .iter()
        .cloned()
        .rev()
        .take(height as usize)
        .rev()
        .collect::<Vec<_>>();
    let p = Paragraph::new(text)
        .alignment(Alignment::Left)
        .block(Block::default().borders(Borders::ALL).title("Tracing"));
    frame.render_widget(p, chunks[1]);
}

struct DrawState {
    last_directive: UiDirective,
    tracing_events: Arc<Mutex<subscriber::Inner>>,
}

#[derive(Debug)]
pub(crate) enum UiDirective {
    /// Request the UI to display some text.
    DisplayText(String),
    /// Request the UI to stop gracefully.
    Stop,
    /// We just need to refresh.
    Refresh,
}

#[derive(Debug, Clone)]
pub(crate) enum UiMessage {
    /// Halt the program as gracefully as possible.
    Halt,
}
