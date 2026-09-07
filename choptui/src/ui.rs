//! The display: what it holds, how it draws, and the thread that runs it.
//!
//! Drawing happens here and nowhere else. The thread collecting data sends
//! updates and never touches the terminal, so the two run at their own rates:
//! a rig sampling at 1 kHz does not ask for a thousand redraws, and a redraw
//! that takes a moment does not delay a write to disk.

use crate::monitor::Update;
use lumberdaq::plot_config::{ self, Plot, PlotConfig };
use lumberdaq::calculated::ChannelRef;
use lumberdaq::config::DaqConfig;
use lumberdaq::datapoint::DataPoint;
use ratatui::crossterm::event::{ self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers };
use ratatui::layout::{ Alignment, Constraint, Layout, Rect };
use ratatui::style::{ Color, Modifier, Style };
use ratatui::text::{ Line, Span };
use ratatui::symbols::Marker;
use ratatui::widgets::{ Axis, Block, Chart, Dataset, GraphType, Padding, Paragraph, Row, Table };
use ratatui::Frame;
use std::collections::{ BTreeMap, VecDeque };
use std::sync::atomic::{ AtomicBool, Ordering };
use std::sync::mpsc::{ Receiver, RecvTimeoutError };
use std::time::{ Duration, Instant };

/// The longest the screen goes without being redrawn, and how often the
/// keyboard is looked at. Short enough that a key press feels immediate.
const TICK: Duration = Duration::from_millis(50);

/// How many past events are kept. Enough to scroll back through a bad patch,
/// not so many that a device reconnecting all night fills memory.
const LOG_KEPT: usize = 200;

const TABS: [&str; 4] = ["Devices", "Plots", "Log", "Settings"];

/// Space between a channel name, its reading and its count.
const GAP: u16 = 2;

/// Room for a count of readings. Six figures is an hour at 50 Hz.
const COUNT_WIDTH: u16 = 7;

/// The most of a disconnection reason to put on a device border. The whole of
/// it is in the log; a driver sentence on the border would otherwise set the
/// width of every box on the screen.
const REASON_SHOWN: usize = 28;

/// How long a plot keeps readings for, to choose between on the Settings tab.
///
/// Steps rather than a free number: the point of the setting is how far back a
/// plot goes, and a handful of round answers covers that better than nudging a
/// number one second at a time.
const HISTORY_CHOICES: [u64; 7] = [10, 30, 60, 120, 300, 600, 1800];

/// A minute, which is long enough to see a trend and short enough to see a
/// change.
const HISTORY_DEFAULT: u64 = 60;

/// The most readings a channel will hold, whatever the window asks for.
///
/// A window is a length of time, and a fast rig fills it with far more points
/// than a plot a couple of hundred columns wide can show. This is over eight
/// minutes at 100 Hz and under a minute at 1 kHz; past it the window really in
/// force is shorter than the one asked for, and the Settings tab says so rather
/// than quietly claiming otherwise.
const MAX_POINTS: usize = 50_000;

/// The rows of the Settings tab, in order.
const SETTINGS: [&str; 2] = ["Plot history", "Plot layout"];
const HISTORY_ROW: usize = 0;
const LAYOUT_ROW: usize = 1;

/// Room for the pointer showing which channel the keys act on. Always there,
/// so a row does not shift sideways when it becomes the selected one.
const POINTER_WIDTH: u16 = 2;

/// Room for a plot number in brackets.
const PLOT_WIDTH: u16 = 3;

/// Room for the legend beside a plot: a channel name and its latest reading.
const LEGEND_WIDTH: u16 = 30;

