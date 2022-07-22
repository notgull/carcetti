// GNU AGPL v3 License

use anyhow::{anyhow, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    cmp,
    collections::HashSet,
    fmt,
    io::{self, Write},
    iter::repeat,
    process,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_stream::StreamExt;
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};

use crate::{
    video::Clip,
    VideoConfig,
};
use textbox::{TextBox, TextBoxState};

mod subscriber;
mod textbox;

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
        table_state: None,
        video_config: None,
        tracing_toggle: false,
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
                    } else if key.code == KeyCode::Up {
                        state.on_up();
                    } else if key.code == KeyCode::Down {
                        state.on_down();
                    } else if key.code == KeyCode::Enter {
                        if let Some(msg) = state.on_enter()? {
                            ui_data.send(msg)?;
                        }
                    } else if key.code == KeyCode::Esc && state.on_esc() {
                        ui_data.send(UiMessage::Halt).ok();
                        return Ok(());
                    } else if let KeyCode::Char(c) = key.code {
                        if let Some(msg) = state.on_char(c) {
                            ui_data.send(msg)?;
                        }
                    } else if key.code == KeyCode::Backspace {
                        state.on_backspace();
                    }
                }
            }
        }
    }
}

fn draw_ui(frame: &mut Frame<'_, impl Backend>, state: &mut DrawState) {
    let ctrs = if state.tracing_toggle {
        [Constraint::Percentage(5), Constraint::Percentage(95)]
    } else {
        [Constraint::Percentage(80), Constraint::Percentage(20)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints(&ctrs)
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

    match &mut state.last_directive {
        UiDirective::Stop => {
            do_string("Press any key to continue...");
        }
        UiDirective::DisplayText(text) => {
            do_string(text);
        }
        UiDirective::Refresh => panic!("Directive should never be refresh"),
        UiDirective::Clips { clips, indices, .. } => {
            let table_state = state.table_state.get_or_insert_with(|| {
                let mut ts = TableState::default();
                ts.select(Some(0));
                ts
            });

            let table_layout = Layout::default()
                .direction(Direction::Vertical)
                .margin(0)
                .constraints([Constraint::Length(5), Constraint::Max(9999)])
                .split(chunks[0]);

            // populate with clips
            let mut total_time = 0;
            let indices = indices.blocking_read();
            let rows = clips
                .iter()
                .enumerate()
                .map(|(index, clip)| {
                    let is_active = indices.contains(&index);
                    if is_active {
                        total_time += clip.len();
                    }

                    Row::new([
                        Cell::from(format!(
                            "{} - {}",
                            Timestamp(clip.start),
                            Timestamp(clip.end)
                        )),
                        Cell::from(Timestamp(clip.len()).to_string()),
                        if is_active {
                            Cell::from("ACTIVE").style(Style::default().fg(Color::Green))
                        } else {
                            Cell::from("INACTIVE").style(Style::default().fg(Color::Red))
                        },
                    ])
                })
                .collect::<Vec<_>>();

            let instructions = Paragraph::new(vec![
                Spans::from("Select clips to produce a video"),
                Spans::from(
                    "Enter to change status, C to reconfigure, Spacebar to continue, Esc to exit",
                ),
                Spans::from(format!("Current time: {}", Timestamp(total_time))),
            ])
            .block(Block::default().borders(Borders::ALL).title("Instructions"));

            let table = Table::new(rows)
                .widths(&[
                    Constraint::Length(32),
                    Constraint::Length(15),
                    Constraint::Length(10),
                ])
                .column_spacing(1)
                .block(Block::default().title("Clips").borders(Borders::ALL))
                .header(
                    Row::new(["Range", "Duration", "Status"]).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                );

            frame.render_widget(instructions, table_layout[0]);
            frame.render_stateful_widget(table, table_layout[1], table_state);
        }
        UiDirective::ModifyVideoConfig(ref mut config) => {
            // create the config state if it doesn't exist already
            let cfg_state = state.video_config.get_or_insert_with(|| {
                let time_to_float_seconds = |time| (time as f64) / 1_000_000.0;

                let mut buffer = ryu::Buffer::new();
                let mut sil_th =
                    TextBoxState::new(buffer.format(config.silence_threshold).to_string());
                let sil_ti = TextBoxState::new(
                    buffer
                        .format(time_to_float_seconds(config.silence_time))
                        .to_string(),
                );
                let sil_deg = TextBoxState::new(buffer.format(config.silence_degrade).to_string());
                let tim_lim = TextBoxState::new(
                    buffer
                        .format(time_to_float_seconds(config.time_limit))
                        .to_string(),
                );
                sil_th.focus(true);

                VideoConfigState {
                    textboxes: [sil_th, sil_ti, sil_deg, tim_lim],
                    focused: 0,
                }
            });

            let bl = Block::default()
                .borders(Borders::ALL)
                .title("Configuration");
            frame.render_widget(bl, chunks[0]);

            let inner_rect = Rect {
                x: chunks[0].x + 1,
                y: chunks[0].y + 1,
                width: chunks[0].width - 2,
                height: chunks[0].height - 2,
            };

            let tb_layout = Layout::default()
                .direction(Direction::Vertical)
                .margin(3)
                .constraints(
                    repeat(Constraint::Percentage(20))
                        .take(5)
                        .collect::<Vec<_>>(),
                )
                .split(inner_rect);

            for (i, header) in [
                "Silence Threshold",
                "Silence Time (seconds)",
                "Silence Degradation",
                "Time Limit (seconds)",
            ]
            .iter()
            .enumerate()
            {
                let tb = TextBox::default().header(*header);

                frame.render_stateful_widget(tb, tb_layout[i], &mut cfg_state.textboxes[i]);
            }

            // render instructions as well
            let inst = Paragraph::new(
                "Press Enter to apply changes, up/down to change focus, Esc to exit",
            )
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::ITALIC),
            );
            frame.render_widget(inst, tb_layout[4]);
        }
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
    table_state: Option<TableState>,
    video_config: Option<VideoConfigState>,
    tracing_toggle: bool,
}

struct VideoConfigState {
    textboxes: [TextBoxState; 4],
    focused: usize,
}

impl DrawState {
    /// The "up" key was pressed.
    fn on_up(&mut self) {
        if let (UiDirective::Clips { .. }, Some(ts)) =
            (&self.last_directive, self.table_state.as_mut())
        {
            ts.select(ts.selected().map(|s| s.saturating_sub(1)));
        } else if let (UiDirective::ModifyVideoConfig(..), Some(vc)) =
            (&self.last_directive, self.video_config.as_mut())
        {
            vc.textboxes[vc.focused].focus(false);
            vc.focused = vc.focused.saturating_sub(1);
            vc.textboxes[vc.focused].focus(true);
        }
    }

    /// The "down" key was pressed.
    fn on_down(&mut self) {
        if let (UiDirective::Clips { clips, .. }, Some(ts)) =
            (&self.last_directive, self.table_state.as_mut())
        {
            ts.select(ts.selected().map(|s| {
                let new_s = s.saturating_add(1);
                cmp::min(new_s, clips.len() - 1)
            }));
        } else if let (UiDirective::ModifyVideoConfig(..), Some(vc)) =
            (&self.last_directive, self.video_config.as_mut())
        {
            vc.textboxes[vc.focused].focus(false);
            vc.focused = vc.focused.saturating_add(1);
            vc.focused = cmp::min(vc.focused, vc.textboxes.len() - 1);
            vc.textboxes[vc.focused].focus(true);
        }
    }

    /// The "enter" key was pressed.
    fn on_enter(&mut self) -> Result<Option<UiMessage>> {
        if let (UiDirective::Clips { indices, .. }, Some(ts)) =
            (&self.last_directive, self.table_state.as_mut())
        {
            let selected = ts.selected().unwrap();
            let mut indices = indices.blocking_write();
            if indices.contains(&selected) {
                indices.remove(&selected);
            } else {
                indices.insert(selected);
            }
        } else if let (UiDirective::ModifyVideoConfig(..), Some(vc)) =
            (&self.last_directive, self.video_config.as_mut())
        {
            let float_seconds_to_time = |f| (f * 1_000_000.0) as i64;

            // parse the values as floats
            let mut values = [0.0; 4];
            for (i, tb) in vc.textboxes.iter_mut().enumerate() {
                let text = tb.text();
                let value = text
                    .parse::<f64>()
                    .map_err(|_| anyhow!("could not parse {} as a float", text))?;
                values[i] = value;
            }
            let config = VideoConfig {
                silence_threshold: values[0],
                silence_time: float_seconds_to_time(values[1]),
                silence_degrade: values[2],
                time_limit: float_seconds_to_time(values[3]),
            };

            return Ok(Some(UiMessage::RerunVideoProcess(config)));
        }

        Ok(None)
    }

    /// The "esc" key was pressed.
    ///
    /// Returns true if the program should exit.
    fn on_esc(&mut self) -> bool {
        if let UiDirective::Clips { .. } | UiDirective::ModifyVideoConfig(..) = &self.last_directive
        {
            return true;
        }

        false
    }

    /// A character was pressed.
    ///
    /// If a UI message is needed, it returns it.
    fn on_char(&mut self, c: char) -> Option<UiMessage> {
        match c {
            ' ' => match self.last_directive {
                UiDirective::Clips { .. } => Some(UiMessage::GoForIt),
                _ => None,
            },
            'c' => match self.last_directive {
                UiDirective::Clips { ref config, .. } => {
                    self.last_directive = UiDirective::ModifyVideoConfig(config.clone());
                    None
                }
                UiDirective::ModifyVideoConfig(..) => {
                    self.video_config.as_mut().unwrap().on_char('c')
                }
                _ => None,
            },
            't' => {
                self.tracing_toggle = !self.tracing_toggle;
                None
            }
            c => match self.last_directive {
                UiDirective::ModifyVideoConfig(_) => self.video_config.as_mut().unwrap().on_char(c),
                _ => None,
            },
        }
    }

    /// Backspace was pressed.
    fn on_backspace(&mut self) {
        if let UiDirective::ModifyVideoConfig(_) = &self.last_directive {
            self.video_config.as_mut().unwrap().on_backspace();
        }
    }
}

impl VideoConfigState {
    fn on_char(&mut self, c: char) -> Option<UiMessage> {
        match c {
            '\n' => {
                // ignore it
            }
            _ => {
                if let Some(textbox) = self.textboxes.iter_mut().find(|tb| tb.focused()) {
                    textbox.push(c);
                }
            }
        }
        None
    }

    fn on_backspace(&mut self) {
        if let Some(textbox) = self.textboxes.iter_mut().find(|tb| tb.focused()) {
            textbox.pop();
        }
    }
}

#[derive(Debug)]
pub(crate) enum UiDirective {
    /// Request the UI to display some text.
    DisplayText(String),
    /// Request the UI to stop gracefully.
    Stop,
    /// We just need to refresh.
    Refresh,
    /// Request the UI display the screen containing
    /// the clips and valid clips.
    Clips {
        clips: Arc<[Clip]>,
        indices: Arc<RwLock<HashSet<usize>>>,
        config: VideoConfig,
    },
    /// Edit and modify the video config.
    ModifyVideoConfig(VideoConfig),
}

#[derive(Debug, Clone)]
pub(crate) enum UiMessage {
    /// Halt the program as gracefully as possible.
    Halt,
    /// Rerun the video processing algorithm with the given configuration.
    RerunVideoProcess(VideoConfig),
    /// Begin processing the video with the given config.
    GoForIt,
}

/// Internal value is in microseconds.
struct Timestamp(i64);

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // value is in microseconds
        let mut value = self.0;
        let _microseconds = value % 1000;
        value /= 1000;
        let milliseconds = value % 1000;
        value /= 1000;
        let seconds = value % 60;
        value /= 60;
        let minutes = value % 60;
        value /= 60;
        let hours = value;

        write!(
            f,
            "{:02}:{:02}:{:02}.{:03}",
            hours, minutes, seconds, milliseconds
        )
    }
}