/// Series colours, in the order channels are added to a plot.
const SERIES: [Color; 6] = [
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// Borders, titles and anything else the interface is made of.
const ACCENT: Color = Color::LightMagenta;

/// Recording, and a device that is not there. Both mean look here, and being a
/// different colour from the interface around them is the whole point: a
/// failure drawn in the same red as the box it sits on was easy to miss.
const ALERT: Color = Color::LightRed;
const DIM: Color = Color::DarkGray;

pub enum Status {
    Connected,
    /// Tried and failed, or lost. The cause is kept so the screen can say why
    /// rather than only that something is wrong.
    Disconnected(Option<String>),
}

pub struct ChannelRow {
    pub name: String,
    pub unit: String,
    /// None until the first reading arrives, which is not the same as zero.
    pub latest: Option<f64>,
    pub readings: usize,
    /// Which plot this is drawn on, if any. Set from the Devices tab.
    pub plot: Option<usize>,
    /// Recent readings, oldest first, for drawing.
    ///
    /// Kept for every channel rather than only the plotted ones. It costs a
    /// few kilobytes each, and it means assigning a channel to a plot shows
    /// what it has been doing rather than an empty box that fills up slowly.
    pub history: VecDeque<DataPoint>,
}

pub struct DeviceRow {
    pub name: String,
    pub status: Status,
    pub channels: Vec<ChannelRow>,
}

pub struct State {
    pub project: String,
    pub devices: Vec<DeviceRow>,
    pub log: VecDeque<String>,
    pub tab: usize,
    /// Which channel the Devices tab is pointing at, counted across every
    /// device rather than within one.
    pub selected: usize,
    /// How far back a plot goes, in seconds. One of [`HISTORY_CHOICES`] once
    /// changed from here, but any value loads, so a layout written elsewhere is
    /// honoured as it was written.
    pub history_seconds: u64,
    /// How a graphical interface arranged these plots, if one has.
    ///
    /// Kept and written back untouched. This monitor draws plots one under
    /// another and has no use for it, but saving here must not throw away
    /// somebody's arrangement just because this program cannot show it.
    pub plot_layout: Option<lumberdaq::plot_config::PlotLayout>,
    /// Which row the Settings tab is pointing at.
    pub setting: usize,
    /// When the current recording started, and nothing when not recording.
    /// The header counts from here.
    pub recording: Option<Instant>,
}

impl State {
    /// Everything the setup says exists, before any of it has been heard from.
    ///
    /// Built from the config rather than from arriving data, so a device that
    /// never connects still appears with its channels rather than the screen
    /// simply not mentioning it.
    pub fn from_config(project: &str, config: &DaqConfig) -> State {
        State {
            project: project.to_string(),
            devices: config
                .devices
                .iter()
                .map(|device| DeviceRow {
                    name: device.info.name.clone(),
                    status: Status::Disconnected(None),
                    channels: device
                        .hardware
                        .channel_infos()
                        .into_iter()
                        .map(|info| ChannelRow {
                            name: info.name,
                            unit: info.unit,
                            latest: None,
                            readings: 0,
                            plot: None,
                            history: VecDeque::new(),
                        })
                        .collect(),
                })
                .collect(),
            log: VecDeque::new(),
            tab: 0,
            selected: 0,
            history_seconds: HISTORY_DEFAULT,
            plot_layout: None,
            setting: 0,
            recording: None,
        }
    }

    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Data { device, channel, points } => {
                let window = self.history_seconds as i64;
                if let Some(row) = self.channel_mut(&device, &channel) {
                    row.latest = points.last().map(|point| point.value);
                    row.readings += points.len();
                    row.history.extend(points);
                    trim(&mut row.history, window);
                }
            }
            Update::Connected { device } => {
                self.note(format!("{} connected", device));
                self.set_status(&device, Status::Connected);
            }
            Update::Disconnected { device, cause } => {
                self.note(match &cause {
                    Some(cause) => format!("{} disconnected: {}", device, cause),
                    None => format!("{} disconnected", device),
                });
                self.set_status(&device, Status::Disconnected(cause));
            }
            // A problem is not a disconnection: the port is fine and the device
            // keeps being read, so the status is left alone.
            Update::Problem { device, message } => self.note(format!("{}: {}", device, message)),
            Update::Said { device, line } => self.note(format!("{} said: {}", device, line)),
        }
    }

    /// Every channel in the setup, in the order they appear on screen.
    pub fn channels(&self) -> impl Iterator<Item = &ChannelRow> {
        self.devices.iter().flat_map(|device| device.channels.iter())
    }

    /// Move the pointer on the Devices tab, staying inside the list.
    pub fn move_selection(&mut self, by: isize) {
        let count = self.channels().count();
        if count == 0 {
            return;
        }
        let last = count as isize - 1;
        self.selected = (self.selected as isize + by).clamp(0, last) as usize;
    }

    /// Put the channel being pointed at on a plot, or take it off one.
    pub fn assign(&mut self, plot: Option<usize>) {
        let selected = self.selected;
        if let Some(row) = self
            .devices
            .iter_mut()
            .flat_map(|device| device.channels.iter_mut())
            .nth(selected)
        {
            row.plot = plot;
        }
    }

    /// Move the pointer on the Settings tab.
    pub fn move_setting(&mut self, by: isize) {
        let last = SETTINGS.len() as isize - 1;
        self.setting = (self.setting as isize + by).clamp(0, last) as usize;
    }

    /// Step the history window to the next choice up or down.
    ///
    /// Relative to whatever it is now rather than by an index, so a window
    /// loaded from a file that is not one of the choices still moves sensibly
    /// instead of jumping.
    pub fn adjust_history(&mut self, by: isize) {
        let now = self.history_seconds;
        self.history_seconds = match by > 0 {
            true => HISTORY_CHOICES.iter().find(|choice| **choice > now),
            false => HISTORY_CHOICES.iter().rev().find(|choice| **choice < now),
        }
        .copied()
        .unwrap_or(now);
        // Shortening it should take effect now, not once enough new readings
        // have arrived to push the old ones out.
        let window = self.history_seconds as i64;
        for device in self.devices.iter_mut() {
            for channel in device.channels.iter_mut() {
                trim(&mut channel.history, window);
            }
        }
    }

    /// The layout as it stands, ready to be written out.
    pub fn plot_config(&self) -> PlotConfig {
        let mut plots: BTreeMap<usize, Vec<ChannelRef>> = BTreeMap::new();
        for device in self.devices.iter() {
            for channel in device.channels.iter() {
                if let Some(number) = channel.plot {
                    plots.entry(number).or_default().push(ChannelRef {
                        device: device.name.clone(),
                        channel: channel.name.clone(),
                    });
                }
            }
        }
        PlotConfig {
            version: plot_config::VERSION,
            history_seconds: self.history_seconds,
            layout: self.plot_layout.clone(),
            plots: plots
                .into_iter()
                // No name: the terminal monitor identifies plots by number and
                // has nowhere to type one.
                .map(|(number, channels)| Plot { number, name: None, channels })
                .collect(),
        }
    }

    /// Put channels back on the plots a saved layout names.
    ///
    /// A channel the layout names but this setup does not have is noted and
    /// skipped. Refusing to open a display because of something that only
    /// affects how a rig is drawn would be the wrong way round, and a layout
    /// saved with a device attached is worth keeping for when it is again.
    pub fn apply_plot_config(&mut self, saved: PlotConfig) {
        self.history_seconds = saved.history_seconds;
        self.plot_layout = saved.layout;
        for plot in saved.plots.iter() {
            for reference in plot.channels.iter() {
                match self.channel_mut(&reference.device, &reference.channel) {
                    Some(row) => row.plot = Some(plot.number),
                    None => self.note(format!(
                        "{} names {}, which this setup does not have",
                        plot_config::FILE,
                        reference
                    )),
                }
            }
        }
    }

    /// The shortest span any channel is really holding, where the cap has bitten.
    fn capped_window(&self) -> Option<u64> {
        self.channels()
            .filter(|channel| channel.history.len() >= MAX_POINTS)
            .filter_map(|channel| {
                let first = channel.history.front()?.datetime;
                let last = channel.history.back()?.datetime;
                Some((last - first).num_seconds().max(0) as u64)
            })
            .min()
    }

    fn channel_mut(&mut self, device: &str, channel: &str) -> Option<&mut ChannelRow> {
        self.devices
            .iter_mut()
            .find(|row| row.name == device)?
            .channels
            .iter_mut()
            .find(|row| row.name == channel)
    }

    fn set_status(&mut self, device: &str, status: Status) {
        if let Some(row) = self.devices.iter_mut().find(|row| row.name == device) {
            row.status = status;
        }
    }

    pub fn note(&mut self, message: String) {
        if self.log.len() == LOG_KEPT {
            self.log.pop_front();
        }
        self.log.push_back(message);
    }
}

/// Drop what has fallen outside the window, oldest first.
///
/// Measured from the newest reading rather than from the clock, so the window
/// is a span of the data and not of the time the display has been open.
fn trim(history: &mut VecDeque<DataPoint>, window: i64) {
    if let Some(newest) = history.back().map(|point| point.datetime) {
        while history
            .front()
            .is_some_and(|point| (newest - point.datetime).num_seconds() > window)
        {
            history.pop_front();
        }
    }
    while history.len() > MAX_POINTS {
        history.pop_front();
    }
}

/// A length of time, the short way.
fn duration_label(seconds: u64) -> String {
    match seconds % 60 == 0 && seconds >= 60 {
        true => format!("{}m", seconds / 60),
        false => format!("{}s", seconds),
    }
}

/// Draw one frame.
///
/// Kept apart from the loop so it can be rendered into a buffer and checked
/// without a terminal to run in.
pub fn draw(frame: &mut Frame, state: &State) {
    let [header, tabs, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Min(0)])
            .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("LUMBERJACK", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", state.project), Style::new().fg(DIM)),
        ])),
        header,
    );
    frame.render_widget(
        Paragraph::new(recording_state(state)).alignment(Alignment::Right),
        header,
    );

    frame.render_widget(Paragraph::new(tab_bar(state.tab)), tabs);

    match state.tab {
        0 => devices(frame, body, state),
        1 => plots(frame, body, state),
        2 => log(frame, body, state),
        3 => settings(frame, body, state),
        _ => frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{} is not built yet.", TABS[state.tab]),
                Style::new().fg(DIM),
            ))
            .block(Block::bordered().border_style(Style::new().fg(ACCENT))),
            body,
        ),
    }
}

/// The record button and how long it has been going.
fn recording_state(state: &State) -> Line<'static> {
    match state.recording {
        Some(since) => Line::from(vec![
            Span::styled(
                "REC ",
                Style::new().fg(ALERT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(clock(since.elapsed()), Style::new().fg(Color::White)),
            Span::styled("  stop (r)", Style::new().fg(DIM)),
        ]),
        None => Line::from(vec![
            Span::styled("not recording", Style::new().fg(DIM)),
            Span::styled("  record (r)", Style::new().fg(ALERT)),
        ]),
    }
}

/// How long a recording has been going, as a clock.
fn clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

fn tab_bar(selected: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, name) in TABS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  -  ", Style::new().fg(DIM)));
        }
        spans.push(match index == selected {
            true => Span::styled(*name, Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
            false => Span::styled(*name, Style::new().fg(ACCENT)),
        });
    }
    Line::from(spans)
}

/// Where each channel sits among those sharing its plot, which is what decides
/// its colour.
///
/// Filtering keeps the order channels are in, so a plot numbers its traces the
/// same way this does, and the box on the Devices tab can be coloured to match
/// the line it produces.
fn series_positions(state: &State) -> Vec<Option<usize>> {
    let mut counted: BTreeMap<usize, usize> = BTreeMap::new();
    state
        .channels()
        .map(|channel| {
            channel.plot.map(|plot| {
                let next = counted.entry(plot).or_insert(0);
                let position = *next;
                *next += 1;
                position
            })
        })
        .collect()
}

fn devices(frame: &mut Frame, area: Rect, state: &State) {
    // Columns are sized to what is in them, and to the widest across every
    // device rather than per device, so the boxes line up with one another
    // instead of each finding its own layout.
    let channels = || state.devices.iter().flat_map(|device| device.channels.iter());
    let name_width = channels()
        .map(|channel| channel.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .clamp(8, 32);
    // A reading and its unit: room for a number, a space, and the unit.
    let value_width = channels()
        .map(|channel| channel.unit.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .clamp(1, 12)
        + 11;

    // Content, plus a border and a space of padding at each side.
    let content = POINTER_WIDTH
        + name_width
        + GAP
        + value_width
        + GAP
        + PLOT_WIDTH
        + GAP
        + COUNT_WIDTH;
    let width = state
        .devices
        .iter()
        .map(title_width)
        .max()
        .unwrap_or(0)
        .max(content + 4);

    // Left aligned at its own width rather than stretched across the terminal,
    // which on a wide screen left a gulf between a name and its reading.
    let width = width.min(area.width);

    // Placed one after another rather than by the constraint solver, which
    // when the terminal is too short shares the shortfall out among the boxes
    // until the padding has eaten every channel. A device is better shown
    // whole or not at all.
    let positions = series_positions(state);
    let mut top = area.y;
    let mut shown = 0;
    let mut first = 0;
    for device in state.devices.iter() {
        let height = device.channels.len() as u16 + 4;
        if top + height > area.bottom() {
            break;
        }
        // Where this device sits in the run of channels the pointer moves
        // through, so it can tell whether the selected one is its own.
        let pointing_at = match state.selected.checked_sub(first) {
            Some(within) if within < device.channels.len() => Some(within),
            _ => None,
        };
        render_device(
            frame,
            Rect { x: area.x, y: top, width: width, height: height },
            device,
            name_width,
            value_width,
            pointing_at,
            &positions[first..first + device.channels.len()],
        );
        first += device.channels.len();
        // A blank line between one device and the next.
        top += height + 1;
        shown += 1;
    }

    // Silently leaving devices off screen would look like a setup with fewer
    // devices in it than it has.
    // `top` has already skipped the blank line after the last box, so that
    // line is the first free one and is where this goes. If even that is past
    // the bottom there is nowhere to say it.
    if shown < state.devices.len() && top <= area.bottom() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{} more below, not enough room", state.devices.len() - shown),
                Style::new().fg(DIM),
            )),
            Rect { x: area.x, y: top - 1, width: width, height: 1 },
        );
    }
}

fn render_device(
    frame: &mut Frame,
    area: Rect,
    device: &DeviceRow,
    name_width: u16,
    value_width: u16,
    pointing_at: Option<usize>,
    positions: &[Option<usize>],
) {
    let (label, colour) = status(&device.status);

    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        // A blank line above and below the channels, and a space at each side,
        // so nothing sits against the border.
        .padding(Padding::symmetric(1, 1))
        .title(Span::styled(
            format!(" {} ", device.name),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ))
        .title_top(
            Line::from(Span::styled(format!(" {} ", label), Style::new().fg(colour)))
                .right_aligned(),
        );

    let rows = device.channels.iter().enumerate().map(|(index, channel)| {
        let selected = pointing_at == Some(index);
        let name = match selected {
            true => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            false => Style::new().fg(ACCENT),
        };
        Row::new(vec![
            // A pointer rather than a highlight, so it shows on a terminal
            // that will not colour a background.
            Span::styled(if selected { ">" } else { " " }, Style::new().fg(Color::White)),
            Span::styled(channel.name.clone(), name),
            Span::styled(
                match channel.latest {
                    Some(value) => format!("{:.3} {}", value, channel.unit),
                    // Nothing yet, which is not a reading of zero.
                    None => format!("--- {}", channel.unit),
                },
                Style::new().fg(Color::White),
            ),
            // Coloured as the trace it produces, so the two can be read
            // against each other.
            match (channel.plot, positions[index]) {
                (Some(plot), Some(position)) => Span::styled(
                    format!("[{}]", plot),
                    Style::new().fg(SERIES[position % SERIES.len()]),
                ),
                _ => Span::styled("[-]", Style::new().fg(DIM)),
            },
            Span::styled(channel.readings.to_string(), Style::new().fg(DIM)),
        ])
    });

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(POINTER_WIDTH),
                Constraint::Length(name_width),
                Constraint::Length(value_width),
                Constraint::Length(PLOT_WIDTH),
                Constraint::Length(COUNT_WIDTH),
            ],
        )
        .column_spacing(GAP)
        .block(block),
        area,
    );
}

/// What a device says on its border, and how that should look.
fn status(status: &Status) -> (String, Color) {
    match status {
        Status::Connected => ("Connected".to_string(), Color::Green),
        Status::Disconnected(None) => ("Waiting".to_string(), DIM),
        Status::Disconnected(Some(reason)) => (shortened(reason), ALERT),
    }
}

fn shortened(reason: &str) -> String {
    match reason.chars().count() > REASON_SHOWN {
        true => reason.chars().take(REASON_SHOWN - 1).collect::<String>() + "\u{2026}",
        false => reason.to_string(),
    }
}

/// How wide a device box has to be for its name and status to fit the border.
fn title_width(device: &DeviceRow) -> u16 {
    let (label, _) = status(&device.status);
    // A space each side of both, two corners, and two dashes between them.
    (device.name.chars().count() + label.chars().count() + 8) as u16
}

fn plots(frame: &mut Frame, area: Rect, state: &State) {
    let mut numbers: Vec<usize> = state.channels().filter_map(|channel| channel.plot).collect();
    numbers.sort_unstable();
    numbers.dedup();

    if numbers.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Nothing is plotted. On the Devices tab, point at a channel and press 1 to 9.",
                Style::new().fg(DIM),
            ))
            .block(Block::bordered().border_style(Style::new().fg(ACCENT))),
            area,
        );
        return;
    }

    // An even share each. A plot of one channel is no less worth seeing than a
    // plot of four.
    let heights = vec![Constraint::Ratio(1, numbers.len() as u32); numbers.len()];
    for (number, area) in numbers.iter().zip(Layout::vertical(heights).split(area).iter()) {
        render_plot(frame, *area, state, *number);
    }
}

fn render_plot(frame: &mut Frame, area: Rect, state: &State, number: usize) {
    let [drawing, legend] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(LEGEND_WIDTH)]).areas(area);

    let members: Vec<&ChannelRow> =
        state.channels().filter(|channel| channel.plot == Some(number)).collect();

    // Now, for this plot, is its newest reading rather than the clock. The axis
    // then says when a reading was taken and not when it happened to be drawn,
    // and a plot of recorded data means the same thing as a plot of live data.
    let newest = members
        .iter()
        .filter_map(|channel| channel.history.back())
        .map(|point| point.datetime)
        .max();

    // Built before the datasets because a dataset borrows its points, and they
    // have to outlive the widget that reads them.
    let series: Vec<Vec<(f64, f64)>> = members
        .iter()
        .map(|channel| {
            channel
                .history
                .iter()
                .map(|point| {
                    let ago = match newest {
                        Some(newest) => {
                            (point.datetime - newest).num_milliseconds() as f64 / 1000.0
                        }
                        None => 0.0,
                    };
                    (ago, point.value)
                })
                .collect()
        })
        .collect();

    let block = Block::bordered().border_style(Style::new().fg(ACCENT)).title(Span::styled(
        format!(" Plot {} ", number),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    ));

    match vertical(&series) {
        None => frame.render_widget(
            Paragraph::new(Span::styled(" waiting for readings", Style::new().fg(DIM)))
                .block(block),
            drawing,
        ),
        Some(up) => {
            let datasets: Vec<Dataset> = series
                .iter()
                .enumerate()
                .map(|(position, points)| {
                    Dataset::default()
                        .marker(Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::new().fg(SERIES[position % SERIES.len()]))
                        .data(points)
                })
                .collect();
            // The whole window, always, whether or not it has filled up yet.
            // An axis that grew with the data would never agree with the
            // setting that produced it, and a trace filling in from the right
            // is what a plot of live readings is expected to do.
            let window = state.history_seconds as f64;
            frame.render_widget(
                Chart::new(datasets)
                    .block(block)
                    .x_axis(
                        Axis::default().style(Style::new().fg(DIM)).bounds([-window, 0.0]).labels(
                            [format!("-{}", duration_label(state.history_seconds)), "now".to_string()],
                        ),
                    )
                    .y_axis(axis(up, 2, "")),
                drawing,
            );
        }
    }

    // The legend doubles as the reading, which is what the Devices tab would
    // otherwise have to be switched back to for.
    let lines: Vec<Line> = members
        .iter()
        .enumerate()
        .map(|(position, channel)| {
            Line::from(vec![
                Span::styled(
                    channel.name.clone(),
                    Style::new().fg(SERIES[position % SERIES.len()]),
                ),
                Span::styled(
                    match channel.latest {
                        Some(value) => format!("  {:.3} {}", value, channel.unit),
                        None => format!("  --- {}", channel.unit),
                    },
                    Style::new().fg(Color::White),
                ),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::new().padding(Padding::new(1, 0, 1, 0))),
        legend,
    );
}

/// How high a plot has to be, or None when nothing has arrived to draw.
///
/// Only the value axis. The time axis is the window, which is a setting rather
/// than something to be worked out from the data.
fn vertical(series: &[Vec<(f64, f64)>]) -> Option<[f64; 2]> {
    let mut up = [f64::INFINITY, f64::NEG_INFINITY];
    for (_, y) in series.iter().flatten() {
        up = [up[0].min(*y), up[1].max(*y)];
    }
    if !up[0].is_finite() {
        return None;
    }
    // A reading that has not moved, or only one of them, would otherwise be
    // asked to fill an axis of no height at all.
    Some(match up[1] - up[0] < f64::EPSILON {
        true => [up[0] - 1.0, up[1] + 1.0],
        false => {
            let room = (up[1] - up[0]) * 0.05;
            [up[0] - room, up[1] + room]
        }
    })
}

fn axis(bounds: [f64; 2], decimals: usize, suffix: &str) -> Axis<'static> {
    Axis::default().style(Style::new().fg(DIM)).bounds(bounds).labels([
        format!("{:.*}{}", decimals, bounds[0], suffix),
        format!("{:.*}{}", decimals, bounds[1], suffix),
    ])
}

fn settings(frame: &mut Frame, area: Rect, state: &State) {
    let [box_area, hint] =
        Layout::vertical([Constraint::Length(SETTINGS.len() as u16 + 4), Constraint::Min(0)])
            .areas(area);

    let values = [
        // Say what is really being held when the cap has cut the window short,
        // rather than showing a number that is not true.
        match state.capped_window() {
            Some(held) if held + 1 < state.history_seconds => format!(
                "{}   (holding {} at this rate)",
                duration_label(state.history_seconds),
                duration_label(held)
            ),
            _ => duration_label(state.history_seconds),
        },
        format!("save to {}", plot_config::FILE),
    ];

    let rows = SETTINGS.iter().zip(values.iter()).enumerate().map(|(index, (name, value))| {
        let selected = state.setting == index;
        Row::new(vec![
            Span::styled(if selected { ">" } else { " " }, Style::new().fg(Color::White)),
            Span::styled(
                *name,
                match selected {
                    true => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    false => Style::new().fg(ACCENT),
                },
            ),
            Span::styled(value.clone(), Style::new().fg(Color::White)),
        ])
    });

    frame.render_widget(
        Table::new(rows, [Constraint::Length(POINTER_WIDTH), Constraint::Length(16), Constraint::Min(10)])
            .column_spacing(GAP)
            .block(
                Block::bordered()
                    .border_style(Style::new().fg(ACCENT))
                    .padding(Padding::symmetric(1, 1))
                    .title(Span::styled(
                        " Settings ",
                        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                    )),
            ),
        box_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Left and right change a setting.  Enter saves the plot layout.  Tab moves on.",
            Style::new().fg(DIM),
        ))),
        hint,
    );
}

fn log(frame: &mut Frame, area: Rect, state: &State) {
    // Newest last, and only as many as fit, so the latest is always on screen
    // without any scrolling to build yet.
    let lines: Vec<Line> = state
        .log
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .rev()
        .map(|entry| Line::from(Span::styled(entry.clone(), Style::new().fg(Color::White))))
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(Style::new().fg(ACCENT))
                .title(Span::styled(" Log ", Style::new().fg(Color::White))),
        ),
        area,
    );
}

/// Run the display until told to stop, or until the operator says so.
///
/// Setting `stop` is how the display asks for the run to end: the same flag
/// Ctrl-C sets, so the devices wind down and the sink gets its final flush
/// rather than the process being killed with data still buffered.
pub fn run(
    updates: Receiver<Update>,
    mut state: State,
    stop: &AtomicBool,
    recording: &AtomicBool,
) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let outcome = watch(&mut terminal, updates, &mut state, stop, recording);
    ratatui::restore();
    outcome
}

/// What a key press does.
///
/// Kept out of the loop so it can be tested. A display whose keys are only ever
/// exercised by hand is one where a key that quietly does nothing goes
/// unnoticed, which is exactly what happened to the Settings tab.
pub fn press(state: &mut State, key: KeyEvent, stop: &AtomicBool, recording: &AtomicBool) {
    // Raw mode means Ctrl-C arrives as a key rather than as a signal, so unless
    // it is handled here it does nothing at all. It should end a run the way it
    // does everywhere else.
    let interrupt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
    if interrupt {
        stop.store(true, Ordering::Relaxed);
        return;
    }

    // A tab that wants a key for itself gets first refusal, so that a page can
    // use the arrow keys for its own values without them ceasing to move
    // between tabs everywhere else.
    if state.tab == 3 && settings_key(state, key.code) {
        return;
    }
    if state.tab == 0 && devices_key(state, key.code) {
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => stop.store(true, Ordering::Relaxed),
        // On any tab. Whether a run is being recorded is not a property of
        // whichever page happens to be showing.
        KeyCode::Char('r') => {
            let starting = !recording.load(Ordering::Relaxed);
            recording.store(starting, Ordering::Relaxed);
            state.recording = starting.then(Instant::now);
            state.note(
                match starting {
                    true => "recording started",
                    false => "recording stopped",
                }
                .to_string(),
            );
        }
        KeyCode::Tab | KeyCode::Right => state.tab = (state.tab + 1) % TABS.len(),
        KeyCode::BackTab | KeyCode::Left => state.tab = (state.tab + TABS.len() - 1) % TABS.len(),
        _ => {}
    }
}

/// Keys the Settings tab takes for itself. `true` when it used one.
///
/// Left and right adjust the setting being pointed at, which is what anyone
/// reaches for on a page of values. Tab still moves between tabs, so nothing is
/// unreachable.
fn settings_key(state: &mut State, code: KeyCode) -> bool {
    match code {
        KeyCode::Up => state.move_setting(-1),
        KeyCode::Down => state.move_setting(1),
        KeyCode::Left | KeyCode::Char('-') if state.setting == HISTORY_ROW => {
            state.adjust_history(-1)
        }
        KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=')
            if state.setting == HISTORY_ROW =>
        {
            state.adjust_history(1)
        }
        KeyCode::Enter if state.setting == LAYOUT_ROW => {
            let saved = plot_config::write(&state.project, &state.plot_config());
            state.note(match saved {
                Ok(path) => format!("plot layout saved to {}", path.display()),
                Err(problem) => problem,
            });
        }
        _ => return false,
    }
    true
}

/// Keys the Devices tab takes for itself. `true` when it used one.
fn devices_key(state: &mut State, code: KeyCode) -> bool {
    match code {
        KeyCode::Up => state.move_selection(-1),
        KeyCode::Down => state.move_selection(1),
        // Zero takes a channel off a plot, there being no plot zero to put it
        // on.
        KeyCode::Char(digit) if digit.is_ascii_digit() => {
            state.assign(match digit.to_digit(10).unwrap_or(0) as usize {
                0 => None,
                plot => Some(plot),
            })
        }
        KeyCode::Char('-') => state.assign(None),
        _ => return false,
    }
    true
}

fn watch(
    terminal: &mut ratatui::DefaultTerminal,
    updates: Receiver<Update>,
    state: &mut State,
    stop: &AtomicBool,
    recording: &AtomicBool,
) -> std::io::Result<()> {
    let mut last_drawn = Instant::now() - TICK;
    while !stop.load(Ordering::Relaxed) {
        // Wait for something, then take everything else already waiting. At a
        // high sample rate that is many batches for one redraw, which is the
        // point: the screen cannot show more than it can show.
        match updates.recv_timeout(TICK) {
            Ok(update) => {
                state.apply(update);
                while let Ok(update) = updates.try_recv() {
                    state.apply(update);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The run has finished and dropped its senders.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_drawn.elapsed() >= TICK {
            terminal.draw(|frame| draw(frame, state))?;
            last_drawn = Instant::now();
        }

        if event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                // Windows reports a key going down and coming back up. Without
                // this every press counts twice, and Tab skips a tab.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                press(state, key, stop, recording);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render one frame into a buffer and give back what it looks like.
    ///
    /// The point of keeping `draw` apart from the loop: layout can be checked
    /// on a machine with no terminal, in a test that runs in milliseconds.
    fn rendered(state: &State, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for row in 0..buffer.area.height {
            for column in 0..buffer.area.width {
                screen.push_str(buffer[(column, row)].symbol());
            }
            screen.push('\n');
        }
        screen
    }

    fn channel(name: &str, unit: &str, latest: Option<f64>, readings: usize) -> ChannelRow {
        ChannelRow {
            name: name.to_string(),
            unit: unit.to_string(),
            latest: latest,
            readings: readings,
            plot: None,
            history: VecDeque::new(),
        }
    }

    /// Readings a second apart, starting at the epoch, so a plot has something
    /// with a known shape to draw.
    fn readings(values: &[f64]) -> Vec<DataPoint> {
        let origin = DateTime::from_timestamp(0, 0).unwrap();
        values
            .iter()
            .enumerate()
            .map(|(second, value)| DataPoint {
                datetime: origin + chrono::Duration::seconds(second as i64),
                value: *value,
            })
            .collect()
    }

    /// A directory of this test own, so tests writing files cannot collide.
    fn scratch(name: &str) -> String {
        let directory = std::env::temp_dir().join(format!("choptui_{}", name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory.to_string_lossy().to_string()
    }

    fn state() -> State {
        State {
            project: "test_projects/scaled".to_string(),
            devices: vec![
                DeviceRow {
                    name: "Rig".to_string(),
                    status: Status::Connected,
                    channels: vec![
                        channel("Flow", "L/min", Some(14.5), 63),
                        channel("Pressure", "bar", Some(7.25), 61),
                    ],
                },
                DeviceRow {
                    name: "Missing rig".to_string(),
                    status: Status::Disconnected(Some("port not found".to_string())),
                    channels: vec![channel("Temperature", "C", None, 0)],
                },
            ],
            log: VecDeque::new(),
            tab: 0,
            selected: 0,
            history_seconds: HISTORY_DEFAULT,
            plot_layout: None,
            setting: 0,
            recording: None,
        }
    }

    #[test]
    fn the_devices_tab_shows_every_channel_and_its_latest_reading() {
        let screen = rendered(&state(), 74, 16);
        println!("{}", screen);
        assert!(screen.contains("LUMBERJACK"), "{}", screen);
        assert!(screen.contains("14.500 L/min"), "{}", screen);
        assert!(screen.contains("Connected"), "{}", screen);
    }

    #[test]
    fn a_device_that_never_connected_still_appears_with_its_channels() {
        // Otherwise the one thing worth looking at, a rig that is not there,
        // would be the one thing the screen does not mention.
        let screen = rendered(&state(), 74, 16);
        assert!(screen.contains("Missing rig"), "{}", screen);
        assert!(screen.contains("Temperature"), "{}", screen);
        assert!(screen.contains("port not found"), "{}", screen);
    }

    #[test]
    fn nothing_yet_reads_differently_from_a_reading_of_zero() {
        let mut state = state();
        state.devices[0].channels[0].latest = Some(0.0);
        let screen = rendered(&state, 74, 16);
        assert!(screen.contains("0.000 L/min"), "{}", screen);
        assert!(screen.contains("--- C"), "{}", screen);
    }

    #[test]
    fn readings_update_the_row_they_belong_to() {
        let mut state = state();
        state.apply(Update::Data {
            device: "Rig".to_string(),
            channel: "Flow".to_string(),
            points: readings(&[1.0, 2.0, 21.0]),
        });
        assert_eq!(state.devices[0].channels[0].latest, Some(21.0));
        assert_eq!(state.devices[0].channels[0].readings, 66);
        // Every reading is kept, not just the newest, or a plot would be
        // drawing the drain rate rather than the signal.
        assert_eq!(state.devices[0].channels[0].history.len(), 3);
    }

    #[test]
    fn a_reading_for_something_not_in_the_setup_is_ignored() {
        // A calculated device is not in `devices`, so its batches arrive naming
        // a channel the screen has no row for. Dropping them beats panicking.
        let mut state = state();
        state.apply(Update::Data {
            device: "Derived".to_string(),
            channel: "Delta P".to_string(),
            points: readings(&[1.0]),
        });
        assert_eq!(state.devices[0].channels[0].readings, 63);
    }

    #[test]
    fn losing_a_device_says_why_on_the_status_and_in_the_log() {
        let mut state = state();
        state.apply(Update::Disconnected {
            device: "Rig".to_string(),
            cause: Some("the port went away".to_string()),
        });
        let screen = rendered(&state, 74, 16);
        assert!(screen.contains("the port went away"), "{}", screen);
        state.tab = 2;
        let screen = rendered(&state, 74, 16);
        assert!(screen.contains("Rig disconnected: the port went away"), "{}", screen);
    }

    #[test]
    fn a_box_is_only_as_wide_as_it_needs_to_be() {
        // Stretched across the terminal, a name and its reading ended up at
        // opposite ends of the screen.
        let screen = rendered(&state(), 120, 16);
        let widest = screen
            .lines()
            .filter(|line| line.contains("L/min"))
            .map(|line| line.trim_end().chars().count())
            .max()
            .unwrap();
        assert!(widest < 60, "box is {} wide on a 120 column terminal", widest);
    }

    #[test]
    fn a_long_reason_does_not_set_the_width_of_every_box() {
        // What a serial port really says when it is not there. On the border it
        // would widen every device on screen; the log has it in full.
        let mut state = state();
        state.apply(Update::Disconnected {
            device: "Missing rig".to_string(),
            cause: Some("The system cannot find the file specified.".to_string()),
        });
        let screen = rendered(&state, 120, 16);
        assert!(!screen.contains("cannot find the file specified."), "{}", screen);
        assert!(screen.contains("The system cannot"), "{}", screen);
        assert!(
            state.log.iter().any(|line| line.ends_with("file specified.")),
            "the whole reason should still be in the log"
        );
    }

    #[test]
    fn devices_that_do_not_fit_are_counted_rather_than_dropped() {
        // A screen quietly showing fewer devices than the setup has looks like
        // a setup with fewer devices in it.
        let screen = rendered(&state(), 74, 10);
        assert!(screen.contains("Rig"), "{}", screen);
        assert!(screen.contains("1 more below"), "{}", screen);
    }

    /// Point at a channel and put it on a plot, the way the keys do.
    fn plot(state: &mut State, selected: usize, number: usize) {
        state.selected = selected;
        state.assign(Some(number));
    }

    #[test]
    fn the_pointer_runs_through_every_device_not_just_one() {
        let mut state = state();
        state.move_selection(2);
        // Two channels on the first device, so this is the second device.
        state.assign(Some(1));
        assert_eq!(state.devices[1].channels[0].plot, Some(1));
    }

    #[test]
    fn the_pointer_stops_at_the_ends_rather_than_wrapping() {
        let mut state = state();
        state.move_selection(-1);
        assert_eq!(state.selected, 0);
        state.move_selection(99);
        assert_eq!(state.selected, 2, "three channels, so the last is 2");
    }

    #[test]
    fn nothing_is_plotted_until_something_is_put_on_a_plot() {
        let mut state = state();
        state.tab = 1;
        let screen = rendered(&state, 90, 20);
        assert!(screen.contains("Nothing is plotted"), "{}", screen);
    }

    #[test]
    fn a_plot_draws_its_channels_and_names_them_beside_it() {
        let mut state = state();
        state.apply(Update::Data {
            device: "Rig".to_string(),
            channel: "Flow".to_string(),
            points: readings(&[1.0, 4.0, 2.0, 8.0, 5.0]),
        });
        plot(&mut state, 0, 1);
        state.tab = 1;
        let screen = rendered(&state, 90, 20);
        println!("{}", screen);
        assert!(screen.contains("Plot 1"), "{}", screen);
        // The legend is the reading as well, so the Devices tab does not have
        // to be switched back to for it.
        assert!(screen.contains("Flow"), "{}", screen);
        assert!(screen.contains("5.000 L/min"), "{}", screen);
        // A few seconds of readings sit at the right hand end of the window,
        // which is what a plot of live data does.
        assert!(screen.contains("now"), "{}", screen);
    }

    #[test]
    fn channels_on_different_plots_get_a_plot_each() {
        let mut state = state();
        plot(&mut state, 0, 1);
        plot(&mut state, 1, 3);
        state.tab = 1;
        let screen = rendered(&state, 90, 20);
        assert!(screen.contains("Plot 1"), "{}", screen);
        assert!(screen.contains("Plot 3"), "{}", screen);
    }

    #[test]
    fn a_channel_comes_off_a_plot_again() {
        let mut state = state();
        plot(&mut state, 0, 1);
        assert_eq!(state.devices[0].channels[0].plot, Some(1));
        state.assign(None);
        assert_eq!(state.devices[0].channels[0].plot, None);
    }

    #[test]
    fn a_plot_of_a_reading_that_never_moves_still_has_an_axis() {
        // Otherwise the axis is zero high and there is nowhere to draw.
        let mut state = state();
        state.apply(Update::Data {
            device: "Rig".to_string(),
            channel: "Flow".to_string(),
            points: readings(&[7.0, 7.0, 7.0]),
        });
        plot(&mut state, 0, 1);
        state.tab = 1;
        let screen = rendered(&state, 90, 20);
        assert!(screen.contains("6.00"), "{}", screen);
        assert!(screen.contains("8.00"), "{}", screen);
    }

    #[test]
    fn a_plotted_channel_with_no_readings_yet_says_so() {
        let mut state = state();
        plot(&mut state, 2, 1);
        state.tab = 1;
        let screen = rendered(&state, 90, 20);
        assert!(screen.contains("waiting for readings"), "{}", screen);
    }

    #[test]
    fn history_is_kept_for_the_window_and_no_longer() {
        // Readings a second apart, so a minute of window is a minute of them.
        let mut state = state();
        let values: Vec<f64> = (0..200).map(|n| n as f64).collect();
        state.apply(Update::Data {
            device: "Rig".to_string(),
            channel: "Flow".to_string(),
            points: readings(&values),
        });
        let history = &state.devices[0].channels[0].history;
        // Sixty seconds back from the newest, inclusive of both ends.
        assert_eq!(history.len(), HISTORY_DEFAULT as usize + 1);
        // The oldest go, not the newest.
        assert_eq!(history.back().unwrap().value, 199.0);
        assert_eq!(history.front().unwrap().value, 139.0);
    }

    #[test]
    fn shortening_the_window_takes_effect_at_once() {
        // Otherwise it would only appear to work once enough new readings had
        // arrived to push the old ones out, which on a slow rig is a long wait.
        let mut state = state();
        let values: Vec<f64> = (0..200).map(|n| n as f64).collect();
        state.apply(Update::Data {
            device: "Rig".to_string(),
            channel: "Flow".to_string(),
            points: readings(&values),
        });
        state.adjust_history(-1);
        assert_eq!(state.history_seconds, 30);
        assert_eq!(state.devices[0].channels[0].history.len(), 31);
    }

    #[test]
    fn the_window_steps_through_the_choices_and_stops_at_the_ends() {
        let mut state = state();
        state.adjust_history(1);
        assert_eq!(state.history_seconds, 120);
        for _ in 0..20 {
            state.adjust_history(1);
        }
        assert_eq!(state.history_seconds, *HISTORY_CHOICES.last().unwrap());
        for _ in 0..20 {
            state.adjust_history(-1);
        }
        assert_eq!(state.history_seconds, HISTORY_CHOICES[0]);
    }

    #[test]
    fn a_window_from_a_file_is_honoured_as_written_and_still_steps() {
        // A layout written elsewhere may hold a value that is not one of the
        // choices. Rounding it on load would quietly change somebody settings.
        let mut state = state();
        state.apply_plot_config(PlotConfig {
            version: 1,
            history_seconds: 45,
            layout: None,
            plots: vec![],
        });
        assert_eq!(state.history_seconds, 45);
        state.adjust_history(1);
        assert_eq!(state.history_seconds, 60, "the next choice above 45");
    }

    #[test]
    fn a_layout_survives_being_written_out_and_read_back() {
        let mut original = state();
        plot(&mut original, 0, 1);
        plot(&mut original, 2, 3);
        original.history_seconds = 300;

        let directory = scratch("round_trip");
        let written = plot_config::write(&directory, &original.plot_config()).unwrap();
        assert!(written.ends_with(plot_config::FILE));

        let mut reopened = state();
        reopened.apply_plot_config(plot_config::read(&directory).unwrap().unwrap());
        assert_eq!(reopened.history_seconds, 300);
        assert_eq!(reopened.devices[0].channels[0].plot, Some(1));
        assert_eq!(reopened.devices[0].channels[1].plot, None);
        // The number typed, not the position in the list: plot 3 stays plot 3.
        assert_eq!(reopened.devices[1].channels[0].plot, Some(3));
        assert!(reopened.log.is_empty(), "{:?}", reopened.log);
    }

    #[test]
    fn a_layout_naming_a_channel_this_setup_lacks_is_noted_not_refused() {
        // A layout saved with a device attached should not stop the display
        // opening when that device is not there.
        let mut state = state();
        state.apply_plot_config(PlotConfig {
            version: 1,
            history_seconds: 60,
            layout: None,
            plots: vec![Plot {
                number: 1,
                name: None,
                channels: vec![ChannelRef {
                    device: "Ghost".to_string(),
                    channel: "Nothing".to_string(),
                }],
            }],
        });
        assert_eq!(state.log.len(), 1);
        assert!(state.log[0].contains("Ghost/Nothing"), "{}", state.log[0]);
    }

    #[test]
    fn no_saved_layout_is_not_a_failure() {
        // The ordinary case for a project only ever recorded from the CLI.
        let directory = scratch("never_saved");
        assert!(plot_config::read(&directory).unwrap().is_none());
    }

    #[test]
    fn a_layout_that_will_not_parse_is_a_failure() {
        // It was meant to say something, so silence would be the wrong answer.
        let directory = scratch("nonsense");
        std::fs::write(plot_config::path(&directory), "{ not json").unwrap();
        assert!(plot_config::read(&directory).is_err());
    }

    #[test]
    fn the_settings_tab_shows_what_each_setting_is() {
        let mut state = state();
        state.tab = 3;
        let screen = rendered(&state, 74, 16);
        println!("{}", screen);
        assert!(screen.contains("Plot history"), "{}", screen);
        assert!(screen.contains("1m"), "{}", screen);
        assert!(screen.contains(plot_config::FILE), "{}", screen);
    }

    #[test]
    fn a_length_of_time_reads_the_short_way() {
        assert_eq!(duration_label(10), "10s");
        assert_eq!(duration_label(30), "30s");
        assert_eq!(duration_label(60), "1m");
        assert_eq!(duration_label(1800), "30m");
        assert_eq!(duration_label(45), "45s");
    }

    #[test]
    fn a_plot_numbers_its_channels_the_same_way_the_devices_tab_colours_them() {
        // The box beside a channel is coloured as the line it produces, which
        // only holds if both count position the same way.
        let mut state = state();
        plot(&mut state, 0, 1);
        plot(&mut state, 2, 1);
        let positions = series_positions(&state);
        assert_eq!(positions[0], Some(0));
        assert_eq!(positions[1], None);
        assert_eq!(positions[2], Some(1));
    }

    #[test]
    fn the_header_offers_to_record_when_it_is_not() {
        let screen = rendered(&state(), 74, 16);
        assert!(screen.contains("not recording"), "{}", screen);
        assert!(screen.contains("record (r)"), "{}", screen);
    }

    #[test]
    fn the_header_counts_a_recording_as_it_goes() {
        let mut state = state();
        state.recording = Some(Instant::now() - Duration::from_secs(3671));
        let screen = rendered(&state, 74, 16);
        assert!(screen.contains("REC"), "{}", screen);
        // An hour, a minute and eleven seconds.
        assert!(screen.contains("01:01:11"), "{}", screen);
        assert!(screen.contains("stop (r)"), "{}", screen);
    }

    #[test]
    fn a_clock_reads_as_a_clock() {
        assert_eq!(clock(Duration::from_secs(0)), "00:00:00");
        assert_eq!(clock(Duration::from_secs(59)), "00:00:59");
        assert_eq!(clock(Duration::from_secs(600)), "00:10:00");
        assert_eq!(clock(Duration::from_secs(86_399)), "23:59:59");
    }

    /// Press a key, the way the loop does.
    fn press_key(state: &mut State, code: KeyCode) {
        let stop = AtomicBool::new(false);
        let recording = AtomicBool::new(false);
        press(state, KeyEvent::new(code, KeyModifiers::NONE), &stop, &recording);
    }

    #[test]
    fn the_history_setting_can_actually_be_changed() {
        // The keys were never tested, and the ones the Settings tab needed did
        // not reach it. Left and right are what anyone reaches for on a page of
        // values, and they were switching tabs instead.
        let mut state = state();
        state.tab = 3;
        assert_eq!(state.history_seconds, 60);

        press_key(&mut state, KeyCode::Right);
        assert_eq!(state.history_seconds, 120, "right should lengthen the window");
        assert_eq!(state.tab, 3, "and should not have moved off the tab");

        press_key(&mut state, KeyCode::Left);
        press_key(&mut state, KeyCode::Left);
        assert_eq!(state.history_seconds, 30);
        assert_eq!(state.tab, 3);
    }

    #[test]
    fn plus_and_minus_change_it_too() {
        let mut state = state();
        state.tab = 3;
        press_key(&mut state, KeyCode::Char('+'));
        assert_eq!(state.history_seconds, 120);
        press_key(&mut state, KeyCode::Char('-'));
        assert_eq!(state.history_seconds, 60);
    }

    #[test]
    fn tab_still_moves_on_from_a_page_that_wants_the_arrow_keys() {
        // Otherwise the Settings tab would be a place you could not leave.
        let mut state = state();
        state.tab = 3;
        press_key(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, 0);
        press_key(&mut state, KeyCode::BackTab);
        assert_eq!(state.tab, 3);
    }

    #[test]
    fn the_arrow_keys_still_move_between_tabs_everywhere_else() {
        let mut state = state();
        state.tab = 1;
        press_key(&mut state, KeyCode::Right);
        assert_eq!(state.tab, 2);
        press_key(&mut state, KeyCode::Left);
        assert_eq!(state.tab, 1);
    }

    #[test]
    fn the_settings_pointer_moves_and_the_keys_follow_it() {
        let mut state = state();
        state.tab = 3;
        press_key(&mut state, KeyCode::Down);
        assert_eq!(state.setting, LAYOUT_ROW);
        // The window is not the row being pointed at, so it must not move.
        press_key(&mut state, KeyCode::Right);
        assert_eq!(state.history_seconds, 60);
    }

    #[test]
    fn enter_on_the_layout_row_saves_it() {
        let mut state = state();
        state.project = scratch("enter_saves");
        state.tab = 3;
        state.setting = LAYOUT_ROW;
        plot(&mut state, 0, 1);
        press_key(&mut state, KeyCode::Enter);
        assert!(plot_config::read(&state.project).unwrap().is_some());
        assert!(state.log.iter().any(|line| line.contains("saved")), "{:?}", state.log);
    }

    #[test]
    fn the_devices_tab_keys_reach_it() {
        let mut state = state();
        press_key(&mut state, KeyCode::Down);
        assert_eq!(state.selected, 1);
        press_key(&mut state, KeyCode::Char('2'));
        assert_eq!(state.devices[0].channels[1].plot, Some(2));
        press_key(&mut state, KeyCode::Char('0'));
        assert_eq!(state.devices[0].channels[1].plot, None);
    }

    #[test]
    fn a_digit_on_another_tab_does_not_quietly_change_a_plot() {
        let mut state = state();
        state.tab = 2;
        press_key(&mut state, KeyCode::Char('3'));
        assert!(state.channels().all(|channel| channel.plot.is_none()));
    }

    #[test]
    fn ctrl_c_stops_the_run_from_any_tab() {
        let mut state = state();
        state.tab = 3;
        let stop = AtomicBool::new(false);
        let recording = AtomicBool::new(false);
        press(
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &stop,
            &recording,
        );
        assert!(stop.load(Ordering::Relaxed));
    }

    #[test]
    fn r_records_from_any_tab_including_one_that_takes_its_own_keys() {
        let mut state = state();
        state.tab = 3;
        let stop = AtomicBool::new(false);
        let recording = AtomicBool::new(false);
        press(&mut state, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &stop, &recording);
        assert!(recording.load(Ordering::Relaxed));
        assert!(state.recording.is_some());
    }

    #[test]
    fn the_time_axis_is_the_window_and_says_so() {
        // The complaint that started this: a plot showing seconds since the run
        // began never agreed with a setting given as a length of time.
        let mut state = state();
        state.apply(Update::Data {
            device: "Rig".to_string(),
            channel: "Flow".to_string(),
            points: readings(&[1.0, 4.0, 2.0]),
        });
        plot(&mut state, 0, 1);
        state.tab = 1;

        let screen = rendered(&state, 90, 20);
        println!("{}", screen);
        assert!(screen.contains("-1m"), "axis should be the window: {}", screen);
        assert!(screen.contains("now"), "{}", screen);

        state.history_seconds = 300;
        let screen = rendered(&state, 90, 20);
        assert!(screen.contains("-5m"), "axis should follow the setting: {}", screen);
    }

    #[test]
    fn a_problem_is_logged_without_marking_the_device_down() {
        // The port is fine and the device keeps being read. Showing it as
        // disconnected would be a lie that hides a real disconnection later.
        let mut state = state();
        state.apply(Update::Problem {
            device: "Rig".to_string(),
            message: "frame would not parse".to_string(),
        });
        assert!(matches!(state.devices[0].status, Status::Connected));
        assert_eq!(state.log.len(), 1);
    }
}
