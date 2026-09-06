mod acquisition;
mod devicewatch;
mod logbook;
mod look;
mod settings;

// Glob imports, for these two only: they are this crate's own vocabulary
// for drawing, used on nearly every line of `view`, and listing forty names
// would say less than it costs. `look::menu` is spelled out where it is
// called, because lucide has an icon of that name and a glob cannot choose.
use crate::acquisition::*;
use crate::look::*;
use chrono::{DateTime, Local, Utc};
use iced::widget::markdown;
use iced::widget::pane_grid::{self, Axis, Configuration, PaneGrid};
use iced::widget::{
    button, checkbox, column, container, opaque, pick_list, row, scrollable, slider, space,
    stack, text, text_input, MouseArea, Row,
};
use iced::window;

use crate::settings::UserSettings;
use iced::event::{self, Event, Status};
use iced::mouse;
use iced::{padding, Border, Bottom, Center, Color, Element, Fill, Point, Subscription, Theme, Top};
// use iced::border;
use iced_fonts::lucide::*;
use iced_plot::{
    LineStyle, PlotStyle, PlotUiMessage, PlotWidget, PlotWidgetBuilder, Series, ShapeId,
};
use lumberdaq::calculated::ChannelRef;
use lumberdaq::channel::{Channel, Scale};
use lumberdaq::config::DaqConfig;
use lumberdaq::configuration::read_configuration_file;
use lumberdaq::daq::DaqInfo;
use lumberdaq::datapoint::DataPoint;
// Qualified rather than imported: `Acquisition` here is this file's own struct
// for a run in progress, and a mock device's way of sampling is a different
// thing that happens to share the word.
use lumberdaq::hardware::mock_hardware::{self, MockHardwareInput};
use lumberdaq::hardware::serial_stream::StreamCheck;
use lumberdaq::hardware::{pico_hrdl, serial_stream};
use lumberdaq::hardware::HardwareConfig;
use lumberdaq::plot_config::{self, PlotLayout, SplitAxis};
use lumberdaq::project::Project;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};


/// How many batches may be waiting for the interface before they start being
/// dropped. See `DisplaySink::write_batch` for why dropping is the answer.
const CHANNEL_DEPTH: usize = 256;

/// How far back a plot goes when its project has no saved layout to say.
const DEFAULT_HISTORY_SECONDS: u64 = 5;


/// How much of the window the sidebar takes, and how it divides between the
/// device list and the settings below it.
const SIDEBAR_WIDTH: f32 = 0.3;
const SIDEBAR_SPLIT: f32 = 0.5;

/// Where the log's divider sits when it is open, and when it is shut.
///
/// Shut leaves just enough for its own title row, which is what makes it
/// openable again.
const LOG_OPEN: f32 = 0.78;
const LOG_SHUT: f32 = 0.95;

/// How long a plot keeps readings for, to choose between.
///
/// Steps rather than a free number: the point of the setting is how far back a
/// plot goes, and a handful of round answers covers that better than typing
/// seconds. The same list choptui offers, so the two interfaces do not
/// disagree about what the sensible spans are.
const HISTORY_CHOICES: [u64; 7] = [10, 30, 60, 120, 300, 600, 1800];

/// How often to collect from a device, to choose between.
///
/// Stepped for the same reason the history is, and because this is a rate a
/// rig is run at rather than a number to tune: a device read every 100 ms or
/// every second is a decision, 137 ms is a typo.
const READ_INTERVALS: [u64; 8] = [10, 20, 50, 100, 200, 500, 1000, 5000];

/// How long to wait after the last change before writing a project out.
///
/// Saving on every change would mean a file write per keystroke while
/// something is being renamed. Waiting for the typing to stop turns that into
/// one write, without asking anybody to remember to press save.
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// Make room for a new plot beside the last one.
///
/// Splitting what is already there rather than rebuilding the grid, so an
/// arrangement somebody has dragged into shape survives adding to it. A grid
/// with nothing in it has nothing to split, so the new plot becomes the whole
/// of it.
fn split_last(panes: &mut pane_grid::State<usize>, number: usize) {
    match panes.iter().map(|(pane, _)| *pane).last() {
        Some(last) => {
            panes.split(Axis::Horizontal, last, number);
        }
        None => *panes = pane_grid::State::with_configuration(Configuration::Pane(number)),
    }
}

/// A plot with nothing on it yet.
///
/// No widget: `PlotWidgetBuilder` will not build a plot of nothing, and a plot
/// gets one when its first channel arrives.
fn empty_plot(number: usize) -> AppPlot {
    AppPlot { number, name: format!("Plot {}", number), channels: Vec::new(), widget: None }
}

/// A rig with nothing in it, for when no project is open.
///
/// Not a failure state: a setup with no devices is a setup, and treating it as
/// one is what keeps every panel written as though a project were open.
fn empty_config() -> DaqConfig {
    DaqConfig {
        info: lumberdaq::daq::DaqInfo { name: String::new(), author: String::new() },
        devices: Vec::new(),
        calculated: None,
    }
}

/// How the window is divided, before anybody has dragged anything.
///
/// The log runs the full width along the bottom, under both columns, rather
/// than sitting inside either of them: it reports on the run as a whole, not
/// on the devices or the plots.
fn default_panes() -> pane_grid::State<PaneKind> {
    pane_grid::State::with_configuration(Configuration::Split {
        axis: Axis::Horizontal,
        // Shut to begin with: the log is worth having when something has gone
        // wrong, and worth nothing the rest of the time.
        ratio: LOG_SHUT,
        a: Box::new(Configuration::Split {
            axis: Axis::Vertical,
            ratio: SIDEBAR_WIDTH,
            a: Box::new(Configuration::Split {
                axis: Axis::Horizontal,
                ratio: SIDEBAR_SPLIT,
                a: Box::new(Configuration::Pane(PaneKind::Devices)),
                b: Box::new(Configuration::Pane(PaneKind::Config)),
            }),
            b: Box::new(Configuration::Pane(PaneKind::Data)),
        }),
        b: Box::new(Configuration::Pane(PaneKind::Log)),
    })
}

/// Whether the calculated channels all build, in the form the dot wants.
///
/// `Some(false)` - a red dot - when any equation will not compile, and `None`
/// when they all do. A calculated device is worked out rather than read, so it
/// has nothing to be connected *to*: a healthy one draws no dot at all and
/// only trouble is worth the ink.
///
/// Worth an alarm because of the shape of the failure. A channel whose
/// equation is wrong is not refused when the run starts - the run starts
/// perfectly well and that channel silently produces nothing, which looks
/// exactly like hardware that is not sending. The panel says what is wrong
/// once the channel is selected; this is what sends somebody to look.
fn calculated_health(config: &DaqConfig) -> Option<bool> {
    let calculated = config.calculated.as_ref()?;

    calculated
        .channels
        .iter()
        // An equation nobody has written yet is unfinished, not wrong. A
        // channel is added with an empty one on purpose, so judging those
        // would turn the device red the moment somebody made a channel and
        // before they had typed a character into it.
        .filter(|channel| !channel.equation.trim().is_empty())
        .any(|channel| channel.validate().is_err())
        .then_some(false)
}



fn devices_from(config: &DaqConfig) -> Vec<AppDevice> {
    let measured = config.devices.iter().enumerate().map(|(index, device)| AppDevice {
        name: device.info.name.clone(),
        channels: device
            .hardware
            .channel_infos()
            .into_iter()
            .map(|channel| AppChannel {
                name: channel.name,
                unit: channel.unit,
                latest: None,
                samples: 0,
            })
            .collect(),
        // Open to begin with. A tree that has to be opened before it says
        // anything hides the thing somebody came to look at, and a rig has few
        // enough devices that showing them all is no worse than showing none.
        expanded: true,
        kind: DeviceKind::Measured(index),
        connected: None,
        concern: None,
    });

    // Last, and in the same list rather than beside it: it is a device to
    // anyone reading the results, its readings arrive through the same channel
    // tagged with its name, and putting it here is what makes those readings
    // show up beside the measured ones with no extra machinery.
    let calculated = config.calculated.iter().map(|device| AppDevice {
        name: device.info.name.clone(),
        channels: device
            .channels
            .iter()
            .map(|channel| AppChannel {
                name: channel.info.name.clone(),
                unit: channel.info.unit.clone(),
                latest: None,
                samples: 0,
            })
            .collect(),
        expanded: true,
        kind: DeviceKind::Calculated,
        connected: calculated_health(config),
        concern: None,
    });

    measured.chain(calculated).collect()
}

/// The trace one channel is drawn as.
///
/// A series cannot be built empty and no data has arrived for a channel just
/// put on a plot, so each starts with points far enough in the past to be
/// outside any viewport. The first real batch trims them away.
///
/// Two of them, at zero and one, rather than one at zero. A single point gives
/// autoscale a range of no height, which it answers with a sliver either side
/// of zero and an axis labelled in ten-millionths - which is what an empty
/// plot used to show. Two points spanning zero to one say "nothing here yet"
/// in the units anybody would draw an empty axis in.
///
/// Not `set_y_lim`, which would do the same job and never stop doing it: a
/// manual limit overrides the data range for the life of the plot and iced_plot
/// offers no way to take one back off.
fn new_series(reference: &ChannelRef, colour: Color) -> Series {
    Series::line_only(
        vec![[f64::MIN / 2.0, 0.0], [f64::MIN / 2.0, 1.0]],
        LineStyle::solid(),
    )
    .with_label(reference.channel.clone())
    .with_color(colour)
}

/// Turn a saved arrangement into one iced can lay out, dropping any plot that
/// is no longer there.
///
/// A split that loses one half becomes whatever is left of it rather than a
/// gap, so deleting a plot elsewhere closes up the arrangement instead of
/// leaving a hole in it.
fn configuration_from(layout: &PlotLayout, known: &[usize]) -> Option<Configuration<usize>> {
    match layout {
        PlotLayout::Plot { number } => {
            known.contains(number).then_some(Configuration::Pane(*number))
        }
        PlotLayout::Split { axis, ratio, first, second } => {
            match (configuration_from(first, known), configuration_from(second, known)) {
                (Some(a), Some(b)) => Some(Configuration::Split {
                    axis: match axis {
                        SplitAxis::Horizontal => Axis::Horizontal,
                        SplitAxis::Vertical => Axis::Vertical,
                    },
                    ratio: *ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// Plots stacked one above another, for a project nobody has arranged.
///
/// Equal shares: the first of three gets a third, and what is left is divided
/// again below it.
fn stacked(numbers: &[usize]) -> Option<Configuration<usize>> {
    let (first, rest) = numbers.split_first()?;
    match stacked(rest) {
        None => Some(Configuration::Pane(*first)),
        Some(rest) => Some(Configuration::Split {
            axis: Axis::Horizontal,
            ratio: 1.0 / numbers.len() as f32,
            a: Box::new(Configuration::Pane(*first)),
            b: Box::new(rest),
        }),
    }
}

/// Read an arrangement back out of the grid, to be saved.
fn layout_from(node: &pane_grid::Node, panes: &pane_grid::State<usize>) -> Option<PlotLayout> {
    match node {
        pane_grid::Node::Pane(pane) => {
            panes.get(*pane).map(|number| PlotLayout::Plot { number: *number })
        }
        pane_grid::Node::Split { axis, ratio, a, b, .. } => {
            match (layout_from(a, panes), layout_from(b, panes)) {
                (Some(first), Some(second)) => Some(PlotLayout::Split {
                    axis: match axis {
                        Axis::Horizontal => SplitAxis::Horizontal,
                        Axis::Vertical => SplitAxis::Vertical,
                    },
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// Build one plot: a trace per channel, and the widget to draw them on.
///
/// The channels are expected to have been checked against the setup already;
/// anything left here gets a trace.
fn build_plot(
    number: usize,
    name: String,
    channels: Vec<ChannelRef>,
    window_seconds: f64,
) -> AppPlot {
    let mut builder = PlotWidgetBuilder::new()
        .with_x_label("Time (s)")
        // Left to autoscale: these are real channels in real units, so there
        // is no sensible fixed range to impose. The x axis is set below
        // instead, which overrides autoscaling for that axis alone.
        .with_autoscale_on_updates(true)
        // Picking a point out of a trace that is sliding leftwards is chasing
        // a moving target, and the highlight covers the data while you do it.
        .with_highlight_on_hover(false)
        // Drawn as ordinary widgets above the plot instead, since the built-in
        // one sits over the y axis and its position is not something
        // iced_plot exposes.
        .disable_legend()
        // Both the frame and the canvas behind it take the card's colour, so a
        // plot reads as part of its card rather than as a panel sitting on
        // one. Everything else is left to iced_plot's theme-derived defaults.
        .with_style(|theme: &Theme| PlotStyle {
            frame: container::background(card_colour(theme)),
            plot_area: container::background(card_colour(theme)),
            ..iced_plot::default_style(theme)
        });

    let mut plotted = Vec::new();
    for (index, reference) in channels.into_iter().enumerate() {
        let colour = PALETTE[index % PALETTE.len()];
        let series = new_series(&reference, colour);

        plotted.push(PlottedChannel { reference, source: None, colour, series: series.id });
        builder = builder.add_series(series);
    }

    let widget = match plotted.is_empty() {
        true => None,
        false => {
            let mut widget = builder.build().expect("a plot of the channels it was given");
            // The in-canvas `?` and its panel: help for a widget's own mouse
            // controls, sitting on top of the data in every plot on screen.
            widget.set_controls_help(false);
            widget.set_x_lim(-window_seconds, 0.0);
            Some(widget)
        }
    };

    AppPlot { number, name, channels: plotted, widget }
}

/// A device name nothing in the setup is using yet.
///
/// Names are how a batch finds its way back to the channel that produced it,
/// so two devices sharing one would send their data to the same place.
fn unused_name(wanted: &str, config: &DaqConfig) -> String {
    let taken = |name: &str| config.devices.iter().any(|device| device.info.name == name);

    if !taken(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|suffix| format!("{} {}", wanted, suffix))
        .find(|name| !taken(name))
        .unwrap_or_else(|| wanted.to_string())
}

/// What the calculated device is called in the list of kinds to add.
///
/// Not one of `HardwareConfig::TYPE_NAMES`, because it is not hardware: it
/// lives in a field of its own in the config for exactly that reason. It is
/// offered alongside them because from where somebody is standing it is
/// another thing that produces channels.
const CALCULATED_TYPE: &str = "Calculated";

/// A variable name this equation is not already using.
///
/// Single letters, because they are what an equation reads well with and what
/// the inputs are called in every worked example. Whoever writes the equation
/// renames them to whatever means something to them.
fn unused_variable(taken: &std::collections::BTreeMap<String, ChannelRef>) -> String {
    ('a'..='z')
        .map(|letter| letter.to_string())
        .find(|name| !taken.contains_key(name))
        .unwrap_or_else(|| format!("v{}", taken.len() + 1))
}

/// A channel name this device is not already using.
fn unused_channel_name(wanted: &str, existing: &[String]) -> String {
    if !existing.iter().any(|name| name == wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|suffix| format!("{} {}", wanted, suffix))
        .find(|name| !existing.contains(name))
        .unwrap_or_else(|| wanted.to_string())
}

/// Find one channel by the pair of names that identifies it.
///
/// By name because that is all a `Batch` carries, and all a `ChannelRef` is.
fn find_channel<'a>(
    devices: &'a mut [AppDevice],
    device_name: &str,
    channel_name: &str,
) -> Option<&'a mut AppChannel> {
    devices
        .iter_mut()
        .find(|device| device.name == device_name)?
        .channels
        .iter_mut()
        .find(|channel| channel.name == channel_name)
}

/// Convert an acquired timestamp to the plot's x axis: seconds from a
/// reference taken when the interface started.
///
/// The axis is driven by one clock throughout — `Utc`, the same one the
/// datapoints carry — so the sliding viewport and the points inside it cannot
/// drift apart. It does mean the axis inherits the wall clock's habits; a
/// machine that steps its clock mid-run would step the plot with it.
fn seconds_since(reference: DateTime<Utc>, moment: DateTime<Utc>) -> f64 {
    (moment - reference).num_microseconds().unwrap_or(0) as f64 / 1_000_000.0
}

/// The logo, rasterised for the window's own icon.
///
/// Rendered from the SVG rather than from a PNG kept beside it: a second copy
/// of a logo is a copy that goes stale, and resvg is already here to draw the
/// marks in the title bar.
///
/// The full mark, drawn at 64 pixels. The plain one is for the sizes below
/// that, and the file icon carries both — see `build.rs`.
///
/// `None` rather than an error if anything goes wrong. A window with the
/// system's default icon is a window; refusing to open over it would not be.
fn window_icon() -> Option<iced::window::Icon> {
    const SIZE: u32 = 64;

    let tree = resvg::usvg::Tree::from_data(LOGO, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIZE, SIZE)?;

    let scale = SIZE as f32 / tree.size().width();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny_skia paints in premultiplied alpha and an icon wants it straight,
    // which shows on the notch's antialiased edge where alpha is partial.
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for pixel in pixmap.pixels() {
        let colour = pixel.demultiply();
        rgba.extend_from_slice(&[colour.red(), colour.green(), colour.blue(), colour.alpha()]);
    }

    iced::window::icon::from_rgba(rgba, SIZE, SIZE).ok()
}

pub fn main() -> iced::Result {
    // Before anything can panic, so that a crash in the interface leaves
    // something to read rather than only a window that is no longer there.
    logbook::catch_panics();

    iced::application(AppDaq::new, AppDaq::update, AppDaq::view)
        .window(window::Settings {
            // The size the design is drawn at, so a screenshot and the mockup
            // can be laid over each other without either being scaled first.
            // Only a starting size - the window is still resizable.
            size: iced::Size::new(1252.0, 839.0),
            icon: window_icon(),
            ..window::Settings::default()
        })
        .subscription(AppDaq::subscription)
        .theme(AppDaq::theme)
        .scale_factor(AppDaq::scale_factor)
        .font(iced_fonts::LUCIDE_FONT_BYTES)
        .font(BRAND_FONT)
        .run()
}

struct AppChannel {
    name: String,
    unit: String,
    /// The most recent value acquired, once any has been.
    latest: Option<f64>,
    samples: usize,
}
struct AppDevice {
    name: String,
    channels: Vec<AppChannel>,
    expanded: bool,
    kind: DeviceKind,
    /// Whether it is talking to its hardware, once anything has tried.
    ///
    /// `None` before a run: not knowing is a third state, and drawing a red
    /// dot for it would say the device is broken when nothing has looked.
    connected: Option<bool>,
    /// What it is complaining about, if anything.
    ///
    /// Set from two places: a run, where the device says so as it reads, and
    /// a stream test, which listens to a stopped device on purpose. The plain
    /// connection check never sets it - it opens the device and lets go
    /// without reading, so there is nothing for it to find.
    concern: Option<String>,
}

impl AppDevice {
    /// The one answer the dot beside the name is drawn from.
    fn health(&self) -> Health {
        match (self.connected, self.concern.is_some()) {
            (None, _) => Health::Unknown,
            (Some(false), _) => Health::Down,
            (Some(true), true) => Health::Troubled,
            (Some(true), false) => Health::Fine,
        }
    }
}

/// A channel being dragged towards a plot.
///
/// Two kinds because the two modes name a channel differently: a live one by
/// the device and channel it is read from, a recorded one by where it sits in
/// the tree of runs, which is what `plot_recorded` needs to fetch it.
#[derive(Clone)]
enum Dragged {
    Live(ChannelRef),
    Recorded(usize, usize, usize),
}

/// What the window is being used for.
///
/// Two jobs that want the same three panes arranged the same way but filled
/// with different things: setting a rig up and watching it read, or picking
/// through what was recorded earlier. A mode rather than a second window,
/// because the panes, the plots and the settings panel are the same furniture
/// either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Configure the rig and record from it.
    Record,
    /// Look at runs already in the results file.
    View,
}

/// One recorded run, and what is under it.
///
/// Held rather than read on every frame: the tree is drawn constantly and the
/// database is on disk. Only the shape is loaded - runs, devices, channels and
/// their names - which is small however long the run was. Readings are the
/// expensive part and are fetched when something asks to draw them.
struct ViewRun {
    id: i64,
    started: DateTime<Utc>,
    expanded: bool,
    devices: Vec<ViewDevice>,
}

struct ViewDevice {
    name: String,
    /// The backend configuration this device was recorded with, as json.
    ///
    /// Kept as it was written. It is what lets a results file describe its own
    /// setup, so a run can be read long after the rig it came from changed.
    hardware: String,
    expanded: bool,
    channels: Vec<ViewChannel>,
}

struct ViewChannel {
    id: i64,
    name: String,
    unit: String,
    /// How many readings it holds.
    ///
    /// Counted when the tree is read rather than when a channel is looked at,
    /// because it is one indexed count per channel and the panel would
    /// otherwise have to run a query while drawing, which happens far more
    /// often than the answer changes.
    readings: usize,
}

/// Which devices a check is about.
///
/// Checking one rather than all of them is not an optimisation. Opening a
/// serial port asserts DTR, which resets an Arduino and most boards like it,
/// so re-checking the whole rig because somebody picked a port from a list
/// would reset every other board on the bench.
#[derive(Clone, PartialEq)]
enum Checking {
    Everything,
    /// One device, by the name the answers come back under.
    Just(String),
}

/// What a check found about one device.
struct Answered {
    device: String,
    connected: bool,
    /// Why it did not answer, where it did not.
    why: Option<String>,
}

/// What a whole check found, or why there was nothing to ask.
///
/// The error is for a rig that will not even build — a channel pointing
/// somewhere it should not, say. Worth saying: answering that with an empty
/// list, as the first version of this did, made a broken configuration and a
/// rig where every device is fine look exactly alike.
type Checked = Result<Vec<Answered>, String>;

/// The verdict on one device's settings, and which device it was about.
///
/// The error is a message rather than the error itself, because it has to
/// cross a thread and `Error` is not `Send`.
type Tested = (usize, Result<StreamCheck, String>);

/// Which of the two formula fields the help was opened from.
///
/// The operators and functions are the same in both. What differs is where the
/// values come from, which is the part somebody is actually stuck on.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FormulaHelp {
    /// A channel's scale, converting one measurement.
    Scale,
    /// A calculated channel's equation, combining several.
    Equation,
}

impl FormulaHelp {
    fn title(self) -> &'static str {
        match self {
            FormulaHelp::Scale => "Writing a scale",
            FormulaHelp::Equation => "Writing an equation",
        }
    }

    /// The help itself, as markdown.
    ///
    /// Written as one document rather than assembled out of text widgets. The
    /// first attempt was the latter, and the sizes and spacings of a dozen
    /// separate pieces drifted apart until the page read as a list of fragments
    /// - which is exactly what a markup language exists to stop.
    fn text(self) -> &'static str {
        match self {
            FormulaHelp::Scale => SCALE_HELP,
            FormulaHelp::Equation => EQUATION_HELP,
        }
    }
}

/// Everything below the first heading is common to both, and deliberately
/// repeated rather than shared: the two documents want to read as one page
/// each, and stitching a shared tail onto a different head is how the seam
/// starts to show.
const SCALE_HELP: &str = r#"### What you have to work with

The measurement is written `x`.

Where the formula names constants — a shunt resistance, a sensor's range —
those are available by name as well, and stay editable beside the formula
rather than dissolving into the arithmetic.

### Examples

```
x * 5 + 5
(x - 4) / 16 * 29
(x / shunt_ohms) * 1000
```

### Operators

`+`  `-`  `*`  `/`  `%`  `^`  and brackets.

`^` raises to a power, so `x ^ 2` is x squared, and `%` is the remainder after
division. Precedence is the usual one, so `2 + 3 * 4` is 14.

### Functions

`sqrt`  `abs`  `round`  `floor`  `ceil`  `ln`  `log10`  `exp`  `sin`  `cos`  `tan`  `asin`  `acos`  `atan`  `min(a, b)`  `max(a, b)`  `pow(a, b)`  `log(x, base)`  `hypot(a, b)`  `atan2(y, x)`

Angles are in radians. For a power, `x ^ 2` reads better than `pow(x, 2)`.

### Numbers

Decimals and scientific notation both: `1.5e3` is 1500, and `1e-3` is 0.001.

### What is checked

A formula that will not parse, or that uses a name with nothing behind it, is
refused as you type, and the message says which names were available.

One that is sound but gives an infinite or undefined answer for a particular
reading has that reading left out and reported, rather than recorded.

### What it will not do

One expression giving one number. There are no comparisons, no conditionals,
and no variables of your own."#;

const EQUATION_HELP: &str = r#"### What you have to work with

The inputs listed below the equation, by the short names you gave them.

That is what the short names are for: channel names have spaces in them, and
quoting those inside a formula is miserable.

### Examples

```
high - low
sqrt(dp) * 12.7
(a + b) / 2
```

### Operators

`+`  `-`  `*`  `/`  `%`  `^`  and brackets.

`^` raises to a power, so `x ^ 2` is x squared, and `%` is the remainder after
division. Precedence is the usual one, so `2 + 3 * 4` is 14.

### Functions

`sqrt`  `abs`  `round`  `floor`  `ceil`  `ln`  `log10`  `exp`  `sin`  `cos`  `tan`  `asin`  `acos`  `atan`  `min(a, b)`  `max(a, b)`  `pow(a, b)`  `log(x, base)`  `hypot(a, b)`  `atan2(y, x)`

Angles are in radians. For a power, `x ^ 2` reads better than `pow(x, 2)`.

### Numbers

Decimals and scientific notation both: `1.5e3` is 1500, and `1e-3` is 0.001.

### What is checked

A formula that will not parse, or that uses a name with nothing behind it, is
refused as you type, and the message says which names were available.

One that is sound but gives an infinite or undefined answer for a particular
reading has that reading left out and reported, rather than recorded.

### What it will not do

One expression giving one number. There are no comparisons, no conditionals,
and no variables of your own."#;

/// Which device in the setup a row of the tree stands for.
///
/// Calculated channels are a device in the results and in the config, but they
/// are not one in `DaqConfig::devices`: lumberdaq keeps them in a field of
/// their own because they own no hardware, are not connected to, and are not
/// read on a thread. So a position in the tree stops meaning a position in
/// `devices` the moment they appear, and the tree carries which kind it is
/// rather than leaving it to arithmetic on the last index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    /// A measured device, by where it sits in `DaqConfig::devices`.
    Measured(usize),
    /// The calculated device. There is at most one, so it needs no index.
    Calculated,
}

/// One channel drawn on a plot, and the colour it is drawn in.
///
/// A reference rather than a copy: the readings belong to the channel on its
/// device, and `ChannelRef` is how lumberdaq itself names one channel of one
/// device — the same pair of names a `Batch` arrives carrying. The colour
/// lives here so the legend and the trace cannot disagree about it.
struct PlottedChannel {
    reference: ChannelRef,
    /// The recorded channel this trace was read from, when it came out of a
    /// results file rather than off a rig.
    ///
    /// A live trace is identified by its `ChannelRef`, which is what a batch
    /// carries. A recorded one cannot be: device and channel names repeat in
    /// every run of the same rig, so run 1's "Value" and run 3's "Value" have
    /// the same reference and different data. The database's own id is the
    /// only thing that tells them apart.
    source: Option<i64>,
    colour: Color,
    /// The trace this channel is drawn as on this plot. A channel on two plots
    /// has a series on each, which is why this lives here and not on the
    /// channel itself.
    series: ShapeId,
}

/// A Pico input range, named as somebody reading a datasheet would name it.
///
/// A wrapper because the range itself belongs to picolog and cannot be given a
/// `Display` here, and because "±2500 mV" is what the setting means where
/// `MilliVolts2500` is only what it is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PicoRange(pico_hrdl::VoltageRange);

impl std::fmt::Display for PicoRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(match self.0 {
            pico_hrdl::VoltageRange::MilliVolts2500 => "±2500 mV",
            pico_hrdl::VoltageRange::MilliVolts1250 => "±1250 mV",
            pico_hrdl::VoltageRange::MilliVolts625 => "±625 mV",
            pico_hrdl::VoltageRange::MilliVolts313 => "±313 mV",
            pico_hrdl::VoltageRange::MilliVolts156 => "±156 mV",
            pico_hrdl::VoltageRange::MilliVolts78 => "±78 mV",
            pico_hrdl::VoltageRange::MilliVolts39 => "±39 mV",
        })
    }
}

/// Every Pico range, widest first, as they are offered.
const PICO_RANGES: [PicoRange; 7] = [
    PicoRange(pico_hrdl::VoltageRange::MilliVolts2500),
    PicoRange(pico_hrdl::VoltageRange::MilliVolts1250),
    PicoRange(pico_hrdl::VoltageRange::MilliVolts625),
    PicoRange(pico_hrdl::VoltageRange::MilliVolts313),
    PicoRange(pico_hrdl::VoltageRange::MilliVolts156),
    PicoRange(pico_hrdl::VoltageRange::MilliVolts78),
    PicoRange(pico_hrdl::VoltageRange::MilliVolts39),
];

/// The spans an NI analog input is usually asked for, smallest last.
///
/// A pair rather than a single number because the config takes a span, and one
/// that is not symmetric is perfectly legal.
const NI_RANGES: [(f64, f64); 5] =
    [(-10.0, 10.0), (-5.0, 5.0), (-2.0, 2.0), (-1.0, 1.0), (-0.2, 0.2)];

/// An NI span, written the way a range is written.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NiRange((f64, f64));

impl std::fmt::Display for NiRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "{} to {} V", self.0 .0, self.0 .1)
    }
}

/// The speeds a serial device is likely to be running at.
const BAUD_RATES: [u32; 8] = [9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600];

/// How long to listen to a port before deciding the settings do not read it.
///
/// Longer than it sounds like it needs to be, and for a reason: opening a port
/// asserts DTR, which resets an Arduino, and its bootloader takes a second or
/// two before the sketch says anything at all. A shorter patience would hear
/// only the silence after the reset and blame the baud rate for it.
///
/// Costs nothing when the settings are right - the test returns the moment one
/// frame reads - so this is only how long a failure takes to be sure of.
const STREAM_TEST_PATIENCE: Duration = Duration::from_secs(5);

/// How often a streaming mock device produces a sample, to choose between.
const MOCK_INTERVALS: [u64; 9] = [1, 2, 5, 10, 20, 50, 100, 500, 1000];

/// Which of a mock device's ways of sampling is in use.
///
/// The variant without its settings, so it can be offered as a choice: an
/// `Acquisition` carries the interval, and picking a mode should not mean
/// picking an interval at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockMode {
    Polled,
    Streaming,
}

impl std::fmt::Display for MockMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(match self {
            MockMode::Polled => "Polled",
            MockMode::Streaming => "Streaming",
        })
    }
}

/// Which kind of value a mock channel generates, without the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockInputKind {
    Random,
    Constant,
    Sine,
}

impl std::fmt::Display for MockInputKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(match self {
            MockInputKind::Random => "Random",
            MockInputKind::Constant => "Constant",
            MockInputKind::Sine => "Sine",
        })
    }
}

impl MockInputKind {
    fn of(input: &MockHardwareInput) -> MockInputKind {
        match input {
            MockHardwareInput::Random => MockInputKind::Random,
            MockHardwareInput::Constant(_) => MockInputKind::Constant,
            MockHardwareInput::Sine { .. } => MockInputKind::Sine,
        }
    }

    /// The number this kind carries, where it carries one.
    fn number(input: &MockHardwareInput) -> Option<f64> {
        match input {
            MockHardwareInput::Random => None,
            MockHardwareInput::Constant(value) => Some(*value),
            MockHardwareInput::Sine { frequency_hz } => Some(*frequency_hz),
        }
    }

    /// What that number is called, for the field's label.
    fn number_label(&self) -> Option<&'static str> {
        match self {
            MockInputKind::Random => None,
            MockInputKind::Constant => Some("Value"),
            MockInputKind::Sine => Some("Frequency (Hz)"),
        }
    }
}

/// Where the pointer was last seen, in window coordinates.
///
/// Outside the state, and deliberately: `on_right_press` says only that a
/// right click happened, never where, and a menu that floats over the
/// interface has to be put somewhere. Held in the interface's own state it
/// would need a message per mouse movement, and iced rebuilds the whole widget
/// tree for every message — so moving the mouse would have cost more than
/// running the acquisition does.
///
/// A `Mutex` rather than a `thread_local!` because subscriptions run on the
/// executor, which by default is a thread pool: the writer and the reader are
/// not promised to be the same thread. One uncontended lock per mouse movement
/// is nothing next to a relayout.
static POINTER: Mutex<Point> = Mutex::new(Point::ORIGIN);

/// Whether something is being dragged.
///
/// Beside `POINTER` and for the same reason: `listen_with` takes a bare `fn`,
/// so anything it needs to know has to be reachable without being captured.
/// A plain flag rather than a lock, because it is read on every mouse movement
/// and written twice a drag.
static DRAGGING: AtomicBool = AtomicBool::new(false);

/// Where the pointer is, for whoever needs to put something there.
///
/// A poisoned lock would mean a panic while holding it, which cannot happen
/// here: nothing between the lock and the unlock can fail. Recovering the
/// value is still better than joining the panic.
fn pointer() -> Point {
    match POINTER.lock() {
        Ok(seen) => *seen,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Follow the pointer without telling the interface about it.
///
/// Always `None`: returning a message here is exactly what this exists to
/// avoid. The right click itself still arrives the ordinary way, through the
/// `on_right_press` on the row that was clicked, and reads the position back
/// out of `POINTER` once.
fn watch_pointer(event: Event, _status: Status, _window: window::Id) -> Option<Message> {
    match event {
        Event::Mouse(mouse::Event::CursorMoved { position }) => {
            if let Ok(mut seen) = POINTER.lock() {
                *seen = position;
            }
            // A message only while something is being dragged, and then only
            // so the interface redraws with the label in its new place. This
            // is the cost that was taken out of ordinary pointer tracking,
            // paid back for the moment somebody is actually dragging - which
            // is the one time a rebuild per movement buys anything.
            match DRAGGING.load(Ordering::Relaxed) {
                true => Some(Message::DragMoved),
                false => None,
            }
        }
        // The one thing here worth a message. Taken from the subscription
        // rather than from a `MouseArea` because a release lands wherever the
        // pointer is, and whatever is under it - a button, a plot's own canvas
        // - takes the event first. This sees it either way.
        //
        // One message per click, which is a rebuild per click. A drag has to
        // end somewhere, and the alternative is watching every movement.
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::PointerReleased)
        }
        _ => None,
    }
}

/// What a right click was on, while its menu is open.
///
/// Says what the menu acts on. Where it is drawn is `context_at`, taken from
/// `POINTER` at the moment of the click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenu {
    Device(usize),
    Channel(usize, usize),
    Plot(usize),
    /// The calculated device. There is at most one, so it needs no index.
    Calculated,
    /// A calculated channel, by its position in the calculated device.
    CalculatedChannel(usize),
    /// A channel of a recorded run, by run, device and channel.
    RunChannel(usize, usize, usize),
    /// A recorded run.
    Run(usize),
    /// A device of a recorded run.
    RunDevice(usize, usize),
}

impl ContextMenu {
    /// The channel this menu is about, where it is about one.
    ///
    /// What "add to plot" acts on. A device or a plot has no answer here,
    /// which is what keeps the entry off their menus.
    fn plottable(&self) -> bool {
        matches!(
            self,
            ContextMenu::Channel(..)
                | ContextMenu::CalculatedChannel(_)
                | ContextMenu::RunChannel(..)
                // A run or a device stands for every channel under it, which
                // is a perfectly good thing to put on a plot.
                | ContextMenu::Run(_)
                | ContextMenu::RunDevice(..)
        )
    }
}

/// What is being configured in the settings panel.
///
/// Only plots so far; devices and channels are the same idea and will join it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Selection {
    Plot(usize),
    /// The settings that apply to every plot rather than to one of them.
    AllPlots,
    /// One of the viewer's plots, which are a different list from the
    /// recording ones and so a different thing to have selected.
    ViewPlot(usize),
    /// A recorded run.
    Run(usize),
    /// A device in a recorded run, by the run and its place in it.
    RunDevice(usize, usize),
    /// A channel of a device in a recorded run.
    RunChannel(usize, usize, usize),
    Device(usize),
    /// A channel, by the device it is on and its position in that device.
    Channel(usize, usize),
    /// The calculated device itself.
    Calculated,
    /// A calculated channel, by its position in the calculated device.
    CalculatedChannel(usize),
}

/// Which channels a plot draws, and what it is called.
struct AppPlot {
    /// As written in the layout file. Kept rather than taken from the position
    /// in the list, so saving does not renumber somebody's plots behind their
    /// back when one in the middle is deleted.
    number: usize,
    name: String,
    channels: Vec<PlottedChannel>,
    /// None until the plot has a channel on it: there is no such thing as a
    /// plot of nothing, and `PlotWidgetBuilder` will not build one.
    widget: Option<PlotWidget>,
}

impl AppPlot {
    /// Put a channel on this plot.
    ///
    /// The trace is added to the widget rather than the whole plot being
    /// rebuilt, so the channels already on it keep the data they have
    /// collected. A plot that had nothing on it has no widget yet, so this is
    /// where it gets one.
    fn add_channel(&mut self, reference: ChannelRef, window_seconds: f64) {
        if self.channels.iter().any(|plotted| plotted.reference == reference) {
            return;
        }

        let colour = self.next_colour();
        let series = new_series(&reference, colour);
        let id = series.id;

        match self.widget.as_mut() {
            Some(widget) => {
                if widget.add_series(series).is_err() {
                    return;
                }
            }
            None => {
                let plot = build_plot(
                    self.number,
                    self.name.clone(),
                    vec![reference.clone()],
                    window_seconds,
                );
                self.widget = plot.widget;
                self.channels = plot.channels;
                return;
            }
        }

        self.channels.push(PlottedChannel { reference, source: None, colour, series: id });
    }

    /// Draw a channel read out of a results file.
    ///
    /// Unlike a live channel, the data is all here at once and never grows, so
    /// the trace is filled as it is added and the viewport is fitted to what
    /// arrived rather than slid along by the clock.
    ///
    /// `at` is the run's start, so a trace begins at zero seconds whichever
    /// run it came from — which is what makes two runs of the same rig
    /// comparable when they are put on one plot.
    fn add_recorded(
        &mut self,
        source: i64,
        reference: ChannelRef,
        readings: &[DataPoint],
        at: DateTime<Utc>,
    ) {
        if self.channels.iter().any(|plotted| plotted.source == Some(source)) {
            return;
        }

        let colour = self.next_colour();
        let positions: Vec<[f64; 2]> = readings
            .iter()
            .map(|point| [seconds_since(at, point.datetime), point.value])
            .collect();

        match self.widget.is_some() {
            true => {
                let mut series = new_series(&reference, colour);
                let id = series.id;
                series.positions = positions;

                let Some(widget) = self.widget.as_mut() else { return };
                if widget.add_series(series).is_err() {
                    return;
                }
                self.channels.push(PlottedChannel {
                    reference,
                    source: Some(source),
                    colour,
                    series: id,
                });
            }
            // A plot with nothing on it has no widget: `PlotWidgetBuilder`
            // refuses to build one with no series, so there is no empty plot to
            // add to. It is built *with* this channel and then filled, which is
            // what `add_channel` does for a live one.
            false => {
                let plot =
                    build_plot(self.number, self.name.clone(), vec![reference.clone()], 0.0);
                self.widget = plot.widget;
                self.channels = plot.channels;

                let Some(plotted) = self.channels.first_mut() else { return };
                plotted.source = Some(source);
                let id = plotted.series;

                let Some(widget) = self.widget.as_mut() else { return };
                // Replaced rather than appended: every series is seeded with a
                // point far off the left of the axis so the builder has
                // something to build, and a live trace trims it on the first
                // batch. Recorded data arrives all at once, so it takes the
                // seed's place.
                // Borrowed, not moved: `update_series` takes an `FnMut`, so
                // the closure cannot consume what it captures.
                let _ = widget.update_series(&id, |trace| {
                    trace.positions.clear();
                    trace.positions.extend_from_slice(&positions);
                });
            }
        }

        self.fit_to_data();
    }

    /// Set the viewport to cover everything drawn on this plot.
    ///
    /// Recorded data does not arrive over time, so there is nothing to slide:
    /// the useful view is the whole of it. A single instant is widened to a
    /// second, since a viewport of zero width has nothing to draw in it.
    fn fit_to_data(&mut self) {
        let Some(widget) = self.widget.as_mut() else { return };

        let mut lowest = f64::MAX;
        let mut highest = f64::MIN;
        for plotted in self.channels.iter() {
            let _ = widget.update_series(&plotted.series, |trace| {
                for position in trace.positions.iter() {
                    lowest = lowest.min(position[0]);
                    highest = highest.max(position[0]);
                }
            });
        }

        if lowest > highest {
            return;
        }
        match highest - lowest {
            span if span > 0.0 => widget.set_x_lim(lowest, highest),
            _ => widget.set_x_lim(lowest, lowest + 1.0),
        }
    }

    /// Take a channel off this plot, by its position in the list.
    fn remove_channel(&mut self, position: usize) {
        if position >= self.channels.len() {
            return;
        }

        let plotted = self.channels.remove(position);
        if let Some(widget) = self.widget.as_mut() {
            let _ = widget.remove_series(&plotted.series);
        }

        // A plot of nothing is not a plot: back to the state a new one starts
        // in, rather than an empty set of axes.
        if self.channels.is_empty() {
            self.widget = None;
        }
    }

    /// A colour no trace on this plot is already using, where there is one.
    ///
    /// Going by the count alone would hand out a colour already on screen once
    /// a channel in the middle has been removed.
    fn next_colour(&self) -> Color {
        PALETTE
            .iter()
            .find(|colour| {
                !self.channels.iter().any(|plotted| plotted.colour == **colour)
            })
            .copied()
            .unwrap_or(PALETTE[self.channels.len() % PALETTE.len()])
    }
}

/// A line in the run log: what happened, and when.
struct LogEntry {
    at: DateTime<Utc>,
    text: String,
}

/// How many log lines to keep. A device that is failing every read would
/// otherwise fill memory with the same complaint.
const LOG_LIMIT: usize = 200;

// Fixed regions, not the freely-splittable tiling pane_grid also supports —
// resizable dividers only, no drag/split/close wired up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneKind {
    Devices,
    Config,
    Data,
    /// Connections, errors, and anything else the run has to say. Separate
    /// from Config because none of it is a setting: it is what happened.
    Log,
}

struct AppDaq {
    devices: Vec<AppDevice>,
    plots: Vec<AppPlot>,
    /// What the window is being used for at the moment.
    mode: Mode,
    /// The plots the viewer draws, and how they are arranged.
    ///
    /// Its own list rather than the recording plots reused: a layout set up to
    /// watch a rig read is not one for picking through a finished run, and
    /// loading a run into the live plots would destroy the first to make the
    /// second. Switching modes then swaps both back intact.
    view_plots: Vec<AppPlot>,
    view_panes: pane_grid::State<usize>,
    /// The runs in the open project's results file, as last read.
    ///
    /// Empty until the viewer is opened, and re-read when it is: a run
    /// recorded since would otherwise not be there to look at.
    runs: Vec<ViewRun>,
    /// The channel being dragged, from the press that started it until the
    /// button comes back up.
    ///
    /// Armed by the same press that selects a channel, rather than by a
    /// handler of its own: `MouseArea` offers an event to its child first and
    /// gives up if the child took it, and these rows are buttons, which take
    /// their own left presses. A press that never reaches a plot simply
    /// selects, which is what a click is.
    dragging: Option<Dragged>,
    /// When the plots last changed enough to need drawing more than once.
    ///
    /// A plot widget publishes what it worked out - its ticks, its camera - as
    /// a message, and can only do so while something is asking it to draw. An
    /// interface that stops asking the moment nothing is moving therefore
    /// leaves a plot half told about itself, with gridlines and no numbers
    /// against them. A short spell of frames after anything changes is enough
    /// for that conversation to finish.
    plots_settling: Option<Instant>,
    /// The plot the pointer is over, while something is being dragged.
    ///
    /// Kept by each plot saying when the pointer arrives and when it leaves,
    /// rather than by comparing the pointer against every plot's rectangle:
    /// the widgets know where they are and iced will tell us, so there is no
    /// geometry to get wrong.
    over_plot: Option<usize>,
    /// Whether the open context menu is showing its list of plots.
    ///
    /// Beside the menu rather than replacing it, so the thing being added and
    /// the place it is going are both on screen at once.
    plot_menu: bool,
    /// Whether the settings dialog is up.
    settings_open: bool,
    /// The formula help, and which field asked for it.
    formula_help: Option<FormulaHelp>,
    /// That help, parsed. Done when the dialog opens rather than while drawing:
    /// the text never changes, and parsing it sixty times a second to show the
    /// same words would be work for nothing.
    help_items: Vec<markdown::Item>,
    /// The log as it is kept on disk, beside the one on screen.
    ///
    /// The pane holds the last couple of hundred lines and loses them when the
    /// window closes; this one is what somebody can send afterwards.
    logbook: logbook::Logbook,
    /// What this person likes, as opposed to what this project is.
    ///
    /// Loaded at startup and written back as soon as anything changes: it is a
    /// tiny file and the changes are rare, so there is nothing here worth the
    /// debounce the project files need.
    settings: UserSettings,
    panes: pane_grid::State<PaneKind>,
    /// The project directory open at the moment, and None when none is.
    ///
    /// Everything that gets saved is saved into it, so with nothing open there
    /// is nothing to save to and the interface says so rather than writing
    /// somewhere arbitrary.
    project: Option<PathBuf>,
    /// Where each plot sits, by plot number. A grid of its own inside the
    /// Data pane, so plots can be arranged against each other without being
    /// draggable over the sidebar.
    plot_panes: pane_grid::State<usize>,
    /// The rig as configured. Kept so a run can be built again after being
    /// stopped: `Daq::from_config` consumes one, and the last one went with
    /// the thread that is now finishing.
    config: DaqConfig,
    /// The run in progress, or what is left of one that is stopping. None once
    /// a stopped run has been reaped.
    acquisition: Option<Acquisition>,
    run: RunState,
    /// Oldest first, read downwards like a terminal. The pane is anchored to
    /// its bottom, so the newest line is the one in view.
    log: Vec<LogEntry>,
    /// Whether the log pane is open, and where its divider was when it last
    /// was — so opening it again returns it to where it was dragged to rather
    /// than to a constant.
    log_open: bool,
    log_ratio: f32,
    /// How much of the recent past every plot shows, from the saved layout.
    window_seconds: f64,
    /// Every channel the setup measures, in the form a plot names one. What
    /// the settings panel offers when adding a channel to a plot.
    /// What an equation may read: measured channels only.
    ///
    /// Two lists rather than one because the two questions differ. A
    /// calculated channel's output goes to the sink rather than back through
    /// the calculator, so it cannot feed another equation — but it is very
    /// much something to plot.
    available: Vec<ChannelRef>,
    /// What a plot may draw: every channel there is.
    plottable: Vec<ChannelRef>,
    /// What the settings panel is showing.
    selected: Option<Selection>,
    /// The open right click menu, if one is.
    context: Option<ContextMenu>,
    /// Whether the file menu under the app name is open.
    file_menu: bool,
    /// Where the pointer was when the open menu was asked for.
    ///
    /// Taken once, at the click. Drawing at the live cursor instead would have
    /// the menu trail the pointer around the window.
    context_at: iced::Point,
    /// The serial ports found last time anybody looked.
    ///
    /// Held rather than asked for while drawing: listing them asks the
    /// operating system, and `view` runs every frame. Refreshed when a list is
    /// about to be useful — a serial device selected, or one being added.
    ports: Vec<serial_stream::PortOption>,
    /// The NI devices the driver reported last time anybody looked. Empty on a
    /// machine with no NI software, which is the ordinary case.
    ni_devices: Vec<String>,
    /// How many analog inputs the selected device has, where the hardware has
    /// said. None when it has not, which is why the lists below fall back to
    /// offering everything the backend allows rather than nothing at all.
    ni_inputs: Option<usize>,
    pico_inputs: Option<u16>,
    /// A check of every device, while it is on its way.
    connection_check: Option<Receiver<Checked>>,
    /// A baud rate being tried against a device, while it is on its way.
    connection_test: Option<Receiver<Tested>>,
    /// A test asked for while the hardware was busy with another check.
    wanted_test: Option<usize>,
    /// What the last test made of a device's settings, and which device.
    ///
    /// One at a time, because one device is configured at a time. Kept with
    /// its index so a verdict cannot be shown against a device it was not
    /// about.
    tested: Option<(usize, StreamCheck)>,
    /// A check asked for while that one was still running.
    ///
    /// Trying ports one after another asks for a check faster than the
    /// hardware can answer, and dropping the later ones leaves the dot
    /// describing a port that is no longer selected.
    wanted_check: Option<Checking>,
    /// An answer being fetched from a Pico unit, while it is on its way.
    ///
    /// The fetch opens the unit over USB, which takes about a second, so it
    /// happens on a thread of its own and the answer arrives here rather than
    /// holding up the click that asked for it.
    pico_probe: Option<Receiver<Result<Option<u16>, String>>>,
    /// What the selected device turned out to be — `USB-6001`, `ADC-24` —
    /// where the hardware could say. Never written to the config: it describes
    /// what is plugged in now, not what the project is.
    model: Option<String>,
    /// The type chosen in the add-device dialog, and whether it is open at
    /// all. A device cannot be added without one, since the type decides what
    /// the rest of its settings even are.
    adding_device: Option<Option<&'static str>>,
    /// The number field of the selected channel, as it is being typed.
    ///
    /// Held rather than formatted afresh each time for two reasons. A
    /// `text_input` borrows its value for as long as the widget lives, so it
    /// cannot come from a local. And a number being typed passes through "",
    /// "1" and "1." on the way to "1.5": keeping the text means the field says
    /// what was typed while the config only takes it once it parses.
    number_draft: String,
    /// When the layout last changed, while it is still waiting to be written.
    /// None once it is saved.
    dirty_since: Option<Instant>,
    /// The same, for the rig's own configuration.
    rig_dirty_since: Option<Instant>,
    /// Where the plots' x axis has its zero.
    reference: DateTime<Utc>,
}

#[derive(Debug, Clone)]
enum Message {
    ThemeChanged(Theme),
    FontStepChanged(u8),
    LastProjectOpened,
    ModeChosen(Mode),
    RunsRefreshed,
    ConnectionsChecked,
    /// Try the current settings against the device on this row.
    StreamTested(usize),
    /// Windows says something was plugged in or unplugged.
    DevicesChanged,
    ToggleRun(usize),
    ToggleRunDevice(usize, usize),
    /// The pointer entered or left a plot, while something is being dragged.
    PlotHovered(usize),
    PlotLeft(usize),
    /// The pointer moved while dragging. Carries nothing: the position is
    /// read from `POINTER` when drawing, and this only says to draw again.
    DragMoved,
    /// The left button came back up, wherever it was.
    PointerReleased,
    /// Open or shut every row of whichever tree is on screen.
    ExpandAll(bool),
    /// Show the list of plots the open menu's channel could go on.
    PlotMenuOpened,
    /// Put the open menu's channel on the plot at this position.
    PlotChosen(usize),
    RunSelected(usize),
    RunDeviceSelected(usize, usize),
    RunChannelSelected(usize, usize, usize),
    RecordedChannelRemoved(usize, usize),
    SettingsOpened,
    SettingsClosed,
    FormulaHelpOpened(FormulaHelp),
    FormulaHelpClosed,
    AddDeviceOpened,
    AddDeviceTypeChosen(&'static str),
    AddDeviceConfirmed,
    AddDeviceCancelled,
    DeviceDeleted(usize),
    ChannelDeleted(usize, usize),
    AddPlot,
    AllPlotsSelected,
    HistoryChanged(u64),
    PlotRenamed(usize, String),
    PlotDeleted(usize),
    ChannelAddedToPlot(usize, ChannelRef),
    ChannelRemovedFromPlot(usize, usize),
    ToggleDevice(usize),
    DeviceSelected(usize),
    ContextOpened(ContextMenu),
    ContextDismissed,
    FileMenuToggled,
    Exported,
    ProjectOpened,
    /// Take the devices out of another configuration file into this project.
    DevicesImported,
    ProjectCreated,
    DeviceRenamed(usize, String),
    DeviceIntervalChanged(usize, u64),
    ChannelSelected(usize, usize),
    CalculatedSelected,
    CalculatedRenamed(String),
    CalculatedDeleted,
    CalculatedChannelAdded,
    CalculatedChannelDeleted(usize),
    CalculatedChannelRenamed(usize, String),
    CalculatedChannelUnitEdited(usize, String),
    CalculatedEquationEdited(usize, String),
    CalculatedInputAdded(usize),
    CalculatedInputRenamed(usize, String, String),
    CalculatedInputSourceChosen(usize, String, ChannelRef),
    CalculatedInputDeleted(usize, String),
    CalculatedChannelSelected(usize),
    ChannelRenamed(usize, usize, String),
    ChannelUnitChanged(usize, usize, String),
    ScaleEdited(usize, usize, String),
    ChannelAdded(usize),
    SerialIndexEdited(usize, usize, String),
    PicoChannelChosen(usize, usize, u16),
    PicoRangeChosen(usize, usize, PicoRange),
    PicoSingleEnded(usize, usize, bool),
    NiChannelChosen(usize, usize, u32),
    NiRangeChosen(usize, usize, NiRange),
    NiSingleEnded(usize, usize, bool),
    PortsRefreshed,
    NiDevicesRefreshed,
    SerialPortChosen(usize, String),
    SerialBaudChosen(usize, u32),
    SerialPatternEdited(usize, String),
    NiDeviceEdited(usize, String),
    MockModeChanged(usize, MockMode),
    MockIntervalChanged(usize, u64),
    MockInputChanged(usize, usize, MockInputKind),
    MockNumberEdited(usize, usize, String),
    LogToggled,
    RunStarted,
    RunStopped,
    RecordPressed,
    PaneResized(pane_grid::ResizeEvent),
    PlotsResized(pane_grid::ResizeEvent),
    PlotClicked(pane_grid::Pane),
    PlotDragged(pane_grid::DragEvent),
    Plot(usize, PlotUiMessage),
    Tick,
}

impl AppDaq {
    fn new() -> Self {
        // Read before the interface exists, so the first frame is already
        // drawn the way this person asked for.
        let (settings, complaint) = UserSettings::load();
        let (logbook, no_log) = logbook::Logbook::open();

        // Nothing open yet. An empty rig is a perfectly good rig - no devices,
        // no plots - which is what lets the rest of the interface be written as
        // though a project were always there. The one thing genuinely missing
        // is a directory to save into, and that is what `project` being None
        // says.
        let mut app = AppDaq {
            project: None,
            devices: Vec::new(),
            plots: Vec::new(),
            settings,
            logbook,
            mode: Mode::Record,
            view_plots: vec![empty_plot(1)],
            view_panes: pane_grid::State::with_configuration(Configuration::Pane(1)),
            runs: Vec::new(),
            // Something to draw at once: the first frames are where every
            // plot on screen works out its axes.
            plots_settling: Some(Instant::now()),
            dragging: None,
            over_plot: None,
            plot_menu: false,
            settings_open: false,
            formula_help: None,
            help_items: Vec::new(),
            panes: default_panes(),
            plot_panes: pane_grid::State::with_configuration(Configuration::Pane(1)),
            config: empty_config(),
            acquisition: None,
            run: RunState::Stopped,
            log: Vec::new(),
            log_open: false,
            log_ratio: LOG_OPEN,
            window_seconds: DEFAULT_HISTORY_SECONDS as f64,
            available: Vec::new(),
            plottable: Vec::new(),
            selected: None,
            context: None,
            file_menu: false,
            context_at: iced::Point::ORIGIN,
            ports: serial_stream::available_ports(),
            // Not asked for at startup: loading the NI driver on a machine that
            // has one is slower than listing serial ports, and a project with
            // no NI device in it should never touch it.
            ni_devices: Vec::new(),
            ni_inputs: None,
            pico_inputs: None,
            connection_check: None,
            wanted_check: None,
            connection_test: None,
            wanted_test: None,
            tested: None,
            pico_probe: None,
            model: None,
            adding_device: None,
            number_draft: String::new(),
            // Nothing to write: there is nothing open to have changed.
            dirty_since: None,
            rig_dirty_since: None,
            reference: Utc::now(),
        };

        // A settings file that would not read is worth saying out loud, and
        // this is the first moment there is a log to say it in.
        if let Some(problem) = complaint {
            app.note(problem);
        }
        // Said on screen, because it is the one thing the file cannot report:
        // that there is no file.
        if let Some(problem) = no_log {
            app.note(problem);
        }

        // A project named on the command line opens straight away. Without one
        // the interface comes up asking for a folder, which is the only state
        // in which there is nothing to show.
        if let Some(named) = std::env::args().nth(1) {
            app.open_project(PathBuf::from(named));
        }
        app
    }

    /// Open a project directory, replacing whatever was open before.
    ///
    /// Everything belonging to the last project goes with it: its rig, its
    /// plots, their arrangement, and the run reading it. A config that will not
    /// read leaves the interface as it was and says so, rather than half
    /// opening onto a project that is not there.
    fn open_project(&mut self, directory: PathBuf) {
        let project = Project::new(&directory);

        let config = match project.read_config() {
            Ok(config) => config,
            Err(error) => {
                self.note(format!("could not open {}: {}", directory.display(), error));
                return;
            }
        };

        self.stop_acquisition();

        let known = config.available_inputs();
        // What a plot may draw, which is not the same list: a calculated
        // channel is not an equation input but is very much something to look
        // at. Filtering the saved plots by the input list would quietly drop
        // every calculated trace and then save the loss.
        let plottable = config.all_channels();
        let mut notes: Vec<String> = Vec::new();

        let (window_seconds, plots, saved_layout) = match project.read_layout() {
            Ok(Some(saved)) => {
                let window = saved.history_seconds as f64;

                // Asked once, of the library, rather than worked out per plot
                // here: whether a layout and a rig agree is the same question
                // however it is being looked at.
                for reference in saved.dangling(&config) {
                    notes.push(format!(
                        "{} names {}, which this setup does not have",
                        plot_config::FILE,
                        reference
                    ));
                }

                let plots = saved
                    .plots
                    .iter()
                    .map(|plot| {
                        // Skipped rather than refused: a plot pointing at
                        // something that is gone should not stop the rest being
                        // drawn, which is how choptui treats it too.
                        let drawable: Vec<ChannelRef> = plot
                            .channels
                            .iter()
                            .filter(|reference| plottable.contains(reference))
                            .cloned()
                            .collect();

                        build_plot(plot.number, plot.display_name(), drawable, window)
                    })
                    .collect();

                notes.push(format!("layout read from {}", plot_config::FILE));
                (window, plots, saved.layout)
            }
            Ok(None) => {
                // No saved layout: everything measured on one plot, which is
                // at least a view of the whole rig to start from.
                notes.push(format!("no {}, showing every channel on one plot", plot_config::FILE));
                let window = DEFAULT_HISTORY_SECONDS as f64;
                (
                    window,
                    vec![build_plot(1, "Plot 1".to_string(), plottable.clone(), window)],
                    None,
                )
            }
            Err(problem) => {
                notes.push(problem.to_string());
                let window = DEFAULT_HISTORY_SECONDS as f64;
                (
                    window,
                    vec![build_plot(1, "Plot 1".to_string(), plottable.clone(), window)],
                    None,
                )
            }
        };

        // The saved arrangement where there is one, pruned to the plots that
        // exist, with anything it does not mention stacked on afterwards. A
        // plot added from the terminal has never been arranged, and should
        // still appear.
        let numbers: Vec<usize> = plots.iter().map(|plot| plot.number).collect();
        let arranged = saved_layout
            .as_ref()
            .and_then(|layout| configuration_from(layout, &numbers))
            .unwrap_or_else(|| {
                stacked(&numbers).unwrap_or(Configuration::Pane(numbers.first().copied().unwrap_or(1)))
            });

        let mut plot_panes = pane_grid::State::with_configuration(arranged);
        let placed: Vec<usize> = plot_panes.iter().map(|(_, number)| *number).collect();
        for number in numbers.iter().filter(|number| !placed.contains(number)) {
            let last = plot_panes.iter().map(|(pane, _)| *pane).last();
            if let Some(last) = last {
                plot_panes.split(Axis::Horizontal, last, *number);
            }
        }

        self.devices = devices_from(&config);
        self.available = known;
        self.plottable = plottable;
        self.plots = plots;
        self.plot_panes = plot_panes;
        self.window_seconds = window_seconds;
        self.selected = None;
        self.context = None;
        self.reference = Utc::now();
        // What is on screen is what the files say, so there is nothing owed to
        // disk for the project just opened.
        self.dirty_since = None;
        self.rig_dirty_since = None;
        self.project = Some(directory.clone());
        self.settings.set_last_project(&directory);
        self.remember_settings();

        // Opened stopped, not reading. A rig is nearly always opened to be
        // looked at or changed, and configuration is locked while a run is on,
        // so starting one for you gets in the way of the thing you came to do.
        // Play is one press away when reading is what you wanted.
        self.config = config;
        self.set_plot_panning(true);
        // Once, so the axes open on a sensible range rather than on the seed
        // point each series is built with.
        self.slide_viewport();
        self.settle_plots();

        // Before anything is asked of the rig, so the dots say what is
        // there rather than nothing until somebody presses play.
        self.check_connections();

        self.note(format!("project {}", directory.display()));
        // After the project is named, so a complaint about it is not the first
        // thing in the log with nothing yet saying which project it is about.
        self.note_address_clashes();
        for note in notes {
            self.note(note);
        }
    }

    /// Begin reading the project that is open.
    ///
    /// Nothing to read without one: a run records beside its project, so there
    /// is no sensible place for one to happen with nothing open.
    fn start_run(&mut self) {
        let Some(directory) = self.project.clone() else {
            self.note("no project open to read".to_string());
            return;
        };

        self.acquisition = Some(start_acquisition(self.config.clone(), directory));
        self.run = RunState::Starting;
        self.set_plot_panning(false);
        self.note("connecting to the devices".to_string());
    }

    /// What the interface shows before a project has been chosen.
    ///
    /// The same two actions the file menu offers, put where they cannot be
    /// missed: with nothing open, finding the menu is the only thing there is
    /// to do, so the dialog does it instead.
    fn welcome(&self) -> Element<'_, Message> {
        container(
            column![
                text("Project").size(22),
                text("Create or open an existing project config.json file.").size(13),
                row![
                    button(text("Open a project").size(14))
                        .padding(8)
                        .on_press(Message::ProjectOpened),
                    button(text("New project").size(14))
                        .style(button::secondary)
                        .padding(8)
                        .on_press(Message::ProjectCreated),
                ]
                .spacing(8),
            ]
            .spacing(16)
            // The last project, when there still is one. Checked rather than
            // trusted: a folder that has been moved, renamed or emptied since
            // is not worth offering, and finding that out by failing to open
            // it would be a worse way to say so.
            .extend(self.reopenable().map(|(directory, name)| {
                column![
                    text("Last opened").size(12),
                    button(text(name).size(14))
                        .style(button::success)
                        .padding(8)
                        .on_press(Message::LastProjectOpened),
                    text(directory).size(11),
                ]
                .spacing(4)
                .into()
            }))
            // `extend` over an Option, because an Option is an iterator of at
            // most one thing and iced 0.14's Column has no `push_maybe`.
            .extend(self.log.last().map(|entry| {
                // The log pane is behind this layer and cannot be read, so a
                // refused folder would otherwise report into somewhere
                // invisible. Only the last line, because the only thing worth
                // saying here is why the last attempt did not take.
                text(entry.text.clone())
                    .size(12)
                    .style(|theme: &Theme| text::Style {
                        color: Some(theme.extended_palette().danger.base.color),
                    })
                    .into()
            }))
            .width(460),
        )
        .padding(36)
        .style(dialog_style)
        .into()
    }

    /// The last project, if it is still one, as its name and its path.
    ///
    /// The name alone is what somebody recognises; the path is there because
    /// two rigs called "test" in different places are not unusual.
    fn reopenable(&self) -> Option<(String, String)> {
        let directory = self.settings.last_project.as_ref()?;
        if !Project::new(directory).config_path().exists() {
            return None;
        }

        let name = directory
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| directory.display().to_string());

        Some((directory.display().to_string(), name))
    }

    /// Where a folder picker should start.
    ///
    /// Beside whatever is open, since the next project is usually a sibling of
    /// the last one. The working directory otherwise, which is where a project
    /// made from the command line would be.
    fn pick_from(&self) -> PathBuf {
        self.project
            .as_ref()
            .and_then(|open| open.parent().map(Path::to_path_buf))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The project open at the moment, where one is.
    fn project(&self) -> Option<Project> {
        self.project.as_ref().map(Project::new)
    }

    /// End the run reading the project being closed.
    ///
    /// Asked to stop rather than waited for: the thread holds only its own
    /// devices and a channel nobody will read again, and blocking until it
    /// noticed would freeze the window in the middle of opening something.
    fn stop_acquisition(&mut self) {
        if let Some(acquisition) = self.acquisition.take() {
            acquisition.recording.store(false, Ordering::Relaxed);
            acquisition.stop.store(true, Ordering::Relaxed);
        }
        self.run = RunState::Stopped;
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::ThemeChanged(theme) => {
                self.settings.set_theme(&theme);
                self.remember_settings();
            }
            Message::FontStepChanged(step) => {
                self.settings.font_step = step;
                self.remember_settings();
            }
            Message::LastProjectOpened => {
                if let Some(directory) = self.settings.last_project.clone() {
                    self.open_project(directory);
                }
            }
            Message::ModeChosen(mode) => {
                self.mode = mode;
                // The plots coming into view have not been drawn since
                // whatever changed while they were hidden.
                self.settle_plots();
                // Re-read on the way in rather than once at startup: a run
                // recorded since the last look is exactly what somebody
                // switching to the viewer wants to see.
                if mode == Mode::View {
                    self.load_runs();
                }
            }
            Message::RunsRefreshed => self.load_runs(),
            Message::ConnectionsChecked => self.check_connections(),
            Message::StreamTested(index) => self.test_stream(index),
            Message::DevicesChanged => {
                // Said whether or not it leads to a check, because "something
                // was plugged in" is itself worth having in a log: it dates a
                // cable being knocked, which is otherwise only visible as
                // readings that stop.
                self.note("something was plugged in or unplugged".to_string());

                // What changed is not said in terms a rig would recognise, so
                // the rig is asked rather than guessed at. Not while running:
                // the run owns the devices, and it already reports one that
                // goes away.
                if self.run == RunState::Stopped {
                    self.check_connections();
                }
            }
            Message::ToggleRun(run) => {
                if let Some(run) = self.runs.get_mut(run) {
                    run.expanded = !run.expanded;
                }
            }
            Message::ToggleRunDevice(run, device) => {
                if let Some(device) =
                    self.runs.get_mut(run).and_then(|run| run.devices.get_mut(device))
                {
                    device.expanded = !device.expanded;
                }
            }
            Message::DragMoved => {}
            Message::PlotHovered(plot) => self.over_plot = Some(plot),
            Message::PlotLeft(plot) => {
                // Only if it is still the one we think we are over: leaving one
                // plot for its neighbour arrives as the neighbour's enter and
                // then this, and clearing blindly would forget where we are.
                if self.over_plot == Some(plot) {
                    self.over_plot = None;
                }
            }
            Message::PointerReleased => {
                DRAGGING.store(false, Ordering::Relaxed);

                // Both taken before either is looked at. Clearing the target
                // first is how the last version of this dropped every channel
                // into nowhere.
                let dragged = self.dragging.take();
                let target = self.over_plot.take();

                let Some(dragged) = dragged else { return };
                let Some(plot) = target else { return };

                match dragged {
                    Dragged::Live(reference) => self.add_to_plot(plot, reference),
                    Dragged::Recorded(run, device, channel) => {
                        self.plot_recorded(run, device, channel, plot)
                    }
                }
            }
            Message::ExpandAll(open) => {
                self.context = None;

                // Whichever tree is showing. The other one keeps whatever it
                // was left as, which is what somebody switching back expects.
                match self.mode {
                    Mode::Record => {
                        for device in self.devices.iter_mut() {
                            device.expanded = open;
                        }
                    }
                    Mode::View => {
                        for run in self.runs.iter_mut() {
                            run.expanded = open;
                            for device in run.devices.iter_mut() {
                                device.expanded = open;
                            }
                        }
                    }
                }
            }
            Message::PlotMenuOpened => self.plot_menu = true,
            Message::PlotChosen(plot) => {
                let target = self.context.take();
                self.plot_menu = false;

                match target {
                    Some(ContextMenu::RunChannel(run, device, channel)) => {
                        self.plot_recorded(run, device, channel, plot);
                    }
                    // Everything under it, in the order it is listed. Each is
                    // read and added the same way one would be, so a channel
                    // already on the plot is skipped rather than doubled.
                    Some(ContextMenu::RunDevice(run, device)) => {
                        for channel in self.channels_under(run, Some(device)) {
                            self.plot_recorded(channel.0, channel.1, channel.2, plot);
                        }
                    }
                    Some(ContextMenu::Run(run)) => {
                        for channel in self.channels_under(run, None) {
                            self.plot_recorded(channel.0, channel.1, channel.2, plot);
                        }
                    }
                    Some(ContextMenu::Channel(device, channel)) => {
                        if let Some(reference) = self.live_reference(device, channel) {
                            self.add_to_plot(plot, reference);
                        }
                    }
                    Some(ContextMenu::CalculatedChannel(channel)) => {
                        if let Some(reference) = self.calculated_reference(channel) {
                            self.add_to_plot(plot, reference);
                        }
                    }
                    _ => {}
                }
            }
            Message::RunSelected(run) => {
                self.context = None;
                self.selected = Some(Selection::Run(run));
            }
            Message::RunDeviceSelected(run, device) => {
                self.context = None;
                self.selected = Some(Selection::RunDevice(run, device));
            }
            Message::RunChannelSelected(run, device, channel) => {
                self.context = None;
                self.selected = Some(Selection::RunChannel(run, device, channel));
                self.start_drag(Dragged::Recorded(run, device, channel));
            }
            Message::RecordedChannelRemoved(plot, position) => {
                if let Some(plot) = self.view_plots.get_mut(plot) {
                    plot.remove_channel(position);
                    plot.fit_to_data();
                }
            }
            Message::SettingsOpened => {
                self.file_menu = false;
                self.settings_open = true;
            }
            Message::SettingsClosed => self.settings_open = false,
            Message::FormulaHelpOpened(which) => {
                // Reparsed on each opening rather than held from launch. It is
                // a few microseconds against a dialog somebody is about to
                // read, and it keeps one copy of the text rather than two.
                self.help_items = markdown::parse(which.text()).collect();
                self.formula_help = Some(which);
            }
            Message::FormulaHelpClosed => {
                self.formula_help = None;
                self.help_items = Vec::new();
            }
            Message::AddDeviceOpened => {
                // Looked up now rather than while drawing, and now is when it
                // matters: something may have been plugged in since startup.
                self.ports = serial_stream::available_ports();
                self.ni_devices = lumberdaq::hardware::ni_daqmx::available_devices();
                self.adding_device = Some(None);
            }
            Message::AddDeviceCancelled => self.adding_device = None,
            Message::AddDeviceTypeChosen(kind) => self.adding_device = Some(Some(kind)),
            Message::AddDeviceConfirmed => {
                let Some(Some(kind)) = self.adding_device else { return };

                // There is only ever one, so this is a create rather than an
                // add: lumberdaq holds it as an `Option`, not a list, because
                // every calculated channel can live in the same one and
                // grouping them into several would be decoration.
                if kind == CALCULATED_TYPE {
                    self.config.calculated = Some(lumberdaq::calculated::CalculatedDevice {
                        info: lumberdaq::device::DeviceInfo {
                            name: unused_name("Calculated", &self.config),
                        },
                        channels: Vec::new(),
                    });
                    // Appended, not rebuilt: it goes last in the tree, it
                    // has no channels yet, and rebuilding would take every
                    // other device's live readings down with it.
                    let name = match self.config.calculated.as_ref() {
                        Some(calculated) => calculated.info.name.clone(),
                        None => return,
                    };
                    self.devices.push(AppDevice {
                        name,
                        channels: Vec::new(),
                        expanded: false,
                        kind: DeviceKind::Calculated,
                        connected: None,
                        concern: None,
                    });

                    self.adding_device = None;
                    self.selected = Some(Selection::Calculated);
                    self.rig_changed();
                    return;
                }

                let Some(mut hardware) = HardwareConfig::of_type(kind) else { return };

                // A serial device starts on the first USB port there is, which
                // is nearly always the instrument somebody just plugged in.
                // The backend leaves the port empty rather than guessing; this
                // is the interface making the guess, where it can be seen and
                // changed in the dropdown right next to it.
                if let HardwareConfig::SerialStream(serial) = &mut hardware {
                    if let Some(port) = self.ports.iter().find(|port| port.usb) {
                        serial.port = port.name.clone();
                    }
                }

                // Named for what it is until somebody says otherwise, and made
                // unique so two new devices are not both "New device" — the
                // name is how a batch finds its way back to a channel.
                let name = unused_name("New device", &self.config);

                self.config.devices.push(lumberdaq::config::DeviceConfig {
                    info: lumberdaq::device::DeviceInfo { name: name.clone() },
                    read_interval_ms: lumberdaq::config::default_read_interval_ms(),
                    hardware,
                });
                // Inserted rather than pushed: the calculated device sits at
                // the end of the tree, and a new measured device belongs
                // before it, where its index into `config.devices` says.
                let at = self.config.devices.len() - 1;
                self.devices.insert(
                    at,
                    AppDevice {
                        name,
                        channels: Vec::new(),
                        expanded: false,
                        kind: DeviceKind::Measured(at),
                        connected: None,
                        concern: None,
                    },
                );

                self.adding_device = None;
                // Selected so its settings are there to fill in: a device with
                // no channels is not finished being made.
                self.selected = Some(Selection::Device(self.config.devices.len() - 1));
                self.rig_changed();
            }
            Message::DeviceDeleted(index) => {
                self.context = None;
                if index < self.config.devices.len() {
                    let device = self.config.devices.remove(index);
                    self.devices.remove(index);
                    // Its channels cannot be plotted any more, so they come off
                    // the plots rather than being left as traces that will
                    // never receive another point.
                    self.forget_device_on_plots(&device.info.name);

                    self.note(format!("deleted {}", device.info.name));
                    self.selected = None;
                    self.rig_changed();
                }
            }
            Message::ChannelDeleted(device, channel) => {
                self.context = None;
                let Some(device_config) = self.config.devices.get_mut(device) else { return };
                let device_name = device_config.info.name.clone();
                let Some(info) = device_config.hardware.channel_info(channel).cloned() else {
                    return;
                };

                if device_config.hardware.remove_channel(channel) {
                    if let Some(app_device) = self.devices.get_mut(device) {
                        app_device.channels.remove(channel);
                    }
                    self.forget_channel_on_plots(&ChannelRef {
                        device: device_name,
                        channel: info.name.clone(),
                    });

                    self.note(format!("deleted {}", info.name));
                    self.selected = Some(Selection::Device(device));
                    self.rig_changed();
                }
            }
            Message::PlotClicked(pane) if self.mode == Mode::View => {
                self.context = None;
                let number = self.view_panes.get(pane).copied();
                if let Some(index) = number
                    .and_then(|number| self.view_plots.iter().position(|p| p.number == number))
                {
                    self.selected = Some(Selection::ViewPlot(index));
                }
            }
            Message::PlotClicked(pane) => {
                self.context = None;
                // The pane knows its plot's number; the settings panel works
                // in positions, so it is looked up rather than assumed equal.
                let number = self.plot_panes.get(pane).copied();
                if let Some(index) =
                    number.and_then(|number| {
                        self.plots.iter().position(|plot| plot.number == number)
                    })
                {
                    self.selected = Some(Selection::Plot(index));
                }
            }
            Message::PlotDragged(pane_grid::DragEvent::Dropped { pane, target })
                if self.mode == Mode::View =>
            {
                // The viewer's arrangement is its own and is not written to
                // the project, so there is nothing to mark dirty.
                self.view_panes.drop(pane, target);
            }
            Message::PlotDragged(pane_grid::DragEvent::Dropped { pane, target }) => {
                self.plot_panes.drop(pane, target);
                self.layout_changed();
            }
            // Picked up and put back, or let go somewhere that is not a pane.
            // Nothing has moved, so there is nothing to save.
            Message::PlotDragged(_) => {}
            Message::PlotsResized(pane_grid::ResizeEvent { split, ratio })
                if self.mode == Mode::View =>
            {
                self.view_panes.resize(split, ratio);
                self.settle_plots();
            }
            Message::PlotsResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.plot_panes.resize(split, ratio);
                self.settle_plots();
                // The arrangement is part of the layout, so dragging a divider
                // is a change to be saved like renaming a plot is.
                self.layout_changed();
            }
            // Whichever set of plots is on screen. Adding one to the recording
            // plots while looking at recorded data would put it where it could
            // not be seen.
            Message::AddPlot if self.mode == Mode::View => {
                let number =
                    self.view_plots.iter().map(|plot| plot.number).max().unwrap_or(0) + 1;
                self.view_plots.push(empty_plot(number));
                split_last(&mut self.view_panes, number);
                self.settle_plots();

                self.selected = Some(Selection::ViewPlot(self.view_plots.len() - 1));
            }
            Message::AddPlot => {
                // Past the highest in use rather than the length, so deleting
                // plot 2 of three and adding one does not reuse the number.
                let number =
                    self.plots.iter().map(|plot| plot.number).max().unwrap_or(0) + 1;
                let name = format!("Plot {}", number);
                self.plots.push(build_plot(number, name, Vec::new(), self.window_seconds));
                split_last(&mut self.plot_panes, number);
                self.settle_plots();

                // A new widget comes with the default bindings, which is the
                // wrong answer mid-run.
                let stopped = self.run == RunState::Stopped;
                self.set_plot_panning(stopped);

                // Selected so it can be configured: an empty plot is no use
                // until something is put on it.
                self.selected = Some(Selection::Plot(self.plots.len() - 1));
                self.layout_changed();
            }
            Message::AllPlotsSelected => {
                self.selected = Some(Selection::AllPlots);
            }
            Message::HistoryChanged(seconds) => {
                // Taken up by the viewport on the next frame and by the trim
                // on the next batch, so there is nothing to apply to each plot
                // here. A longer window does not bring back points already
                // trimmed: the traces grow into it as data arrives.
                self.window_seconds = seconds as f64;
                // Applied here as well as by the frame tick, because stopped
                // there is no frame tick to apply it and the setting would
                // look broken until the next run.
                self.slide_viewport();
                self.layout_changed();
            }
            Message::PlotRenamed(index, name) => {
                if let Some(plot) = self.plots.get_mut(index) {
                    plot.name = name;
                    self.layout_changed();
                }
            }
            Message::ChannelAddedToPlot(index, reference) => {
                let window = self.window_seconds;
                if let Some(plot) = self.plots.get_mut(index) {
                    plot.add_channel(reference, window);
                    // The first channel on a plot builds its widget, which
                    // arrives with the default bindings.
                    let stopped = self.run == RunState::Stopped;
                    self.set_plot_panning(stopped);
                    self.layout_changed();
                }
            }
            Message::ChannelRemovedFromPlot(index, position) => {
                if let Some(plot) = self.plots.get_mut(index) {
                    plot.remove_channel(position);
                    self.layout_changed();
                }
            }
            Message::PlotDeleted(index) if self.mode == Mode::View => {
                self.context = None;
                if index >= self.view_plots.len() {
                    return;
                }

                let plot = self.view_plots.remove(index);
                let pane = self
                    .view_panes
                    .iter()
                    .find(|(_, number)| **number == plot.number)
                    .map(|(pane, _)| *pane);

                if let Some(pane) = pane {
                    self.view_panes.close(pane);
                }
                self.selected = None;
            }
            Message::PlotDeleted(index) => {
                self.context = None;
                if index < self.plots.len() {
                    let plot = self.plots.remove(index);

                    // Closing its pane hands the space back to its neighbour,
                    // which is what makes the arrangement close up rather than
                    // leave a gap where the plot was.
                    // Found into a binding first: on this edition the borrow in
                    // an `if let` scrutinee lasts for the whole block, so
                    // closing inside one would still be holding the read.
                    let pane = self
                        .plot_panes
                        .iter()
                        .find(|(_, number)| **number == plot.number)
                        .map(|(pane, _)| *pane);

                    if let Some(pane) = pane {
                        self.plot_panes.close(pane);
                    }

                    self.note(format!("deleted {}", plot.name));
                    // The panel was showing a plot that no longer exists, and
                    // every index after this one has moved.
                    self.selected = None;
                    self.layout_changed();
                }
            }
            Message::ToggleDevice(index) => {
                if let Some(device) = self.devices.get_mut(index) {
                    device.expanded = !device.expanded;
                }
            }
            // Any press puts an open menu away. It fires alongside whatever
            // was actually clicked rather than instead of it, which is what
            // makes clicking straight through to something else work: the menu
            // closes and the thing under the cursor still gets its click.
            Message::ContextDismissed => {
                self.context = None;
                self.file_menu = false;
            }
            Message::FileMenuToggled => self.file_menu = !self.file_menu,
            Message::ProjectOpened => {
                self.file_menu = false;
                // Blocking, and deliberately: the picker is modal, so the
                // window behind it is meant to be waiting. Doing it without an
                // async runtime is worth more here than not blocking during a
                // dialog nobody can see past.
                // The file rather than the folder it is in. A folder
                // picker hides files, so every candidate folder looks alike
                // and there is no way to see which one holds a project.
                // Pointing at the config says which, and the project is the
                // folder around it.
                let Some(config) = rfd::FileDialog::new()
                    .set_title("Open project: choose its config.json")
                    .add_filter("Project", &["json"])
                    .set_directory(self.pick_from())
                    .pick_file()
                else {
                    return;
                };

                match config.parent() {
                    Some(directory) => self.open_project(directory.to_path_buf()),
                    // A path with no parent is a bare file name, which a
                    // picker does not produce, but saying so beats unwrapping.
                    None => self.note(format!("{} is not inside a folder", config.display())),
                }
            }
            Message::DevicesImported => {
                self.file_menu = false;

                // A change to the rig like any other, so it waits for the run
                // to stop rather than rebuilding the tree under a thread that
                // is reading from it.
                if !self.rig_editable() {
                    self.note("stop the run before adding devices".to_string());
                    return;
                }

                let Some(path) = rfd::FileDialog::new()
                    .set_title("Add devices from a configuration")
                    .add_filter("Configuration", &["json"])
                    .set_directory(self.pick_from())
                    .pick_file()
                else {
                    return;
                };

                let library = match read_configuration_file(&path) {
                    Ok(library) => library,
                    Err(problem) => {
                        self.note(format!("could not read {}: {}", path.display(), problem));
                        return;
                    }
                };

                let report = self.config.merge(library);

                for name in report.devices.iter() {
                    self.note(format!("added device {}", name));
                }
                for name in report.calculated.iter() {
                    self.note(format!("added calculated channel {}", name));
                }
                // Said one by one rather than counted. The whole point of the
                // rule is that somebody can see what did not come across and
                // why, and a number cannot say why.
                for (name, reason) in report.skipped.iter() {
                    self.note(format!("skipped {}: {}", name, reason));
                }
                if report.took_nothing() && report.skipped.is_empty() {
                    self.note(format!("nothing to add from {}", path.display()));
                }

                if !report.took_nothing() {
                    self.reload_devices();
                    self.rig_changed();
                    // A library device may name a port something here is
                    // already on, which no name check would catch.
                    self.note_address_clashes();
                }
            }
            Message::ProjectCreated => {
                self.file_menu = false;
                let Some(directory) = rfd::FileDialog::new()
                    .set_title("New project: choose an empty folder")
                    .set_directory(self.pick_from())
                    .pick_folder()
                else {
                    return;
                };

                // Refused rather than merged: opening a folder that is already
                // a project is what Load is for, and writing a fresh config
                // over one would throw a rig away.
                let project = Project::new(&directory);
                if project.config_path().exists() {
                    self.note(format!(
                        "{} is already a project - open it rather than creating one",
                        directory.display()
                    ));
                    return;
                }

                let named = directory
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();

                let fresh = DaqConfig { info: DaqInfo { name: named, ..empty_config().info }, ..empty_config() };
                match lumberdaq::configuration::write_configuration_file(
                    &project.config_path(),
                    &fresh,
                ) {
                    Ok(()) => {
                        self.note(format!("created {}", project.config_path().display()));
                        self.open_project(directory);
                    }
                    Err(error) => self.note(format!("could not create the project: {}", error)),
                }
            }
            Message::Exported => {
                self.file_menu = false;

                let Some(project) = self.project() else {
                    self.note("no project open to export".to_string());
                    return;
                };

                // Read only, and skipping any run already written, so pressing
                // this twice costs nothing and cannot touch the results.
                match lumberdaq::export::export(
                    &project.database_path(),
                    &project.export_path(),
                ) {
                    Ok(report) => self.note(match report.written.len() {
                        0 => "nothing new to export".to_string(),
                        written => format!(
                            "exported {} run(s) to {}",
                            written,
                            project.export_path().display()
                        ),
                    }),
                    Err(error) => self.note(format!("could not export: {}", error)),
                }
            }
            Message::ContextOpened(target) => {
                // Right clicking the same thing again puts the menu away,
                // which is the way out of one without choosing from it.
                self.context = match self.context == Some(target) {
                    true => None,
                    false => {
                        self.context_at = pointer();
                        self.plot_menu = false;
                        Some(target)
                    }
                };
            }
            Message::DeviceSelected(index) => {
                // Refreshed on selecting a serial device, since its port list
                // is about to be shown and something may have been unplugged.
                self.context = None;
                // Asked once, on selection, rather than while drawing: every
                // one of these talks to a driver.
                self.model = None;
                match self.config.devices.get(index).map(|device| &device.hardware) {
                    Some(HardwareConfig::SerialStream(serial)) => {
                        self.ports = serial_stream::available_ports();
                        // Whatever the port says it is, which is as close to a
                        // model as a serial device gets: the thing on the other
                        // end is only a stream of bytes.
                        self.model = self
                            .ports
                            .iter()
                            .find(|port| port.name == serial.port)
                            .map(|port| port.product.clone())
                            .filter(|product| !product.is_empty());
                    }
                    Some(HardwareConfig::NiDaqmx(ni)) => {
                        self.ni_devices = lumberdaq::hardware::ni_daqmx::available_devices();
                        // How many inputs it has decides both which channels
                        // can be chosen and which one a differential pair
                        // takes, and only the device knows.
                        self.ni_inputs = lumberdaq::hardware::ni_daqmx::input_count(&ni.device);
                        self.model = lumberdaq::hardware::ni_daqmx::product_type(&ni.device);
                    }
                    // Nothing asked of the unit here. Selecting a device shows
                    // its name and its read interval, and opening a Pico to
                    // fill those in costs a second: `Hrdl::open` loads the
                    // driver and initialises the unit over USB, on this thread,
                    // while the click waits. The one thing the unit knows that
                    // the config does not — how many inputs it has — belongs to
                    // the channel panel, which is where it is asked for.
                    _ => {}
                }
                self.selected = Some(Selection::Device(index));
            }
            Message::CalculatedRenamed(name) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let was = std::mem::replace(&mut calculated.info.name, name.clone());

                if let Some(device) = self.calculated_row() {
                    device.name = name.clone();
                }
                // The same follow-through a measured device gets: a plot names
                // a channel by its device, so a rename would leave every trace
                // of it pointing at something that is not there.
                self.rename_device_on_plots(&was, &name);
                self.rig_changed();
            }
            Message::CalculatedDeleted => {
                self.context = None;
                let Some(calculated) = self.config.calculated.take() else { return };

                // Its channels go off the plots with it, the same as a measured
                // device's do: a trace named after something the setup no
                // longer has is a trace that never receives anything again.
                self.forget_device_on_plots(&calculated.info.name);
                self.devices.retain(|device| device.kind != DeviceKind::Calculated);
                self.selected = None;
                self.note(format!("deleted {}", calculated.info.name));
                self.rig_changed();
            }
            Message::CalculatedChannelAdded => {
                self.context = None;
                let Some(calculated) = self.config.calculated.as_mut() else { return };

                let existing: Vec<String> =
                    calculated.channels.iter().map(|channel| channel.info.name.clone()).collect();

                // Empty rather than guessed. An equation with nothing in it is
                // plainly unfinished, where one that says `0` looks done and
                // records zeroes.
                calculated.channels.push(lumberdaq::calculated::CalculatedChannel {
                    info: lumberdaq::channel::ChannelInfo {
                        name: unused_channel_name("New channel", &existing),
                        unit: String::new(),
                        scale: None,
                    },
                    inputs: std::collections::BTreeMap::new(),
                    equation: String::new(),
                });

                let last = calculated.channels.len() - 1;
                let added = calculated.channels[last].info.clone();

                // Appended rather than rebuilt from the config. Rebuilding is
                // the easy way to keep the tree honest, and it throws away
                // everything the tree knows that the config does not: which
                // devices are open, and every channel's live reading and
                // sample count. Mid-run those are the point of the tree.
                if let Some(device) = self.calculated_row() {
                    device.channels.push(AppChannel {
                        name: added.name,
                        unit: added.unit,
                        latest: None,
                        samples: 0,
                    });
                    device.expanded = true;
                }

                self.selected = Some(Selection::CalculatedChannel(last));
                self.rig_changed();
            }
            Message::CalculatedChannelDeleted(channel) => {
                self.context = None;
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                if channel >= calculated.channels.len() {
                    return;
                }

                let gone = calculated.channels.remove(channel).info.name;
                let reference = ChannelRef {
                    device: calculated.info.name.clone(),
                    channel: gone.clone(),
                };

                if let Some(device) = self.calculated_row() {
                    device.channels.remove(channel);
                }
                // Off the plots as well, or they keep a trace named after
                // something the setup no longer has.
                self.forget_channel_on_plots(&reference);

                // Whatever was selected is at best a different channel now.
                self.selected = Some(Selection::Calculated);
                self.note(format!("deleted {}", gone));
                self.rig_changed();
            }
            Message::CalculatedChannelRenamed(channel, name) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let device_name = calculated.info.name.clone();
                let Some(target) = calculated.channels.get_mut(channel) else { return };
                let was = std::mem::replace(&mut target.info.name, name.clone());

                if let Some(row) =
                    self.calculated_row().and_then(|device| device.channels.get_mut(channel))
                {
                    row.name = name.clone();
                }
                self.rename_channel_on_plots(&device_name, &was, &name);
                self.rig_changed();
            }
            Message::CalculatedChannelUnitEdited(channel, unit) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let Some(target) = calculated.channels.get_mut(channel) else { return };
                target.info.unit = unit.clone();

                // The unit is shown on the reading beside the channel, so it
                // comes along or the tree keeps showing the old one.
                if let Some(row) =
                    self.calculated_row().and_then(|device| device.channels.get_mut(channel))
                {
                    row.unit = unit;
                }
                self.rig_changed();
            }
            Message::CalculatedEquationEdited(channel, equation) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let Some(target) = calculated.channels.get_mut(channel) else { return };
                // Kept whatever it says. An equation half typed is not valid
                // and saying so is the panel's job, not a reason to refuse the
                // keystroke.
                target.equation = equation;
                self.rig_changed();
            }
            Message::CalculatedInputAdded(at) => {
                let first = self.available.first().cloned();
                let Some(source) = first else {
                    self.note("no measured channels to read from yet".to_string());
                    return;
                };

                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let Some(channel) = calculated.channels.get_mut(at) else { return };

                let name = unused_variable(&channel.inputs);
                channel.inputs.insert(name, source);
                self.rig_changed();
            }
            Message::CalculatedInputRenamed(at, from, to) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let Some(channel) = calculated.channels.get_mut(at) else { return };

                // Refused rather than applied: a map takes one value per key,
                // so renaming onto a name already there would quietly swallow
                // the other input. Empty is allowed through, because it is
                // what clearing the field to retype it looks like, and an
                // equation referring to nothing is what `validate` reports.
                if !to.is_empty() && channel.inputs.contains_key(&to) {
                    return;
                }
                let Some(source) = channel.inputs.remove(&from) else { return };
                channel.inputs.insert(to, source);
                self.rig_changed();
            }
            Message::CalculatedInputSourceChosen(at, name, source) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let Some(channel) = calculated.channels.get_mut(at) else { return };
                channel.inputs.insert(name, source);
                self.rig_changed();
            }
            Message::CalculatedInputDeleted(at, name) => {
                let Some(calculated) = self.config.calculated.as_mut() else { return };
                let Some(channel) = calculated.channels.get_mut(at) else { return };
                channel.inputs.remove(&name);
                self.rig_changed();
            }
            Message::CalculatedSelected => {
                self.context = None;
                self.model = None;
                self.selected = Some(Selection::Calculated);
            }
            Message::CalculatedChannelSelected(channel) => {
                self.context = None;
                self.selected = Some(Selection::CalculatedChannel(channel));
                if let Some(reference) = self.calculated_reference(channel) {
                    self.start_drag(Dragged::Live(reference));
                }
            }
            Message::ChannelSelected(device, channel) => {
                self.context = None;
                self.selected = Some(Selection::Channel(device, channel));
                self.number_draft = self.channel_number(device, channel);
                if let Some(reference) = self.live_reference(device, channel) {
                    self.start_drag(Dragged::Live(reference));
                }

                // This panel is the one place the unit knows something the
                // config does not: how many inputs it has, and so which
                // channel numbers are real.
                if matches!(
                    self.config.devices.get(device).map(|device| &device.hardware),
                    Some(HardwareConfig::PicoHrdl(_))
                ) {
                    self.probe_pico();
                }
            }
            Message::ChannelAdded(index) => {
                self.context = None;
                let Some(device) = self.config.devices.get_mut(index) else { return };

                // Named so it is findable, and uniquely, since a name is how a
                // reading gets back to the channel it came from.
                let existing: Vec<String> = device
                    .hardware
                    .channel_infos()
                    .into_iter()
                    .map(|info| info.name)
                    .collect();
                let name = unused_channel_name("New channel", &existing);

                let info = lumberdaq::channel::ChannelInfo {
                    name: name.clone(),
                    ..Default::default()
                };

                if device.hardware.add_channel(info) {
                    let position = device.hardware.channel_infos().len() - 1;
                    // Read back rather than assumed: the backend may have
                    // filled in the unit its readings arrive in.
                    let unit = device
                        .hardware
                        .channel_info(position)
                        .map(|info| info.unit.clone())
                        .unwrap_or_default();

                    if let Some(app_device) = self.devices.get_mut(index) {
                        app_device.expanded = true;
                        app_device.channels.push(AppChannel {
                            name,
                            unit,
                            latest: None,
                            samples: 0,
                        });
                    }
                    // Straight to its settings: a channel bound to whatever
                    // input came next is a starting point, not an answer.
                    self.selected = Some(Selection::Channel(index, position));
                    self.number_draft = self.channel_number(index, position);
                    self.rig_changed();
                }
            }
            Message::SerialIndexEdited(device, channel, text) => {
                // Whole fields only, and not negative: an index counts fields
                // of a frame. The config hears about it once it is one, while
                // the field shows what is being typed either way.
                if let Ok(index) = text.parse::<u32>() {
                    if let Some(HardwareConfig::SerialStream(serial)) =
                        self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                    {
                        if let Some(channel) = serial.channels.get_mut(channel) {
                            channel.index = index as i64;
                            self.rig_changed();
                        }
                    }
                }
                self.number_draft = text;
            }
            Message::PicoChannelChosen(device, channel, number) => {
                if let Some(HardwareConfig::PicoHrdl(pico)) =
                    self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                {
                    if let Some(channel) = pico.channels.get_mut(channel) {
                        channel.channel = number;
                        self.rig_changed();
                    }
                }
            }
            Message::PicoRangeChosen(device, channel, range) => {
                if let Some(HardwareConfig::PicoHrdl(pico)) =
                    self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                {
                    if let Some(channel) = pico.channels.get_mut(channel) {
                        channel.range = range.0;
                        self.rig_changed();
                    }
                }
            }
            Message::PicoSingleEnded(device, channel, single_ended) => {
                if let Some(HardwareConfig::PicoHrdl(pico)) =
                    self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                {
                    if let Some(channel) = pico.channels.get_mut(channel) {
                        channel.single_ended = single_ended;
                        self.rig_changed();
                    }
                }
            }
            Message::NiChannelChosen(device, channel, number) => {
                if let Some(HardwareConfig::NiDaqmx(ni)) =
                    self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                {
                    if let Some(channel) = ni.channels.get_mut(channel) {
                        channel.channel = number;
                        self.rig_changed();
                    }
                }
            }
            Message::NiRangeChosen(device, channel, range) => {
                if let Some(HardwareConfig::NiDaqmx(ni)) =
                    self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                {
                    if let Some(channel) = ni.channels.get_mut(channel) {
                        channel.range = range.0;
                        self.rig_changed();
                    }
                }
            }
            Message::NiSingleEnded(device, channel, single_ended) => {
                if let Some(HardwareConfig::NiDaqmx(ni)) =
                    self.config.devices.get_mut(device).map(|device| &mut device.hardware)
                {
                    if let Some(channel) = ni.channels.get_mut(channel) {
                        channel.single_ended = single_ended;
                        self.rig_changed();
                    }
                }
            }
            Message::PortsRefreshed => {
                self.ports = serial_stream::available_ports();
                self.note(format!("{} serial port(s) found", self.ports.len()));
            }
            Message::NiDevicesRefreshed => {
                self.ni_devices = lumberdaq::hardware::ni_daqmx::available_devices();
                self.note(format!("{} NI device(s) found", self.ni_devices.len()));
            }
            Message::SerialPortChosen(index, port) => {
                if let Some(HardwareConfig::SerialStream(serial)) =
                    self.config.devices.get_mut(index).map(|device| &mut device.hardware)
                {
                    serial.port = port;
                    self.rig_changed();

                    // The dot beside the device is about the old port until
                    // something asks again, and the moment somebody picks a
                    // port is the moment they want to know about it.
                    if let Some(device) = self.config.devices.get(index) {
                        let name = device.info.name.clone();
                        self.check_devices(Checking::Just(name));
                    }

                    self.note_address_clashes();
                }
            }
            Message::SerialBaudChosen(index, baudrate) => {
                if let Some(HardwareConfig::SerialStream(serial)) =
                    self.config.devices.get_mut(index).map(|device| &mut device.hardware)
                {
                    serial.baudrate = baudrate;
                    self.rig_changed();

                    // Picking a rate is asking whether it is the right one,
                    // and the answer takes seconds to get, so start now
                    // rather than waiting to be asked a second time.
                    self.test_stream(index);
                }
            }
            Message::SerialPatternEdited(index, pattern) => {
                if let Some(HardwareConfig::SerialStream(serial)) =
                    self.config.devices.get_mut(index).map(|device| &mut device.hardware)
                {
                    serial.frame_pattern = pattern;
                    self.rig_changed();
                }
            }
            Message::NiDeviceEdited(index, name) => {
                if let Some(HardwareConfig::NiDaqmx(ni)) =
                    self.config.devices.get_mut(index).map(|device| &mut device.hardware)
                {
                    ni.device = name;
                    self.rig_changed();
                }
            }
            Message::MockModeChanged(index, mode) => {
                if let Some(HardwareConfig::MockHardware(mock)) =
                    self.config.devices.get_mut(index).map(|device| &mut device.hardware)
                {
                    mock.acquisition = match mode {
                        MockMode::Polled => mock_hardware::Acquisition::Polled,
                        // The interval it had, or a default it can be changed
                        // from: switching mode should not also be choosing a
                        // rate somebody never said.
                        MockMode::Streaming => mock_hardware::Acquisition::Streaming {
                            sample_interval_ms: match mock.acquisition {
                                mock_hardware::Acquisition::Streaming { sample_interval_ms } => sample_interval_ms,
                                mock_hardware::Acquisition::Polled => 100,
                            },
                        },
                    };
                    self.rig_changed();
                }
            }
            Message::MockIntervalChanged(index, interval) => {
                if let Some(HardwareConfig::MockHardware(mock)) =
                    self.config.devices.get_mut(index).map(|device| &mut device.hardware)
                {
                    mock.acquisition = mock_hardware::Acquisition::Streaming { sample_interval_ms: interval };
                    self.rig_changed();
                }
            }
            Message::MockInputChanged(device, channel, kind) => {
                if let Some(input) = self.mock_input_mut(device, channel) {
                    // The number carried over where the new kind has one, so
                    // flipping between Constant and Sine to look at them does
                    // not quietly lose what was typed.
                    let number = MockInputKind::number(input).unwrap_or(1.0);
                    *input = match kind {
                        MockInputKind::Random => MockHardwareInput::Random,
                        MockInputKind::Constant => MockHardwareInput::Constant(number),
                        MockInputKind::Sine => MockHardwareInput::Sine { frequency_hz: number },
                    };
                    self.number_draft = self.channel_number(device, channel);
                    self.rig_changed();
                }
            }
            Message::MockNumberEdited(device, channel, text) => {
                // The field shows what was typed either way; the config only
                // hears about it once it is a number.
                if let Ok(number) = text.parse::<f64>() {
                    if let Some(input) = self.mock_input_mut(device, channel) {
                        *input = match input {
                            MockHardwareInput::Constant(_) => MockHardwareInput::Constant(number),
                            MockHardwareInput::Sine { .. } => {
                                MockHardwareInput::Sine { frequency_hz: number }
                            }
                            MockHardwareInput::Random => MockHardwareInput::Random,
                        };
                        self.rig_changed();
                    }
                }
                self.number_draft = text;
            }
            Message::DeviceRenamed(index, name) => {
                let Some(device) = self.config.devices.get_mut(index) else { return };
                let was = std::mem::replace(&mut device.info.name, name.clone());

                // The list on screen mirrors the config rather than holding its
                // own truth, so it is brought along rather than left to drift.
                if let Some(device) = self.devices.get_mut(index) {
                    device.name = name.clone();
                }
                // A plot names a channel by its device and its own name, so a
                // rename would otherwise leave every plot pointing at a device
                // that no longer exists — a trace that quietly stops receiving
                // anything, and a layout naming something that is not there.
                self.rename_device_on_plots(&was, &name);
                self.rig_changed();
            }
            Message::DeviceIntervalChanged(index, interval) => {
                if let Some(device) = self.config.devices.get_mut(index) {
                    device.read_interval_ms = interval;
                    self.rig_changed();
                }
            }
            Message::ChannelRenamed(device, channel, name) => {
                let Some(device_config) = self.config.devices.get(device) else { return };
                let device_name = device_config.info.name.clone();

                let Some(info) = self.channel_info_mut(device, channel) else { return };
                let was = std::mem::replace(&mut info.name, name.clone());

                if let Some(app_channel) =
                    self.devices.get_mut(device).and_then(|d| d.channels.get_mut(channel))
                {
                    app_channel.name = name.clone();
                }
                self.rename_channel_on_plots(&device_name, &was, &name);
                self.rig_changed();
            }
            Message::ChannelUnitChanged(device, channel, unit) => {
                if let Some(info) = self.channel_info_mut(device, channel) {
                    info.unit = unit.clone();
                }
                if let Some(app_channel) =
                    self.devices.get_mut(device).and_then(|d| d.channels.get_mut(channel))
                {
                    app_channel.unit = unit;
                }
                self.rig_changed();
            }
            Message::ScaleEdited(device, channel, equation) => {
                if let Some(info) = self.channel_info_mut(device, channel) {
                    info.scale = match equation.trim().is_empty() {
                        // An empty formula is no calculation, which is how the
                        // config says it too: the field is simply absent. So
                        // clearing the box is how one is removed, and there is
                        // no separate add or delete to find.
                        true => None,
                        // The parameters of a parameterised scale are kept:
                        // only the equation is being typed, and dropping the
                        // constants it refers to would break it on the first
                        // keystroke.
                        false => Some(match info.scale.take() {
                            Some(Scale::Parameterised { from, parameters, .. }) => {
                                Scale::Parameterised { from, equation, parameters }
                            }
                            _ => Scale::Equation(equation),
                        }),
                    };
                    self.rig_changed();
                }
            }
            Message::LogToggled => {
                // The log is the outermost split, so the root of the layout is
                // its divider. Resizing that is what opens and shuts it: a
                // pane_grid pane has no hidden state of its own.
                let Some((split, ratio)) = self.log_split() else { return };

                if self.log_open {
                    // Remembered before shutting, so a divider that was dragged
                    // somewhere deliberate returns there.
                    self.log_ratio = ratio;
                    self.panes.resize(split, LOG_SHUT);
                } else {
                    self.panes.resize(split, self.log_ratio);
                }
                self.log_open = !self.log_open;
            }
            Message::RunStarted => {
                if self.run == RunState::Stopped {
                    self.start_run();
                }
            }
            Message::RecordPressed => {
                // One button for both halves of the same intention. Recording
                // implies acquiring, so asking to record a stopped rig starts
                // it: nobody presses record meaning "write down nothing".
                if self.recording() {
                    if let Some(acquisition) = self.acquisition.as_ref() {
                        acquisition.recording.store(false, Ordering::Relaxed);
                    }
                    self.note("recording stopped".to_string());
                    return;
                }

                if self.run == RunState::Stopped {
                    self.start_run();
                }

                // Not while a previous run is still winding up: the flag would
                // be set on an acquisition that is about to be dropped.
                if self.run == RunState::Running {
                    if let Some(acquisition) = self.acquisition.as_ref() {
                        acquisition.recording.store(true, Ordering::Relaxed);
                        self.note("recording started".to_string());
                    }
                }
            }
            Message::RunStopped => {
                if self.run == RunState::Running {
                    if let Some(acquisition) = self.acquisition.as_ref() {
                        // Lowered before the run ends so the recorder flushes
                        // and closes its sink, rather than the recording being
                        // cut off wherever the last batch happened to land.
                        // It also means starting again does not silently
                        // resume recording.
                        acquisition.recording.store(false, Ordering::Relaxed);
                        acquisition.stop.store(true, Ordering::Relaxed);
                    }
                    // Not Stopped yet: the thread is still reading whatever it
                    // was in the middle of. `reap_stopped_run` decides when.
                    self.run = RunState::Stopping;
                    self.note("stopping".to_string());
                }
            }
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                // The plots just changed width, so their axes need working out
                // again.
                self.settle_plots();
            }
            // Back to whichever list the plot came from. A plot widget works
            // out its own ticks while drawing and publishes them as a message,
            // and only draws their labels once that message has come back - so
            // delivering it to the wrong list is why the viewer's plots had no
            // numbers on their axes.
            Message::Plot(index, plot_message) => {
                let plots = match self.mode {
                    Mode::Record => &mut self.plots,
                    Mode::View => &mut self.view_plots,
                };

                if let Some(widget) = plots.get_mut(index).and_then(|plot| plot.widget.as_mut()) {
                    widget.update(plot_message);
                }
            }
            Message::Tick => {
                // Take everything waiting, rather than one per frame: batches
                // arrive at each device's read interval and carry however many
                // samples were collected in it, so the count per frame varies.
                // Drained into a batch first: `record` and `note` both want
                // &mut self, which the receiver borrow would otherwise hold.
                let mut arrived = Vec::new();
                if let Some(acquisition) = self.acquisition.as_ref() {
                    while let Ok(message) = acquisition.from_acquisition.try_recv() {
                        arrived.push(message);
                    }
                }

                for message in arrived {
                    match message {
                        FromAcquisition::Data { device, channel, datapoints } => {
                            self.record(&device, &channel, &datapoints);
                        }
                        FromAcquisition::Status(text) => self.note(text),
                        FromAcquisition::Ready => {
                            if self.run == RunState::Starting {
                                self.run = RunState::Running;
                                // Timed from here rather than from the press,
                                // so the trace starts at the right hand edge
                                // where it belongs.
                                self.slide_viewport();
                                self.note("acquisition started".to_string());
                            }
                        }
                        FromAcquisition::Connection { device, connected } => {
                            if let Some(found) =
                                self.devices.iter_mut().find(|found| found.name == device)
                            {
                                found.connected = Some(connected);
                            }
                        }
                        FromAcquisition::Trouble { device, concern } => {
                            if let Some(found) =
                                self.devices.iter_mut().find(|found| found.name == device)
                            {
                                found.concern = concern;
                            }
                        }
                    }
                }

                self.reap_stopped_run();
                self.collect_pico_probe();
                self.collect_connection_check();
                self.collect_stream_test();

                // Slid every frame whether or not anything arrived, which is
                // what keeps the scroll smooth while the data stays honest.
                // Only while reading. A stopped plot that kept sliding would
                // walk away from its own data and undo a pan on the next
                // frame; a starting one would scroll away the seconds spent
                // opening the hardware, and the first reading would land
                // somewhere behind the leading edge.
                if self.run == RunState::Running {
                    self.slide_viewport();
                }

                // The frame tick doubles as the timer the debounce needs, so
                // saving costs no extra machinery.
                self.save_layout_if_settled();
                self.save_rig_if_settled();
            }
        }
    }

    /// Ask every device whether it is there, without starting a run.
    ///
    /// Opening a rig is how you find out that a cable is loose or a driver is
    /// missing, and finding that out at the moment you meant to start
    /// recording is finding out too late. This does the opening and lets go
    /// again, so the dots beside the devices mean something while stopped.
    ///
    /// On a thread, because opening hardware is slow: a Pico is about a
    /// second, and a serial port that is not there waits for its timeout.
    ///
    /// Not on a timer, deliberately. Opening a serial port asserts DTR, which
    /// resets an Arduino and most boards like it — so a check that ran itself
    /// every half minute would reset somebody's hardware all afternoon. It
    /// happens when a project opens, when a run stops, when a device is
    /// pointed at different hardware, and when asked.
    fn check_connections(&mut self) {
        self.check_devices(Checking::Everything);
    }

    /// The same, for some or all of the rig.
    fn check_devices(&mut self, scope: Checking) {
        // A run owns its devices exclusively; opening them from here would be
        // taking them away from it.
        if self.run != RunState::Stopped {
            return;
        }

        // One at a time: two threads opening the same port would have the
        // second fail for no reason but the first, and report a red dot for
        // a device that is sitting there working.
        if self.connection_check.is_some() || self.connection_test.is_some() {
            self.wanted_check = Some(match self.wanted_check.take() {
                // Two different devices changed while one check ran. Rather
                // than choose between them, ask about everything.
                Some(already) if already != scope => Checking::Everything,
                _ => scope,
            });
            return;
        }

        let mut config = self.config.clone();
        if let Checking::Just(name) = &scope {
            config.devices.retain(|device| &device.info.name == name);
        }
        if config.devices.is_empty() {
            return;
        }

        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let found: Checked = match lumberdaq::daq::Daq::from_config(config) {
                Ok(mut daq) => {
                    let report = daq.connect();
                    let mut answers: Vec<Answered> = report
                        .connected
                        .into_iter()
                        .map(|device| Answered { device, connected: true, why: None })
                        .collect();
                    answers.extend(report.failed.into_iter().map(|(device, why)| Answered {
                        device,
                        connected: false,
                        why: Some(why),
                    }));
                    // Dropped here, which is what lets go: a serial reader is
                    // stopped and a Pico unit is closed on the way out.
                    drop(daq);
                    Ok(answers)
                }
                Err(problem) => Err(problem.to_string()),
            };

            let _ = sender.send(found);
        });

        self.connection_check = Some(receiver);
    }

    /// Try a device's settings against the hardware and see if they read it.
    ///
    /// A baud rate cannot be worked out by asking, only by listening, so this
    /// opens the port and waits to hear something the config can read. On a
    /// thread for the same reason as everything else that touches hardware:
    /// the patience below is seconds, and a window that stops drawing for that
    /// long looks broken.
    fn test_stream(&mut self, index: usize) {
        if self.run != RunState::Stopped {
            return;
        }

        // The old verdict is about settings that may no longer be the ones on
        // screen, and a stale warning border is worse than none. Before the
        // queueing below, so a test that has to wait its turn does not leave
        // the last answer showing in the meantime.
        self.tested = None;

        // Both open ports. Two things opening one port has the second fail on
        // the first, which would be reported as a device that is not there.
        if self.connection_check.is_some() || self.connection_test.is_some() {
            self.wanted_test = Some(index);
            return;
        }

        let Some(device) = self.config.devices.get(index) else { return };
        let HardwareConfig::SerialStream(serial) = &device.hardware else { return };

        let config = serial.clone();
        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let found = serial_stream::check_stream(&config, STREAM_TEST_PATIENCE)
                .map_err(|problem| problem.to_string());
            let _ = sender.send((index, found));
        });

        self.connection_test = Some(receiver);
        self.note("listening to the port".to_string());
    }

    /// Take the verdict, if the test has finished.
    fn collect_stream_test(&mut self) {
        // Not an early return on there being no test in flight: a test can be
        // waiting on the *connection* check instead, and that leaves nothing
        // here to collect while still leaving something to start.
        if let Some(test) = self.connection_test.as_ref() {
            match test.try_recv() {
            Ok((index, found)) => {
                self.connection_test = None;

                let name = match self.config.devices.get(index) {
                    Some(device) => device.info.name.clone(),
                    None => return,
                };

                match found {
                    Ok(check) => {
                        self.note(match &check {
                            StreamCheck::Reads => format!("{} reads at these settings", name),
                            StreamCheck::Unreadable => format!(
                                "{} is sending something, but none of it matches the frame                                  pattern - the baud rate is the usual reason",
                                name
                            ),
                            StreamCheck::Mismatched { reason } => format!(
                                "{} is sending frames the channels cannot read: {}",
                                name, reason
                            ),
                            StreamCheck::Silent => {
                                format!("{} sent nothing at all", name)
                            }
                        });
                        // The dot says the same thing as the border, so
                        // the two cannot disagree about one device. Only the
                        // verdict that colours the box: a frame the channels
                        // cannot read is a channel fault, and pointing at the
                        // device for it would send somebody to the wrong
                        // place twice over.
                        if let Some(found) =
                            self.devices.iter_mut().find(|found| found.name == name)
                        {
                            // The port opened, so whatever else is wrong, it
                            // is there.
                            found.connected = Some(true);
                            found.concern = match &check {
                                StreamCheck::Unreadable => {
                                    Some("nothing it sends matches the frame pattern".to_string())
                                }
                                _ => None,
                            };
                        }

                        self.tested = Some((index, check));
                    }
                    // Not being able to open the port is a different failure,
                    // and saying the baud rate is wrong would send somebody
                    // after the wrong thing entirely.
                    Err(problem) => self.note(format!("could not listen to {}: {}", name, problem)),
                }
            }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.connection_test = None,
            }
        }

        // Only once nothing else has the port.
        if self.connection_test.is_none() && self.connection_check.is_none() {
            if let Some(index) = self.wanted_test.take() {
                self.test_stream(index);
            }
        }
    }

    /// Take the answer, if the check has finished.
    fn collect_connection_check(&mut self) {
        let Some(check) = self.connection_check.as_ref() else { return };

        match check.try_recv() {
            Ok(Ok(answers)) => {
                self.connection_check = None;

                let (mut answered, total) = (0, answers.len());

                for answer in answers {
                    if let Some(found) =
                        self.devices.iter_mut().find(|found| found.name == answer.device)
                    {
                        found.connected = Some(answer.connected);
                    }
                    match answer.why {
                        Some(why) => {
                            self.note(format!("{} did not answer: {}", answer.device, why))
                        }
                        None => answered += 1,
                    }
                }

                // One line whether or not anything is wrong. Reporting only
                // failures reads well until nothing fails, and then silence
                // means both "every device is there" and "nothing ever
                // looked" - which is exactly the doubt this is here to
                // settle.
                self.note(format!("checked the devices: {} of {} answered", answered, total));
            }
            Ok(Err(problem)) => {
                self.connection_check = None;
                self.note(format!("could not check the devices: {}", problem));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.connection_check = None,
        }

        // Only once the last one has let go of the hardware.
        if self.connection_check.is_none() {
            if let Some(scope) = self.wanted_check.take() {
                self.check_devices(scope);
            }
        }
    }

    /// Ask the attached Pico unit how many inputs it has, on a thread.
    ///
    /// Opening the unit loads the driver and initialises it over USB, which is
    /// slow enough to be felt: doing it here would freeze the window for about
    /// a second every time a channel was clicked. So it goes to a thread, the
    /// panel draws immediately offering every channel the backend allows, and
    /// the list narrows when the answer comes back.
    ///
    /// Skipped when the answer is already known, when one is already on its
    /// way, and while a run is going — a unit being read cannot also be opened
    /// to be asked. A failed attempt leaves it unknown rather than remembered,
    /// so plugging a unit in and clicking again tries afresh.
    fn probe_pico(&mut self) {
        if self.pico_inputs.is_some()
            || self.pico_probe.is_some()
            || self.run != RunState::Stopped
        {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            // Nobody is left waiting if this fails: the receiver going away
            // means the interface moved on, which is allowed.
            let found = pico_hrdl::identify()
                .map(|unit| unit.inputs)
                .map_err(|problem| problem.to_string());
            let _ = sender.send(found);
        });
        self.pico_probe = Some(receiver);
    }

    /// Take the answer, if the thread has finished with it.
    fn collect_pico_probe(&mut self) {
        let Some(probe) = self.pico_probe.as_ref() else { return };

        match probe.try_recv() {
            Ok(found) => {
                self.pico_probe = None;
                self.pico_inputs = found.as_ref().ok().copied().flatten();

                match found {
                    Ok(Some(inputs)) => self.note(format!("the Pico unit has {} inputs", inputs)),
                    Ok(None) => self.note(
                        "the Pico unit did not say which model it is, so every channel the \
                         backend allows is offered"
                            .to_string(),
                    ),
                    // The backend's own words, which name the driver and where
                    // to get it. Saying "no unit answered" instead would send
                    // somebody looking at their cable when the software is
                    // what is missing.
                    Err(problem) => self.note(problem),
                }
            }
            // Still working. `Disconnected` would mean the thread died without
            // sending, which it cannot, but treating it as "no answer" keeps
            // the probe from being waited on for ever.
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.pico_probe = None,
        }
    }

    /// Ask for a moment of frames, so the plots can settle their axes.
    ///
    /// Called wherever a plot appears, gains or loses a trace, or changes
    /// size: each of those leaves the widget with something to recompute and
    /// tell us about.
    fn settle_plots(&mut self) {
        self.plots_settling = Some(Instant::now());
    }

    /// Put every plot's viewport on the last `window_seconds` up to now.
    ///
    /// Called every frame while reading, which is what makes the scroll
    /// smooth, and once by hand at the points where the span changes without a
    /// run to carry it through.
    fn slide_viewport(&mut self) {
        let now = seconds_since(self.reference, Utc::now());
        let window = self.window_seconds;
        for plot in self.plots.iter_mut() {
            if let Some(widget) = plot.widget.as_mut() {
                widget.set_x_lim(now - window, now);
            }
        }
    }

    /// Let the plots be panned by hand, or not.
    ///
    /// While a run is going the viewport is set from the clock on every frame,
    /// so a pan is overwritten the moment it is made — the trace springs back
    /// to now and the drag looks broken rather than disallowed. Unbinding the
    /// gesture says plainly that it is not available, and the cursor stops
    /// offering it. Box zoom on the right button is left alone: it sets a
    /// range rather than fighting for the same one.
    fn set_plot_panning(&mut self, allowed: bool) {
        for plot in self.plots.iter_mut() {
            let Some(widget) = plot.widget.as_mut() else { continue };
            let controls = widget.get_controls_mut();

            match allowed {
                true => {
                    controls.bind_drag(iced::mouse::Button::Left, iced_plot::DragAction::Pan);
                    controls.bind_scroll(
                        iced::keyboard::Modifiers::NONE,
                        iced_plot::ScrollAction::Pan,
                    );
                }
                false => {
                    controls.unbind_drag(iced::mouse::Button::Left);
                    controls.unbind_scroll(iced::keyboard::Modifiers::NONE);
                }
            }
        }
    }

    /// Whether a recording is being asked for.
    ///
    /// The flag rather than the `Recorder` itself, which is on the other side
    /// of the thread. It is what was asked for; the recorder acts on it when
    /// the next batch or flush comes round.
    fn recording(&self) -> bool {
        self.acquisition
            .as_ref()
            .is_some_and(|acquisition| acquisition.recording.load(Ordering::Relaxed))
    }

    /// Let go of a run that has finished stopping.
    ///
    /// `is_finished` rather than joining outright: `join` would block the
    /// interface until the thread came round its read loop, which is up to a
    /// whole read interval of frozen window. Asking every frame costs nothing
    /// and the join is instant once it answers yes.
    fn reap_stopped_run(&mut self) {
        if self.run != RunState::Stopping {
            return;
        }

        let finished = self
            .acquisition
            .as_ref()
            .is_some_and(|acquisition| acquisition.thread.is_finished());

        if finished {
            if let Some(acquisition) = self.acquisition.take() {
                // Instant, now that the thread has ended, and it is what turns
                // a panic in there into something we hear about.
                if acquisition.thread.join().is_err() {
                    self.note("the acquisition thread panicked".to_string());
                }
            }
            self.run = RunState::Stopped;
            // A complaint is about a device that is reading, and none of them
            // are now. Left standing it would be an amber dot describing a run
            // that is over.
            for device in self.devices.iter_mut() {
                device.concern = None;
            }
            // The devices are free again, and whether they are still there is
            // worth knowing before the next run rather than during it.
            self.check_connections();
            self.set_plot_panning(true);
            self.note("acquisition stopped".to_string());
        }
    }

    /// One channel's description in the configuration, to change.
    fn channel_info_mut(
        &mut self,
        device: usize,
        channel: usize,
    ) -> Option<&mut lumberdaq::channel::ChannelInfo> {
        self.config.devices.get_mut(device)?.hardware.channel_info_mut(channel)
    }

    /// Follow a device's new name on every plot that draws one of its channels.
    fn rename_device_on_plots(&mut self, was: &str, now: &str) {
        if was == now {
            return;
        }
        for plot in self.plots.iter_mut() {
            for plotted in plot.channels.iter_mut() {
                if plotted.reference.device == was {
                    plotted.reference.device = now.to_string();
                }
            }
        }
        self.layout_changed();
    }

    /// The same for one channel of one device.
    fn rename_channel_on_plots(&mut self, device: &str, was: &str, now: &str) {
        if was == now {
            return;
        }
        for plot in self.plots.iter_mut() {
            for plotted in plot.channels.iter_mut() {
                if plotted.reference.device == device && plotted.reference.channel == was {
                    plotted.reference.channel = now.to_string();
                }
            }
        }
        self.layout_changed();
    }

    /// Take every channel of a device off every plot.
    fn forget_device_on_plots(&mut self, device: &str) {
        for plot in self.plots.iter_mut() {
            while let Some(position) = plot
                .channels
                .iter()
                .position(|plotted| plotted.reference.device == device)
            {
                plot.remove_channel(position);
            }
        }
        self.layout_changed();
    }

    /// Take one channel off every plot it is on.
    fn forget_channel_on_plots(&mut self, reference: &ChannelRef) {
        for plot in self.plots.iter_mut() {
            while let Some(position) =
                plot.channels.iter().position(|plotted| &plotted.reference == reference)
            {
                plot.remove_channel(position);
            }
        }
        self.layout_changed();
    }

    /// What generates one mock channel's values, to change.
    fn mock_input_mut(
        &mut self,
        device: usize,
        channel: usize,
    ) -> Option<&mut MockHardwareInput> {
        match self.config.devices.get_mut(device).map(|device| &mut device.hardware) {
            Some(HardwareConfig::MockHardware(mock)) => {
                mock.channels.get_mut(channel).map(|channel| &mut channel.input)
            }
            _ => None,
        }
    }

    /// A channel's number as text, for the field that edits it.
    ///
    /// One draft serves every backend because only one channel is selected at
    /// a time, and none of them has two free-typed numbers.
    fn channel_number(&self, device: usize, channel: usize) -> String {
        match self.config.devices.get(device).map(|device| &device.hardware) {
            Some(HardwareConfig::MockHardware(mock)) => mock
                .channels
                .get(channel)
                .and_then(|channel| MockInputKind::number(&channel.input))
                .map(|number| number.to_string())
                .unwrap_or_default(),
            Some(HardwareConfig::SerialStream(serial)) => serial
                .channels
                .get(channel)
                .map(|channel| channel.index.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Note that the rig's configuration differs from what is on disk.
    ///
    /// Separate from the layout only in what gets written; both settle on the
    /// same delay. Nothing here reaches the devices being read right now — a
    /// run holds its own `Daq`, built from the config as it was when it
    /// started, so a change takes effect when the run is started again.
    fn rig_changed(&mut self) {
        // Rebuilt here rather than kept from launch. The channels a plot
        // offers to add come from the setup, so renaming one has to rename it
        // in the list somebody picks from — and adding or deleting one has to
        // show up there too. Every edit to the rig comes through here, which
        // is what makes this the one place it needs doing.
        self.available = self.config.available_inputs();
        self.plottable = self.config.all_channels();
        self.rig_dirty_since = Some(Instant::now());

        // Judged here rather than while drawing. Compiling every equation is
        // cheap once per edit and wasteful sixty times a second, and an
        // equation only changes by an edit.
        let calculated = calculated_health(&self.config);
        if let Some(device) =
            self.devices.iter_mut().find(|device| device.kind == DeviceKind::Calculated)
        {
            device.connected = calculated;
        }
        // A verdict is about the settings that were tried. Any edit may have
        // changed them, and a warning border left over from settings nobody
        // can see any more is worse than no border at all. The dot goes with
        // it, or the two would disagree about the same device.
        //
        // Safe to clear the concerns here because the rig can only be edited
        // while stopped, so none of them can be from a run in progress.
        self.tested = None;
        for device in self.devices.iter_mut() {
            device.concern = None;
        }
    }

    /// Write the configuration out, if it has settled since the last change.
    fn save_rig_if_settled(&mut self) {
        match self.rig_dirty_since {
            Some(changed) if changed.elapsed() >= SAVE_DELAY => {}
            _ => return,
        }
        self.rig_dirty_since = None;

        let Some(project) = self.project() else {
            return;
        };

        let path = project.config_path();
        match lumberdaq::configuration::write_configuration_file(&path, &self.config) {
            Ok(()) => self.note(format!("saved {}", path.display())),
            Err(error) => self.note(format!("could not save the configuration: {}", error)),
        }
    }

    /// Note that the layout differs from what is on disk.
    ///
    /// The write itself waits for `SAVE_DELAY` to pass without another change,
    /// so a burst of typing becomes one write rather than one per keystroke.
    fn layout_changed(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    /// Write the layout out, if it has settled since the last change.
    fn save_layout_if_settled(&mut self) {
        match self.dirty_since {
            Some(changed) if changed.elapsed() >= SAVE_DELAY => {}
            _ => return,
        }
        self.dirty_since = None;

        let saved = plot_config::PlotConfig {
            version: plot_config::VERSION,
            history_seconds: self.window_seconds as u64,
            // Read back out of the grid, so what is written is where the panes
            // actually are rather than where they were put.
            layout: layout_from(self.plot_panes.layout(), &self.plot_panes),
            plots: self
                .plots
                .iter()
                .map(|plot| plot_config::Plot {
                    number: plot.number,
                    // Only a name somebody chose. Writing out the "Plot 3" a
                    // reader would have produced anyway just makes the file
                    // noisier than the layout it describes.
                    name: match plot.name == format!("Plot {}", plot.number) {
                        true => None,
                        false => Some(plot.name.clone()),
                    },
                    channels: plot
                        .channels
                        .iter()
                        .map(|plotted| plotted.reference.clone())
                        .collect(),
                })
                .collect(),
        };

        let Some(project) = self.project() else {
            // Nothing open. The arrangement is still on screen and still
            // correct; there is simply nowhere for it to live yet.
            return;
        };

        match project.write_layout(&saved) {
            Ok(path) => self.note(format!("saved {}", path.display())),
            Err(problem) => self.note(problem.to_string()),
        }
    }

    /// A pane's heading row, inset from the edges like the content below it.
    ///
    /// The row's own alignment is left alone: most headings want their parts
    /// centred against each other, but one holding text of two sizes wants
    /// them sharing a baseline instead.
    fn pane_heading<'a>(&self, row: Row<'a, Message>) -> Element<'a, Message> {
        row.padding([8, 10]).into()
    }

    /// The same, where clicking anywhere along it means something.
    ///
    /// The whole line rather than the words on it: a heading is a place, and
    /// somebody aiming for it aims at the row. Buttons sitting in the row keep
    /// working, because a button captures its own press and a `MouseArea`
    /// leaves an event alone once its child has taken it.
    fn pressable_heading<'a>(
        &self,
        row: Row<'a, Message>,
        on_press: Message,
    ) -> Element<'a, Message> {
        MouseArea::new(row.padding([8, 10]).width(Fill)).on_press(on_press).into()
    }

    /// A whole pane: heading, a rule the full width of it, then the content.
    ///
    /// The rule is outside the padding on purpose. Inset, it reads as a line
    /// belonging to the title; edge to edge, it separates the pane's heading
    /// from its body the way a heading rule should.
    /// A pane whose body fills it rather than scrolling inside it.
    ///
    /// For content that lays itself out to the space available — a grid of
    /// plots — where a scrollable would be asking something of unbounded
    /// height to fit in a box of unbounded height.
    fn pane_filling_pressable<'a>(
        &self,
        title: Row<'a, Message>,
        on_press: Message,
        body: Element<'a, Message>,
    ) -> Element<'a, Message> {
        column![
            self.pressable_heading(title, on_press),
            pane_rule(),
            container(body).padding(10).width(Fill).height(Fill),
        ]
        .into()
    }

    fn pane<'a>(&self, title: Row<'a, Message>, body: Element<'a, Message>) -> Element<'a, Message> {
        column![
            self.pane_heading(title),
            pane_rule(),
            // Scrolls under a heading that stays put, rather than the heading
            // scrolling away with it.
            scrollable(container(body).padding(10).width(Fill)).height(Fill),
        ]
        .into()
    }

    /// The divider between the log and everything above it, and where it sits.
    fn log_split(&self) -> Option<(pane_grid::Split, f32)> {
        match self.panes.layout() {
            pane_grid::Node::Split { id, ratio, .. } => Some((*id, *ratio)),
            pane_grid::Node::Pane(_) => None,
        }
    }

    /// Add a line to the run log, dropping the oldest once it is full.
    fn note(&mut self, text: String) {
        let at = Utc::now();
        self.logbook.write(at, &text);

        self.log.push(LogEntry { at, text });
        // The oldest goes, since the newest is the one being read.
        if self.log.len() > LOG_LIMIT {
            self.log.remove(0);
        }
    }

    /// File one batch against the channel it came from.
    ///
    /// Matched by name because that is the only identity a `Batch` carries,
    /// and the same pair of names lumberdaq's own `ChannelRef` uses.
    fn record(&mut self, device_name: &str, channel_name: &str, datapoints: &[DataPoint]) {
        let Some(channel) = find_channel(&mut self.devices, device_name, channel_name) else {
            return;
        };

        channel.samples += datapoints.len();
        if let Some(point) = datapoints.last() {
            channel.latest = Some(point.value);
        }

        let reference = self.reference;
        let cutoff = seconds_since(reference, Utc::now()) - self.window_seconds;

        // Every plot showing this channel, since a channel may be on several.
        for plot in self.plots.iter_mut() {
            let Some(widget) = plot.widget.as_mut() else { continue };

            for plotted in plot.channels.iter() {
                if plotted.reference.device != device_name
                    || plotted.reference.channel != channel_name
                {
                    continue;
                }

                // Ignored rather than unwrapped: a series that has gone missing
                // is a trace that stops drawing, not a reason to take the
                // interface down in the middle of a run.
                let _ = widget.update_series(&plotted.series, |trace| {
                    for point in datapoints {
                        trace
                            .positions
                            .push([seconds_since(reference, point.datetime), point.value]);
                    }
                    trace.positions.retain(|position| position[0] >= cutoff);
                });
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        // Watching the pointer costs nothing when nothing moves, so it is
        // always on. The same is true of listening for a device: nothing
        // happens until Windows says something has, and then it wakes the
        // window - which a plain channel could not do while the window has
        // stopped asking to be drawn.
        let mut listening = vec![
            event::listen_with(watch_pointer),
            Subscription::run(devicewatch::changes),
        ];

        // Frames are expensive: iced rebuilds the whole widget tree for every
        // message, so subscribing to them is asking for that sixty times a
        // second. Worth it only when something has to happen without anybody
        // touching the interface - a run to collect from and scroll, or a
        // change waiting out `SAVE_DELAY` before it is written. Idle on a
        // saved project, neither is true and the app stops asking to be
        // redrawn entirely.
        let reading = self.run != RunState::Stopped;
        let owed = self.dirty_since.is_some() || self.rig_dirty_since.is_some();
        // Long enough for a plot to draw, publish what it found and be drawn
        // again with it. A handful of frames would do; a second is unnoticed
        // and leaves no room for it to be marginal.
        let settling = self
            .plots_settling
            .is_some_and(|since| since.elapsed() < Duration::from_secs(1));
        // A thread that will have an answer shortly is a third reason: without
        // this the answer would sit in the channel until something else woke
        // the interface up.
        let asking = self.pico_probe.is_some()
            || self.connection_check.is_some()
            || self.connection_test.is_some();
        if reading || owed || asking || settling {
            listening.push(window::frames().map(|_| Message::Tick));
        }

        Subscription::batch(listening)
    }

    /// What one recorded run is: when it ran and what it recorded.
    fn run_settings(&self, run: usize) -> Element<'_, Message> {
        let Some(run) = self.runs.get(run) else {
            return text("Nothing selected.").size(14).into();
        };

        let channels: usize = run.devices.iter().map(|device| device.channels.len()).sum();
        let readings: usize = run
            .devices
            .iter()
            .flat_map(|device| device.channels.iter())
            .map(|channel| channel.readings)
            .sum();

        column![
            text(format!("Run {}", run.id)).size(16),
            fact("Started", run.started.with_timezone(&Local).format("%d %B %Y, %H:%M:%S").to_string()),
            // Said as well as shown in local time: a results file keeps UTC,
            // and a run compared against a note made somewhere else needs the
            // unambiguous one.
            fact("Started (UTC)", run.started.format("%Y-%m-%d %H:%M:%S").to_string()),
            fact("Devices", run.devices.len().to_string()),
            fact("Channels", channels.to_string()),
            fact("Readings", readings.to_string()),
        ]
        .spacing(10)
        .into()
    }

    /// What one device of a run was, as the file remembers it.
    fn run_device_settings(&self, run: usize, device: usize) -> Element<'_, Message> {
        let Some(device) = self.runs.get(run).and_then(|run| run.devices.get(device)) else {
            return text("Nothing selected.").size(14).into();
        };

        let readings: usize = device.channels.iter().map(|channel| channel.readings).sum();

        column![
            text(&device.name).size(16),
            fact("Hardware", recorded_hardware(&device.hardware)),
            fact("Channels", device.channels.len().to_string()),
            fact("Readings", readings.to_string()),
        ]
        .spacing(10)
        .into()
    }

    /// What one recorded channel is, and how much of it there is.
    fn run_channel_settings(&self, run: usize, device: usize, channel: usize) -> Element<'_, Message> {
        let found = self
            .runs
            .get(run)
            .and_then(|run| run.devices.get(device))
            .and_then(|device| device.channels.get(channel));

        let Some(channel) = found else {
            return text("Nothing selected.").size(14).into();
        };

        let drawn = self
            .view_plots
            .iter()
            .filter(|plot| plot.channels.iter().any(|t| t.source == Some(channel.id)))
            .map(|plot| plot.name.clone())
            .collect::<Vec<_>>();

        column![
            text(&channel.name).size(16),
            fact(
                "Unit",
                match channel.unit.is_empty() {
                    true => "-".to_string(),
                    false => channel.unit.clone(),
                },
            ),
            fact("Readings", channel.readings.to_string()),
            fact(
                "On plots",
                match drawn.is_empty() {
                    true => "not drawn".to_string(),
                    false => drawn.join(", "),
                },
            ),
        ]
        .spacing(10)
        .into()
    }

    /// What one of the viewer's plots holds.
    ///
    /// Not the recording plot panel: nothing here is configured against a rig.
    /// The channels came out of a file, they are named by the run they came
    /// from, and the only thing to do to one is take it off again.
    fn view_plot_settings(&self, index: usize) -> Element<'_, Message> {
        let Some(plot) = self.view_plots.get(index) else {
            return text("Nothing selected.").size(14).into();
        };

        let traces: Element<'_, Message> = match plot.channels.is_empty() {
            true => text("Click a channel under a run to draw it here.").size(13).into(),
            false => column(plot.channels.iter().enumerate().map(|(at, plotted)| {
                row![
                    container(space::horizontal().width(14).height(3)).style(
                        move |_theme: &Theme| container::Style {
                            background: Some(plotted.colour.into()),
                            ..container::Style::default()
                        }
                    ),
                    text(plotted.reference.to_string()).size(13),
                    space::horizontal(),
                    hint(
                        button(trash_two().size(14))
                            .style(button::text)
                            .padding(4)
                            .on_press(Message::RecordedChannelRemoved(index, at)),
                        "Take this channel off the plot",
                    ),
                ]
                .spacing(8)
                .align_y(Center)
                .into()
            }))
            .spacing(4)
            .into(),
        };

        column![
            text(plot.name.clone()).size(16),
            column![field_label("Channels"), traces].spacing(4),
        ]
        .spacing(12)
        .into()
    }

    /// Begin dragging a channel.
    ///
    /// The flag is what turns pointer movement back into messages, which is
    /// what lets the label follow the cursor. It comes off again on release,
    /// whether or not the drag landed anywhere.
    fn start_drag(&mut self, what: Dragged) {
        self.dragging = Some(what);
        DRAGGING.store(true, Ordering::Relaxed);
    }

    /// What is being dragged, as it should be labelled.
    fn dragged_label(&self) -> Option<String> {
        match self.dragging.as_ref()? {
            Dragged::Live(reference) => Some(reference.channel.clone()),
            Dragged::Recorded(run, device, channel) => self
                .runs
                .get(*run)
                .and_then(|run| run.devices.get(*device))
                .and_then(|device| device.channels.get(*channel))
                .map(|channel| channel.name.clone()),
        }
    }

    /// What a measured channel is called, as a plot names one.
    fn live_reference(&self, device: usize, channel: usize) -> Option<ChannelRef> {
        let found = self.config.devices.get(device)?;
        let name = found.info.name.clone();
        found
            .hardware
            .channel_info(channel)
            .map(|info| ChannelRef { device: name, channel: info.name.clone() })
    }

    /// The same for a calculated one.
    fn calculated_reference(&self, channel: usize) -> Option<ChannelRef> {
        let calculated = self.config.calculated.as_ref()?;
        calculated.channels.get(channel).map(|found| ChannelRef {
            device: calculated.info.name.clone(),
            channel: found.info.name.clone(),
        })
    }

    /// Put a measured or calculated channel on one of the recording plots.
    fn add_to_plot(&mut self, plot: usize, reference: ChannelRef) {
        let window = self.window_seconds;
        let Some(plot) = self.plots.get_mut(plot) else { return };

        plot.add_channel(reference, window);
        let stopped = self.run == RunState::Stopped;
        self.set_plot_panning(stopped);
        self.settle_plots();
        self.layout_changed();
    }

    /// The plots a channel could be put on, in the mode it belongs to.
    fn plot_names(&self) -> Vec<(usize, String)> {
        let plots = match self.mode {
            Mode::Record => &self.plots,
            Mode::View => &self.view_plots,
        };
        plots.iter().enumerate().map(|(at, plot)| (at, plot.name.clone())).collect()
    }

    /// Every channel under a run, or under one of its devices.
    ///
    /// Collected into a list first because plotting them borrows the interface
    /// mutably, and walking the tree while doing so would be holding a read of
    /// the very thing being written.
    fn channels_under(&self, run: usize, device: Option<usize>) -> Vec<(usize, usize, usize)> {
        let Some(found) = self.runs.get(run) else { return Vec::new() };

        found
            .devices
            .iter()
            .enumerate()
            .filter(|(at, _)| device.is_none_or(|only| *at == only))
            .flat_map(|(at, found_device)| {
                (0..found_device.channels.len()).map(move |channel| (run, at, channel))
            })
            .collect()
    }

    /// Read one recorded channel and draw it.
    ///
    /// The readings are fetched here rather than when the tree was loaded:
    /// this is the first moment anybody has said they want this particular
    /// channel, and a run holds far more of them than anyone looks at.
    ///
    /// Done on this thread. Unlike opening a Pico, a query against a local
    /// file with an index on exactly this lookup is quick; if a long run ever
    /// makes it felt, it moves to a thread the way the Pico probe did.
    fn plot_recorded(&mut self, run: usize, device: usize, channel: usize, target: usize) {
        let Some(project) = self.project() else { return };

        let Some(found) = self.runs.get(run) else { return };
        let Some(found_device) = found.devices.get(device) else { return };
        let Some(found_channel) = found_device.channels.get(channel) else { return };

        let started = found.started;
        let source = found_channel.id;
        let reference = ChannelRef {
            // Said with the run, because the same rig recorded twice gives two
            // devices of the same name holding different data.
            device: format!("run {} · {}", found.id, found_device.name),
            channel: found_channel.name.clone(),
        };

        let archive = match lumberdaq::history::Archive::open(&project.database_path()) {
            Ok(archive) => archive,
            Err(problem) => {
                self.note(format!("could not read the results: {}", problem));
                return;
            }
        };

        let readings = match archive.readings(source) {
            Ok(readings) => readings,
            Err(problem) => {
                self.note(format!("could not read {}: {}", reference, problem));
                return;
            }
        };

        if readings.is_empty() {
            self.note(format!("{} has no readings", reference));
            return;
        }

        let count = readings.len();
        if let Some(plot) = self.view_plots.get_mut(target) {
            plot.add_recorded(source, reference.clone(), &readings, started);
        }
        self.settle_plots();
        self.note(format!("{} readings of {}", count, reference));
    }

    /// The recorded runs, as a tree of run, device and channel.
    ///
    /// The same shape as the device tree it replaces, so the pane reads the
    /// same way in either mode: something to open, something under it, and a
    /// channel at the bottom with its unit beside it.
    fn run_tree(&self) -> Element<'_, Message> {
        let refresh = hint(
            button(refresh_cw().size(14))
                .style(button::text)
                .padding(4)
                .on_press(Message::RunsRefreshed),
            "Look for new runs",
        );

        let body: Element<'_, Message> = match self.runs.is_empty() {
            true => text("Nothing recorded in this project yet.").size(13).into(),
            false => column(self.runs.iter().enumerate().map(|(index, run)| {
                let heading = row![
                    button(match run.expanded {
                        true => chevron_down().size(14),
                        false => chevron_right().size(14),
                    })
                    .style(button::text)
                    .padding(4)
                    .on_press(Message::ToggleRun(index)),
                    // Local time, not the UTC the file keeps: this is the one
                    // place a person is matching what they see against when
                    // they remember running it.
                    button(
                        text(
                            run.started
                                .with_timezone(&Local)
                                .format("%d %b %H:%M:%S")
                                .to_string(),
                        )
                        .size(14),
                    )
                    .style(match self.selected == Some(Selection::Run(index)) {
                        true => button::primary,
                        false => button::text,
                    })
                    .padding(4)
                    .width(Fill)
                    .on_press(Message::RunSelected(index)),
                ]
                .align_y(Center);
                let heading = MouseArea::new(heading)
                    .on_right_press(Message::ContextOpened(ContextMenu::Run(index)));

                let mut entry = column![heading];

                if run.expanded {
                    entry = entry.push(
                        column(run.devices.iter().enumerate().map(|(at, device)| {
                            let heading = row![
                                button(match device.expanded {
                                    true => chevron_down().size(12),
                                    false => chevron_right().size(12),
                                })
                                .style(button::text)
                                .padding(4)
                                .on_press(Message::ToggleRunDevice(index, at)),
                                button(text(&device.name).size(14))
                                    .style(
                                        match self.selected
                                            == Some(Selection::RunDevice(index, at))
                                        {
                                            true => button::primary,
                                            false => button::text,
                                        },
                                    )
                                    .padding(4)
                                    .width(Fill)
                                    .on_press(Message::RunDeviceSelected(index, at)),
                            ]
                            .align_y(Center);
                            let heading = MouseArea::new(heading).on_right_press(
                                Message::ContextOpened(ContextMenu::RunDevice(index, at)),
                            );

                            let mut under = column![heading];

                            if device.expanded {
                                under = under.push(
                                    column(device.channels.iter().enumerate().map(
                                        |(position, channel)| {
                                            let drawn = self.view_plots.iter().any(|plot| {
                                                plot.channels
                                                    .iter()
                                                    .any(|t| t.source == Some(channel.id))
                                            });
                                            let picked = self.selected
                                                == Some(Selection::RunChannel(
                                                    index, at, position,
                                                ));

                                            // Selected is what you are looking
                                            // at; drawn is what is on a plot.
                                            // Different things, so different
                                            // weights rather than one colour
                                            // meaning both.
                                            let look = match (picked, drawn) {
                                                (true, _) => RowLook::Picked,
                                                (false, true) => RowLook::Drawn,
                                                (false, false) => RowLook::Plain,
                                            };

                                            channel_row(
                                                row![
                                                    square_arrow_right().size(11),
                                                    text(&channel.name).size(14),
                                                    space::horizontal(),
                                                    text(match channel.unit.is_empty() {
                                                        true => "-".to_string(),
                                                        false => channel.unit.clone(),
                                                    })
                                                    .size(12),
                                                ]
                                                .spacing(8)
                                                .align_y(Center)
                                                .into(),
                                                look,
                                                Message::RunChannelSelected(index, at, position),
                                                Message::ContextOpened(ContextMenu::RunChannel(
                                                    index, at, position,
                                                )),
                                            )
                                        },
                                    ))
                                    .spacing(4)
                                    .padding(padding::left(14)),
                                );
                            }
                            under.into()
                        }))
                        .spacing(4)
                        .padding(padding::left(14)),
                    );
                }
                entry.into()
            }))
            .spacing(5)
            .into(),
        };

        self.pane(row![text("Runs"), space::horizontal(), refresh].align_y(Center), body)
    }

    /// Read what the project's results file holds, as far as its channels.
    ///
    /// Only the shape: runs, their devices, their channels and the names of
    /// each. That is a handful of small queries whatever the run was, where
    /// the readings themselves could be millions of rows - so those wait until
    /// something asks to draw them.
    ///
    /// A missing file is not a problem worth an error: a project that has
    /// never been recorded has nothing to view, which is a fact rather than a
    /// fault.
    fn load_runs(&mut self) {
        self.runs.clear();

        let Some(project) = self.project() else { return };
        let database = project.database_path();
        if !database.exists() {
            self.note("nothing recorded in this project yet".to_string());
            return;
        }

        let archive = match lumberdaq::history::Archive::open(&database) {
            Ok(archive) => archive,
            Err(problem) => {
                self.note(format!("could not read {}: {}", database.display(), problem));
                return;
            }
        };

        let runs = match archive.runs() {
            Ok(runs) => runs,
            Err(problem) => {
                self.note(format!("could not read the runs: {}", problem));
                return;
            }
        };

        for run in runs {
            let mut devices = Vec::new();
            for device in archive.devices(run.id).unwrap_or_default() {
                let channels = archive
                    .channels(device.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|channel| ViewChannel {
                        readings: archive.reading_count(channel.id).unwrap_or(0),
                        id: channel.id,
                        name: channel.name,
                        unit: channel.unit,
                    })
                    .collect();

                devices.push(ViewDevice {
                    name: device.name,
                    hardware: device.hardware,
                    expanded: true,
                    channels,
                });
            }

            self.runs.push(ViewRun {
                id: run.id,
                started: run.started,
                expanded: true,
                devices,
            });
        }

        self.note(format!(
            "{} run{} in {}",
            self.runs.len(),
            match self.runs.len() {
                1 => "",
                _ => "s",
            },
            database.display()
        ));
    }

    /// What deleting would delete, given what is selected.
    ///
    /// One button in the configuration heading rather than one at the foot of
    /// each panel: everything that can be deleted is deleted the same way, and
    /// the control does not move about depending on what is being looked at.
    /// `None` greys it, which is the answer for a recorded run - the file is
    /// not ours to edit - and for the settings that belong to every plot.
    fn delete_selected(&self) -> Option<Message> {
        let editable = self.rig_editable();

        match self.selected? {
            Selection::Device(index) => editable.then_some(Message::DeviceDeleted(index)),
            Selection::Channel(device, channel) => {
                editable.then_some(Message::ChannelDeleted(device, channel))
            }
            Selection::Calculated => editable.then_some(Message::CalculatedDeleted),
            Selection::CalculatedChannel(channel) => {
                editable.then_some(Message::CalculatedChannelDeleted(channel))
            }
            Selection::Plot(index) | Selection::ViewPlot(index) => {
                Some(Message::PlotDeleted(index))
            }
            // Recorded data, and the settings that are about every plot at
            // once. Neither is a thing to delete.
            Selection::AllPlots
            | Selection::Run(_)
            | Selection::RunDevice(..)
            | Selection::RunChannel(..) => None,
        }
    }

    /// The calculated device's row in the tree, to be kept in step by hand.
    ///
    /// By hand rather than by rebuilding from the config, because the tree
    /// holds what the config does not: which devices are open, and each
    /// channel's latest reading and sample count. Rebuilding is how those get
    /// thrown away without anybody noticing, in the middle of a run.
    fn calculated_row(&mut self) -> Option<&mut AppDevice> {
        self.devices.iter_mut().find(|device| device.kind == DeviceKind::Calculated)
    }

    /// The calculated device's own settings: what it is called.
    ///
    /// Short because there is little to it. It owns no hardware, so there is no
    /// port, no range and no read interval to set — its channels produce a
    /// value whenever their slowest input does, which is lumberdaq's rule and
    /// not something to choose here.
    fn calculated_settings(&self) -> Element<'_, Message> {
        let Some(calculated) = self.config.calculated.as_ref() else {
            return text("Nothing selected.").size(14).into();
        };

        column![
            column![
                text("Calculated channels").size(16),
                text("Worked out from measured channels, and recorded beside them.").size(13),
            ]
            .spacing(2),
            self.rig_field("Name", &calculated.info.name, Message::CalculatedRenamed),
            row![
                text(format!(
                    "{} channel{}",
                    calculated.channels.len(),
                    match calculated.channels.len() {
                        1 => "",
                        _ => "s",
                    }
                ))
                .size(13),
                space::horizontal(),
                hint(
                    button(circle_plus().size(14))
                        .style(button::text)
                        .padding(4)
                        .on_press_maybe(match self.rig_editable() {
                            true => Some(Message::CalculatedChannelAdded),
                            false => None,
                        }),
                    "Add a calculated channel",
                ),
            ]
            .align_y(Center),
        ]
        .spacing(12)
        .into()
    }

    /// One calculated channel: what it is, and what it is worked out from.
    fn calculated_channel_settings(&self, at: usize) -> Element<'_, Message> {
        let Some(calculated) = self.config.calculated.as_ref() else {
            return text("Nothing selected.").size(14).into();
        };
        let Some(channel) = calculated.channels.get(at) else {
            return text("Nothing selected.").size(14).into();
        };

        // What lumberdaq would say if this were run, said now instead: the same
        // check a run makes, so anything passing here will build. Run on every
        // keystroke, which is what `validate` exists for - an equation is a
        // string read at run time, so nothing about it is settled at compile
        // time and there is no cheaper moment to find out.
        let problem = channel.validate().err().map(|problem| problem.to_string());

        // Named inputs rather than the equation naming devices and channels
        // itself: channel names have spaces in them, and quoting those inside
        // an expression is miserable. The short name is what appears in the
        // equation; the dropdown says which measured channel it reads.
        let editable = self.rig_editable();
        let rows = column(channel.inputs.iter().map(|(name, source)| {
            let renaming = name.clone();
            let choosing = name.clone();
            let deleting = name.clone();

            row![
                match editable {
                    true => text_input("name", name)
                        .on_input(move |to| {
                            Message::CalculatedInputRenamed(at, renaming.clone(), to)
                        })
                        .size(14)
                        .width(70)
                        .style(field_style),
                    false => text_input("name", name).size(14).width(70).style(field_style),
                },
                text("=").size(14),
                match editable {
                    true => Element::from(
                        pick_list(self.available.as_slice(), Some(source), move |chosen| {
                            Message::CalculatedInputSourceChosen(at, choosing.clone(), chosen)
                        })
                        .style(field_pick_style)
                        .text_size(14)
                        .width(Fill),
                    ),
                    false => text(source.to_string()).size(14).width(Fill).into(),
                },
                hint(
                    button(trash_two().size(14))
                        .style(button::text)
                        .padding(4)
                        .on_press_maybe(match editable {
                            true => Some(Message::CalculatedInputDeleted(at, deleting.clone())),
                            false => None,
                        }),
                    "Remove this input",
                ),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        }))
        .spacing(4);

        let inputs: Element<'_, Message> = match channel.inputs.is_empty() {
            true => text("No inputs yet, so the equation has nothing to read.").size(12).into(),
            false => rows.into(),
        };

        column![
            column![
                text("Calculated channel").size(16),
                text("Worked out whenever its slowest input produces a value.").size(12),
            ]
            .spacing(2),
            self.rig_field("Name", &channel.info.name, move |name| {
                Message::CalculatedChannelRenamed(at, name)
            }),
            self.rig_field_explained(
                "Unit",
                Some("What the equation works out to, which only you know: it is the result of the equation, not of any one input."),
                &channel.info.unit,
                move |unit| Message::CalculatedChannelUnitEdited(at, unit),
            ),
            self.rig_field_checked(
                "Equation",
                Some("Written in terms of the input names below, such as (v + 1) * 2.5."),
                &channel.equation,
                problem,
                Some(FormulaHelp::Equation),
                move |equation| Message::CalculatedEquationEdited(at, equation),
            ),
            column![
                row![
                    field_label("Inputs"),
                    space::horizontal(),
                    hint(
                        button(circle_plus().size(14))
                            .style(button::text)
                            .padding(4)
                            .on_press_maybe(match editable {
                                true => Some(Message::CalculatedInputAdded(at)),
                                false => None,
                            }),
                        "Add an input to this equation",
                    ),
                ]
                .align_y(Center),
                inputs,
            ]
            .spacing(2),
        ]
        .spacing(12)
        .into()
    }

    /// What kinds of device can be added right now.
    ///
    /// The hardware backends always, and the calculated device only while
    /// there is not one: a second would have nowhere to live, since the config
    /// holds a single `Option` rather than a list.
    fn addable_kinds(&self) -> Vec<&'static str> {
        let mut kinds = HardwareConfig::TYPE_NAMES.to_vec();
        if self.config.calculated.is_none() {
            kinds.push(CALCULATED_TYPE);
        }
        kinds
    }

    /// The dialog for how the application itself looks.
    ///
    /// Everything in here takes effect on the spot and is written out on the
    /// spot: there is no OK to press, because there is nothing to agree to.
    /// Seeing the change is the confirmation, and closing is just closing.
    fn settings_dialog(&self) -> Element<'_, Message> {
        let step = self.settings.font_step.clamp(1, settings::FONT_STEPS);

        container(
            column![
                row![
                    text("Settings").size(16),
                    space::horizontal(),
                    button(x().size(14))
                        .style(button::text)
                        .padding(2)
                        .on_press(Message::SettingsClosed),
                ]
                .align_y(Center),
                column![
                    text("Theme").size(13),
                    pick_list(settings::themes(), Some(self.settings.theme()), Message::ThemeChanged)
                        .style(field_pick_style)
                        .text_size(14)
                        .width(Fill),
                ]
                .spacing(6),
                column![
                    row![
                        text("Text size").size(13),
                        space::horizontal(),
                        text(format!("{} of {}", step, settings::FONT_STEPS)).size(13),
                    ],
                    // A slider rather than a list: the steps are ordered and
                    // the only question is bigger or smaller, which a slider
                    // answers without opening anything. It also sidesteps the
                    // question of which way a dropdown would open, which in a
                    // dialog is never a settled one.
                    slider(1..=settings::FONT_STEPS, step, Message::FontStepChanged),
                    text("Scales the whole interface, so spacing and icons keep up with the text.")
                        .size(11),
                ]
                .spacing(6),
            ]
            .spacing(16)
            .width(320),
        )
        .padding(20)
        .style(dialog_style)
        .into()
    }

    /// What can be written in a formula, and where its values come from.
    ///
    /// Beside the field rather than in the manual: the moment somebody wants
    /// the list of functions is the moment they are looking at the box, and a
    /// sentence of explanation above the field is not that list.
    ///
    /// Every operator and function named in these documents was tried against
    /// the equation engine rather than taken from its documentation. A help
    /// page listing something that does not work is worse than no help page.
    fn formula_help_dialog(&self, which: FormulaHelp) -> Element<'_, Message> {
        // Sizes given rather than derived. `Settings::with_text_size` makes the
        // first heading twice the body, which on a page that is mostly headings
        // reads as a series of titles with fragments between them.
        let settings = markdown::Settings {
            text_size: 13.0.into(),
            h1_size: 16.0.into(),
            h2_size: 15.0.into(),
            h3_size: 14.0.into(),
            h4_size: 13.0.into(),
            h5_size: 13.0.into(),
            h6_size: 13.0.into(),
            code_size: 12.0.into(),
            spacing: 12.0.into(),
            style: markdown::Style::from(&self.settings.theme()),
        };

        container(
            column![
                row![
                    text(which.title()).size(16),
                    space::horizontal(),
                    button(x().size(14))
                        .style(button::text)
                        .padding(2)
                        .on_press(Message::FormulaHelpClosed),
                ]
                .align_y(Center),
                scrollable(markdown::view(&self.help_items, settings).map(move |_link| {
                    // Nothing here is a link. Reopening the help that is
                    // already open is the honest way to spell "no message",
                    // and needs no variant that can never be sent.
                    Message::FormulaHelpOpened(which)
                }))
                // Room for the scrollbar, rather than padding pretending to be
                // room. Without a spacing iced draws the bar *over* the
                // content, so a right padding of ten and a bar ten wide leave
                // the text touching it exactly.
                .spacing(10)
                .height(Fill),
            ]
            .spacing(14)
            .width(630)
            .height(Fill),
        )
        .max_height(840)
        .padding(20)
        .style(dialog_style)
        .into()
    }

    /// The dialog for adding a device.
    ///
    /// A type has to be chosen before there is anything to add: it decides
    /// what the rest of the settings are, so there is no sensible device to
    /// create without one. Hence the Add button being dead until it is picked.
    fn add_device_dialog(&self, chosen: Option<&'static str>) -> Element<'_, Message> {
        container(
            column![
                row![
                    text("Add device").size(16),
                    space::horizontal(),
                    button(x().size(14))
                        .style(button::text)
                        .padding(2)
                        .on_press(Message::AddDeviceCancelled),
                ]
                .align_y(Center),
                text("What kind of hardware is it?").size(13),
                pick_list(self.addable_kinds(), chosen, Message::AddDeviceTypeChosen)
                .placeholder("Choose a type")
                .style(field_pick_style)
                .style(field_pick_style)
                .text_size(14)
                .width(Fill),
                row![
                    space::horizontal(),
                    button(text("Add").size(14))
                        .padding(6)
                        .on_press_maybe(chosen.map(|_| Message::AddDeviceConfirmed)),
                ],
            ]
            .spacing(12)
            .width(320),
        )
        .padding(16)
        .style(dialog_style)
        .into()
    }

    /// The plots on offer beside an open menu.
    ///
    /// Named rather than numbered, because a plot's name is what somebody gave
    /// it and its number is bookkeeping. A rig with no plots yet says so
    /// instead of offering an empty list.
    fn plot_menu_entries(&self) -> Element<'_, Message> {
        let plots = self.plot_names();
        match plots.is_empty() {
            true => look::menu(vec![MenuItem::Entry("No plots yet", None)], 150.0),
            false => look::menu(
                plots
                    .into_iter()
                    .map(|(at, name)| MenuItem::Owned(name, Some(Message::PlotChosen(at))))
                    .collect(),
                150.0,
            ),
        }
    }

    /// What the open right click menu offers, for whatever it was opened on.
    ///
    /// Every entry stays in place and is greyed when it cannot be used, so the
    /// menu is the same shape each time — "Add channel" being unavailable
    /// during a run is worth saying, where a menu that quietly loses an entry
    /// just looks different.
    fn context_entries(&self, target: ContextMenu) -> Element<'_, Message> {
        let editable = self.rig_editable();
        let when = |allowed: bool, message: Message| match allowed {
            true => Some(message),
            false => None,
        };

        // Opening and shutting the tree comes first and is ruled off from
        // what follows: it acts on the whole list rather than on the row that
        // was clicked, so it should not sit among the entries that do.
        let tree_top = |mut rest: Vec<(&'static str, Option<Message>)>| {
            let mut entries = vec![
                ("Expand all", Some(Message::ExpandAll(true))),
                ("Collapse all", Some(Message::ExpandAll(false))),
                DIVIDER,
            ];
            entries.append(&mut rest);
            entries
        };

        context_menu(match target {
            ContextMenu::Device(index) => tree_top(vec![
                ("Add channel", when(editable, Message::ChannelAdded(index))),
                ("Delete device", when(editable, Message::DeviceDeleted(index))),
            ]),
            ContextMenu::Channel(device, channel) => tree_top(vec![
                ("Add to plot", Some(Message::PlotMenuOpened)),
                ("Delete channel", when(editable, Message::ChannelDeleted(device, channel))),
            ]),
            ContextMenu::Calculated => tree_top(vec![
                ("Add channel", when(editable, Message::CalculatedChannelAdded)),
                ("Delete device", when(editable, Message::CalculatedDeleted)),
            ]),
            ContextMenu::CalculatedChannel(channel) => tree_top(vec![
                ("Add to plot", Some(Message::PlotMenuOpened)),
                ("Delete channel", when(editable, Message::CalculatedChannelDeleted(channel))),
            ]),
            ContextMenu::RunChannel(..) => {
                tree_top(vec![("Add to plot", Some(Message::PlotMenuOpened))])
            }
            ContextMenu::Run(..) | ContextMenu::RunDevice(..) => {
                tree_top(vec![("Add to plot", Some(Message::PlotMenuOpened))])
            }
            // A plot is the interface's own, not the rig's, so it goes whether
            // or not a run is in progress.
            ContextMenu::Plot(index) => {
                vec![("Delete plot", Some(Message::PlotDeleted(index)))]
            }
        })
    }

    /// Whether the rig's settings can be changed at the moment.
    ///
    /// A run holds the devices on its own thread and was built from the config
    /// as it stood when it started, so editing during one would be describing
    /// a rig that is not the one being read. Shown either way: what a device is
    /// set to is worth seeing most while it is running.
    fn rig_editable(&self) -> bool {
        self.run == RunState::Stopped
    }

    /// One field of the rig's configuration.
    ///
    /// A `text_input` with no `on_input` is iced's own way of saying a field
    /// cannot be typed into: it still shows its value and can be read from.
    fn rig_field<'a>(
        &self,
        label: &'a str,
        value: &'a str,
        on_input: impl Fn(String) -> Message + 'a,
    ) -> Element<'a, Message> {
        self.rig_field_explained(label, None, value, on_input)
    }

    /// The same, with an explanation reachable by hovering the label.
    fn rig_field_explained<'a>(
        &self,
        label: &'a str,
        explanation: Option<&'a str>,
        value: &'a str,
        on_input: impl Fn(String) -> Message + 'a,
    ) -> Element<'a, Message> {
        self.rig_field_checked(label, explanation, value, None, None, on_input)
    }

    /// The same again, saying whether what is in it will do.
    ///
    /// `problem` is what is wrong with the value, if anything. A field that is
    /// wrong is drawn with a red border and carries the reason as a tooltip,
    /// so the panel keeps its shape while somebody types and the explanation
    /// is there when it is wanted.
    fn rig_field_checked<'a>(
        &self,
        label: &'a str,
        explanation: Option<&'a str>,
        value: &'a str,
        problem: Option<String>,
        help: Option<FormulaHelp>,
        on_input: impl Fn(String) -> Message + 'a,
    ) -> Element<'a, Message> {
        let style = match problem {
            Some(_) => field_error_style,
            None => field_style,
        };

        let field = match self.rig_editable() {
            true => text_input(label, value).on_input(on_input).size(14).style(style),
            false => text_input(label, value).size(14).style(style),
        };

        // Beside the box rather than in the paragraph above it. What somebody
        // wants when they are stuck is the list of operators, and a sentence of
        // explanation is not that; this is where the room for the list is.
        //
        // Constant per field rather than appearing and disappearing, so the
        // shape of the tree at this position never changes and the box cannot
        // lose the caret to it.
        let entry: Element<'a, Message> = match help {
            None => explaining(field.into(), problem),
            Some(which) => row![
                explaining(field.into(), problem),
                button(circle_help().size(14))
                    .style(button::text)
                    .padding(4)
                    .on_press(Message::FormulaHelpOpened(which)),
            ]
            .spacing(4)
            .align_y(Center)
            .into(),
        };

        column![
            match explanation {
                Some(explanation) => labelled(label, explanation),
                None => field_label(label),
            },
            entry,
        ]
        .spacing(2)
        .into()
    }

    /// The settings for one device.
    fn device_settings(&self, index: usize) -> Element<'_, Message> {
        let Some(device) = self.config.devices.get(index) else {
            return text("Nothing selected.").size(14).into();
        };

        // A config written elsewhere may hold an interval that is not one of
        // the choices, and it is not this program's business to round it.
        let mut intervals = READ_INTERVALS.to_vec();
        if !intervals.contains(&device.read_interval_ms) {
            intervals.push(device.read_interval_ms);
            intervals.sort_unstable();
        }

        // A pick_list has no disabled state, so a run shows the value as text.
        let interval: Element<'_, Message> = match self.rig_editable() {
            true => pick_list(intervals, Some(device.read_interval_ms), move |interval| {
                Message::DeviceIntervalChanged(index, interval)
            })
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(format!("{} ms", device.read_interval_ms)).size(14).into(),
        };

        column![
            // What the hardware turned out to be if it could say, and what
            // the config claims otherwise. Nobody types this: a device saying
            // it is a USB-6002 is not to be argued with.
            read_only(
                "Device type",
                match &self.model {
                    Some(model) => model.clone(),
                    None => device.hardware.describe(),
                },
                Tone::Plain,
            ),
            // The same fact as the dot beside the device in the tree, said in
            // words. Also not something to ask for - it is the answer the
            // hardware gave.
            {
                let connected = self
                    .devices
                    .iter()
                    .find(|found| found.kind == DeviceKind::Measured(index))
                    .and_then(|found| found.connected);

                read_only(
                    "Status",
                    match connected {
                        Some(true) => "Connected".to_string(),
                        Some(false) => "Disconnected".to_string(),
                        None => "Not tried yet".to_string(),
                    },
                    match connected {
                        Some(true) => Tone::Good,
                        Some(false) => Tone::Bad,
                        None => Tone::Plain,
                    },
                )
            },
            self.rig_field("Name", &device.info.name, move |name| {
                Message::DeviceRenamed(index, name)
            }),
            // Named for what it is: how often lumberdaq collects, which is the
            // sample rate only for hardware that samples on being read.
            column![labelled("Read interval (ms)", "How often lumberdaq collects from this device. For hardware that samples when read, this is also its sample rate."), interval].spacing(2),
            // Below the shared fields: everything from here differs by
            // backend, and there is no form that fits all of them.
            match &device.hardware {
                HardwareConfig::MockHardware(mock) => self.mock_device_settings(index, mock),
                HardwareConfig::SerialStream(serial) => {
                    self.serial_device_settings(index, serial)
                }
                HardwareConfig::NiDaqmx(ni) => self.ni_device_settings(index, ni),
                other => text(format!("{} settings are not editable yet.", other.type_name()))
                    .size(13)
                    .into(),
            },
            // The backend's own verdict on its channels, rather than rules
            // repeated here. Shown as it is found, so a differential pair on
            // an even channel is said now and not at connect.
            match device.hardware.channel_problem() {
                Some(problem) => Element::from(text(problem).size(13).color(PALETTE[1])),
                None => space::horizontal().width(0).into(),
            },
            // Only when stopped, like every other change to the rig, and it
            // takes its channels off the plots with it.
            match self.rig_editable() {
                true => Element::from(space::horizontal().width(0)),
                false => field_label("Stop the run to change these"),
            },
        ]
        .spacing(8)
        .into()
    }

    /// The settings a serial device has that others do not.
    fn serial_device_settings<'a>(
        &'a self,
        index: usize,
        serial: &'a lumberdaq::hardware::serial_stream::SerialStreamConfig,
    ) -> Element<'a, Message> {
        // The ports that are plugged in, plus whatever the config names if it
        // is not among them. A rig configured at the bench and opened on a
        // laptop should still show the port it is set to, rather than looking
        // blank as though nobody had chosen.
        let mut options: Vec<String> = self.ports.iter().map(|port| port.label()).collect();
        let current = self
            .ports
            .iter()
            .find(|port| port.name == serial.port)
            .map(|port| port.label())
            .unwrap_or_else(|| serial.port.clone());

        if !serial.port.is_empty() && !options.contains(&current) {
            options.push(current.clone());
        }

        // Over the whole setup rather than this one field, because the
        // dropdown is not the only way a port gets named twice: a project
        // loaded from disk, a library merged in, and a hand edited file all
        // arrive without passing through it.
        let taken_by = self.config.devices.get(index).and_then(|device| {
            let name = device.info.name.clone();
            self.config
                .address_clashes()
                .into_iter()
                .find(|clash| clash.device == name)
                .map(|clash| clash.taken_by)
        });

        let port_style: fn(&Theme, pick_list::Status) -> pick_list::Style = match taken_by {
            Some(_) => field_pick_warning_style,
            None => field_pick_style,
        };

        // Named rather than only marked. "In use" tells somebody to go and
        // find the other device; saying which one means not having to.
        let clash = taken_by.map(|by| format!("COM port in use by {}", by));

        let port: Element<'_, Message> = match self.rig_editable() {
            true => pick_list(options, Some(current), move |label| {
                // The label carries the product name for the human; the config
                // wants only the port, so it is taken back off here.
                let port = label.split(" — ").next().unwrap_or(&label).to_string();
                Message::SerialPortChosen(index, port)
            })
            .placeholder("Choose a port")
            .style(port_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(current).size(14).into(),
        };

        let mut bauds = BAUD_RATES.to_vec();
        if !bauds.contains(&serial.baudrate) {
            bauds.push(serial.baudrate);
            bauds.sort_unstable();
        }

        // Only `Unreadable` colours the box. Bytes arriving that make no
        // sense is the signature of a wrong rate; a frame that matched but
        // would not read proves the rate is *right* and the fault is a
        // channel; and silence proves nothing at all.
        let baud_is_wrong = matches!(
            &self.tested,
            Some((tested, StreamCheck::Unreadable)) if *tested == index
        );
        // Through a function pointer because the two are different types until
        // they are coerced, and a `match` needs them to be one type.
        let baud_style: fn(&Theme, pick_list::Status) -> pick_list::Style = match baud_is_wrong {
            true => field_pick_warning_style,
            false => field_pick_style,
        };

        let baud: Element<'_, Message> = match self.rig_editable() {
            true => pick_list(bauds, Some(serial.baudrate), move |baudrate| {
                Message::SerialBaudChosen(index, baudrate)
            })
            .style(baud_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(serial.baudrate.to_string()).size(14).into(),
        };

        column![
            column![
                field_label("Port"),
                // Beside the list rather than in a menu: the usual reason a
                // port is missing is that it was not plugged in yet, and the
                // answer is to plug it in and look again.
                row![
                    // Wrapped whether or not there is anything to say, so what
                    // sits at this position keeps its type as a clash appears
                    // and clears - the same reason the equation field is
                    // always wrapped.
                    warning_about(port, clash),
                    hint(
                        button(refresh_cw().size(14))
                            .style(button::text)
                            .padding(4)
                            .on_press(Message::PortsRefreshed),
                        "Look for serial ports again",
                    ),
                ]
                .spacing(4)
                .align_y(Center),
            ]
            .spacing(2),
            match self.ports.is_empty() {
                true => text("No serial ports found.").size(13),
                false => text(format!("{} port(s) found", self.ports.len())).size(13),
            },
            column![
                field_label("Baud rate"),
                // Beside the list for the same reason the refresh is beside
                // the ports: a rate cannot be worked out by looking, only by
                // trying, so the way to find out is right where it is set.
                row![
                    baud,
                    hint(
                        // A different mark while it listens, because five
                        // seconds of a button that looks exactly as it did
                        // before reads as a button that did nothing.
                        button(
                            match self.connection_test.is_some() {
                                true => loader_pinwheel(),
                                false => check(),
                            }
                            .size(14)
                        )
                            .style(button::text)
                            .padding(4)
                            // Nothing while a run owns the port, or while the
                            // last test is still listening to it.
                            .on_press_maybe(
                                match self.run == RunState::Stopped
                                    && self.connection_test.is_none()
                                {
                                    true => Some(Message::StreamTested(index)),
                                    false => None,
                                }
                            ),
                        "Listen to the port and see whether these settings read it",
                    ),
                ]
                .spacing(4)
                .align_y(Center),
            ]
            .spacing(2),
            // Free text because it is a regular expression: there is no list
            // of the right answers, and the one in the config may be anything.
            self.rig_field("Frame pattern", &serial.frame_pattern, move |pattern| {
                Message::SerialPatternEdited(index, pattern)
            }),
        ]
        .spacing(8)
        .into()
    }

    /// The settings an NI device has that others do not.
    fn ni_device_settings<'a>(
        &'a self,
        index: usize,
        ni: &'a lumberdaq::hardware::ni_daqmx::NiDaqmxConfig,
    ) -> Element<'a, Message> {
        // Offered as a list when the driver can say what is attached, and
        // typed when it cannot — which is every machine without NI software,
        // where the name still has to be enterable.
        let chooser: Element<'_, Message> = match self.ni_devices.is_empty()
            || !self.rig_editable()
        {
            true => self.rig_field("Device", &ni.device, move |name| {
                Message::NiDeviceEdited(index, name)
            }),
            false => {
                let mut names = self.ni_devices.clone();
                if !ni.device.is_empty() && !names.contains(&ni.device) {
                    names.push(ni.device.clone());
                }

                column![
                    field_label("Device"),
                    pick_list(names, Some(ni.device.clone()), move |name| {
                        Message::NiDeviceEdited(index, name)
                    })
                    .style(field_pick_style)
                    .text_size(14)
                    .width(Fill),
                ]
                .spacing(2)
                .into()
            }
        };

        column![
            row![
                chooser,
                hint(
                    button(refresh_cw().size(14))
                        .style(button::text)
                        .padding(4)
                        .on_press(Message::NiDevicesRefreshed),
                    "Look for NI devices again",
                ),
            ]
            .spacing(4)
            .align_y(iced::Bottom),
            // Said plainly, because it decides what the channel list offers and
            // which input a differential pair takes.
            match self.ni_inputs {
                Some(inputs) => text(format!("{} analog inputs", inputs)).size(13),
                None => text("Input count unknown until the device is attached.").size(13),
            },
            match self.ni_devices.is_empty() {
                true => text("No NI devices found. Is the DAQmx runtime installed?").size(13),
                false => text(format!("{} device(s) found", self.ni_devices.len())).size(13),
            },
        ]
        .spacing(8)
        .into()
    }

    /// The settings a mock device has that others do not.
    fn mock_device_settings<'a>(
        &'a self,
        index: usize,
        mock: &'a lumberdaq::hardware::mock_hardware::MockHardwareConfig,
    ) -> Element<'a, Message> {
        let mode = match mock.acquisition {
            mock_hardware::Acquisition::Polled => MockMode::Polled,
            mock_hardware::Acquisition::Streaming { .. } => MockMode::Streaming,
        };

        let mode_field: Element<'_, Message> = match self.rig_editable() {
            true => pick_list(vec![MockMode::Polled, MockMode::Streaming], Some(mode), move |mode| {
                Message::MockModeChanged(index, mode)
            })
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(mode.to_string()).size(14).into(),
        };

        // Only streaming keeps a schedule of its own; a polled device samples
        // when it is read, so there would be nothing for this to say.
        let interval: Element<'_, Message> = match mock.acquisition {
            mock_hardware::Acquisition::Polled => space::horizontal().width(0).into(),
            mock_hardware::Acquisition::Streaming { sample_interval_ms } => {
                let mut choices = MOCK_INTERVALS.to_vec();
                if !choices.contains(&sample_interval_ms) {
                    choices.push(sample_interval_ms);
                    choices.sort_unstable();
                }

                let field: Element<'_, Message> = match self.rig_editable() {
                    true => pick_list(choices, Some(sample_interval_ms), move |interval| {
                        Message::MockIntervalChanged(index, interval)
                    })
                    .style(field_pick_style)
                    .text_size(14)
                    .width(Fill)
                    .into(),
                    false => text(format!("{} ms", sample_interval_ms)).size(14).into(),
                };

                column![field_label("Sample interval (ms)"), field].spacing(2).into()
            }
        };

        column![column![labelled("Acquisition", "Polled takes a sample when asked. Streaming keeps its own schedule and is drained."), mode_field].spacing(2), interval]
            .spacing(8)
            .into()
    }

    /// Which field of a serial frame a channel reads.
    ///
    /// Typed rather than picked from a list: a frame can have as many fields
    /// as the device sends, so any list would be a guess at where to stop.
    fn serial_channel_settings(&self, device: usize, channel: usize) -> Element<'_, Message> {
        column![
            self.rig_field_explained(
                "Frame Index",
                Some("Which field of the frame this channel reads, counting from zero."),
                &self.number_draft,
                move |text| Message::SerialIndexEdited(device, channel, text),
            ),
        ]
        .spacing(8)
        .into()
    }

    /// Which input a Pico channel reads, and how.
    fn pico_channel_settings(
        &self,
        device: usize,
        channel: usize,
        pico: &pico_hrdl::PicoHrdlChannel,
    ) -> Element<'_, Message> {
        // What the attached unit actually has, or everything the backend allows
        // when nothing has said. Offering sixteen on an ADC-20 invites choosing
        // an input that is not there, which is only found out at connect.
        let highest = self.pico_inputs.unwrap_or(pico_hrdl::HIGHEST_CHANNEL);
        let mut numbers: Vec<u16> = (pico_hrdl::LOWEST_CHANNEL..=highest).collect();
        if !numbers.contains(&pico.channel) {
            numbers.push(pico.channel);
            numbers.sort_unstable();
        }
        let editable = self.rig_editable();

        let number: Element<'_, Message> = match editable {
            true => pick_list(numbers, Some(pico.channel), move |number| {
                Message::PicoChannelChosen(device, channel, number)
            })
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(pico.channel.to_string()).size(14).into(),
        };

        let range: Element<'_, Message> = match editable {
            true => pick_list(PICO_RANGES.to_vec(), Some(PicoRange(pico.range)), move |range| {
                Message::PicoRangeChosen(device, channel, range)
            })
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(PicoRange(pico.range).to_string()).size(14).into(),
        };

        column![
            column![field_label("Analog input"), number].spacing(2),
            column![field_label("Range"), range].spacing(2),
            // A differential pair starts on an odd channel and takes the one
            // above it; whether this one may is the backend's to say, and it
            // says so through the problem line on the device.
            checkbox(pico.single_ended)
                .label("Single ended")
                .text_size(14)
                .on_toggle_maybe(match editable {
                    true => Some(move |single| Message::PicoSingleEnded(device, channel, single)),
                    false => None,
                }),
        ]
        .spacing(8)
        .into()
    }

    /// Which input an NI channel reads, and how.
    fn ni_channel_settings(
        &self,
        device: usize,
        channel: usize,
        ni: &lumberdaq::hardware::ni_daqmx::NiDaqmxChannel,
    ) -> Element<'_, Message> {
        let editable = self.rig_editable();
        // The device's own inputs where the driver has said, and a plain
        // sixteen where it has not — a guess either way, but one that stops
        // being a guess as soon as the hardware is attached.
        let mut numbers: Vec<u32> = (0..self.ni_inputs.unwrap_or(16) as u32).collect();
        if !numbers.contains(&ni.channel) {
            numbers.push(ni.channel);
            numbers.sort_unstable();
        }

        let number: Element<'_, Message> = match editable {
            true => pick_list(numbers, Some(ni.channel), move |number| {
                Message::NiChannelChosen(device, channel, number)
            })
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(format!("ai{}", ni.channel)).size(14).into(),
        };

        let mut ranges: Vec<NiRange> = NI_RANGES.iter().copied().map(NiRange).collect();
        if !ranges.iter().any(|range| range.0 == ni.range) {
            ranges.push(NiRange(ni.range));
        }

        let range: Element<'_, Message> = match editable {
            true => pick_list(ranges, Some(NiRange(ni.range)), move |range| {
                Message::NiRangeChosen(device, channel, range)
            })
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(NiRange(ni.range).to_string()).size(14).into(),
        };

        column![
            column![field_label("Analog input (ai)"), number].spacing(2),
            column![text("Range").size(13), range].spacing(2),
            // NI pairs a channel with the one four above it, so on an eight
            // input device only ai0 to ai3 can start a pair.
            checkbox(ni.single_ended)
                .label("Single ended")
                .text_size(14)
                .on_toggle_maybe(match editable {
                    true => Some(move |single| Message::NiSingleEnded(device, channel, single)),
                    false => None,
                }),
        ]
        .spacing(8)
        .into()
    }

    /// What a mock channel generates, and the number that shapes it.
    fn mock_channel_settings<'a>(
        &'a self,
        device: usize,
        channel: usize,
        input: &MockHardwareInput,
    ) -> Element<'a, Message> {
        let kind = MockInputKind::of(input);

        let kind_field: Element<'_, Message> = match self.rig_editable() {
            true => pick_list(
                vec![MockInputKind::Random, MockInputKind::Constant, MockInputKind::Sine],
                Some(kind),
                move |kind| Message::MockInputChanged(device, channel, kind),
            )
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
            false => text(kind.to_string()).size(14).into(),
        };

        let number: Element<'_, Message> = match kind.number_label() {
            None => space::horizontal().width(0).into(),
            // The draft rather than the config's value, so the field can hold
            // "1." while somebody is partway through typing "1.5".
            Some(label) => self.rig_field(label, &self.number_draft, move |text| {
                Message::MockNumberEdited(device, channel, text)
            }),
        };

        column![column![labelled("Input", "What generates this channel's values. Sine and Constant take the number below."), kind_field].spacing(2), number].spacing(8).into()
    }

    /// The settings for one channel.
    fn channel_settings(&self, device: usize, channel: usize) -> Element<'_, Message> {
        // Borrowed from the config rather than copied out of it: a text_input
        // holds its value for as long as the widget lives, which is longer
        // than a local would.
        let Some(info) = self
            .config
            .devices
            .get(device)
            .and_then(|device| device.hardware.channel_info(channel))
        else {
            return text("Nothing selected.").size(14).into();
        };

        // Judged by the library rather than here: building a channel from this
        // description runs exactly the checks a run would, so a formula that
        // passes will also start. Said as it is typed, rather than at connect.
        let problem = match info.scale.is_some() {
            false => None,
            true => Channel::from_info(info.clone()).err().map(|error| error.to_string()),
        };

        column![
            text("Channel").size(16),
            self.rig_field("Name", &info.name, move |name| {
                Message::ChannelRenamed(device, channel, name)
            }),
            // Directly above the unit it produces, because that is the pair:
            // the formula decides what the recorded number is and the unit
            // says what it means. A scale replaces the measurement rather than
            // adding a channel, so this belongs to the channel's own settings
            // rather than to a thing of its own.
            self.rig_field_checked(
                "Formula",
                Some(
                    "The measurement is written x, so x * 5 + 5 records five times the reading \
                     plus five, in the unit below. The raw reading is not kept. Leave empty to \
                     record the measurement as it is.",
                ),
                info.scale.as_ref().map(|scale| scale.equation()).unwrap_or(""),
                problem,
                Some(FormulaHelp::Scale),
                move |equation| Message::ScaleEdited(device, channel, equation),
            ),
            self.rig_field("Unit", &info.unit, move |unit| {
                Message::ChannelUnitChanged(device, channel, unit)
            }),
            // A scale carrying named constants from a sensor definition shows
            // where they came from and what they are, read only: editing those
            // wants a form of its own rather than a list.
            match info.scale.as_ref().and_then(|scale| scale.from()) {
                Some(from) => field_label(from),
                None => Element::from(space::horizontal().width(0)),
            },
            match info.scale.as_ref().and_then(|scale| scale.parameters()) {
                None => Element::from(space::horizontal().width(0)),
                Some(parameters) => column(parameters.iter().map(|(name, value)| {
                    row![
                        text(name.as_str()).size(13),
                        space::horizontal(),
                        text(value.to_string()).size(13)
                    ]
                    .into()
                }))
                .spacing(2)
                .into(),
            },
            // Everything below differs by backend: a mock channel's input, a
            // serial channel's field index, a Pico's channel number and how it
            // pairs for a differential reading. One form cannot ask all of it.
            match self.config.devices.get(device).map(|device| &device.hardware) {
                Some(HardwareConfig::MockHardware(mock)) => match mock.channels.get(channel) {
                    Some(mock_channel) => {
                        self.mock_channel_settings(device, channel, &mock_channel.input)
                    }
                    None => space::horizontal().width(0).into(),
                },
                Some(HardwareConfig::SerialStream(serial)) => match serial.channels.get(channel) {
                    Some(_) => self.serial_channel_settings(device, channel),
                    None => space::horizontal().width(0).into(),
                },
                Some(HardwareConfig::PicoHrdl(pico)) => match pico.channels.get(channel) {
                    Some(pico_channel) => {
                        self.pico_channel_settings(device, channel, pico_channel)
                    }
                    None => space::horizontal().width(0).into(),
                },
                Some(HardwareConfig::NiDaqmx(ni)) => match ni.channels.get(channel) {
                    Some(ni_channel) => self.ni_channel_settings(device, channel, ni_channel),
                    None => space::horizontal().width(0).into(),
                },
                Some(HardwareConfig::None) => space::horizontal().width(0).into(),
                None => space::horizontal().width(0).into(),
            },
            match self.rig_editable() {
                true => Element::from(space::horizontal().width(0)),
                false => field_label("Stop the run to change these"),
            },
        ]
        .spacing(8)
        .into()
    }

    /// The settings that apply to every plot at once.
    fn all_plots_settings(&self) -> Element<'_, Message> {
        let current = self.window_seconds as u64;

        // A layout may have been written with a span that is not one of the
        // choices. Offering only the choices would mean the setting appeared
        // blank, or worse, quietly changed on the first look at it.
        let mut spans = HISTORY_CHOICES.to_vec();
        if !spans.contains(&current) {
            spans.push(current);
            spans.sort_unstable();
        }

        column![
            text("All plots").size(16),
            labelled("History", "How far back every plot goes. One setting for all of them, as the saved layout keeps one."),
            pick_list(spans, Some(current), Message::HistoryChanged)
                .style(field_pick_style)
                .text_size(14)
                .width(Fill),
        ]
        .spacing(8)
        .into()
    }

    /// The settings for one plot, shown in the configuration panel.
    fn plot_settings<'a>(&'a self, index: usize, plot: &'a AppPlot) -> Element<'a, Message> {
        let channels = match plot.channels.is_empty() {
            true => column![text("No channels yet.").size(14)],
            false => column(plot.channels.iter().enumerate().map(|(position, plotted)| {
                row![
                    text(plotted.reference.to_string()).size(14),
                    space::horizontal(),
                    button(circle_minus().size(14))
                        .style(button::text)
                        .padding(2)
                        .on_press(Message::ChannelRemovedFromPlot(index, position)),
                ]
                .align_y(Center)
                .into()
            }))
            .spacing(4),
        };

        // Only what is not already on this plot, so the list is what can
        // actually be added rather than everything with some of it inert.
        let addable: Vec<ChannelRef> = self
            .plottable
            .iter()
            .filter(|reference| {
                !plot.channels.iter().any(|plotted| plotted.reference == **reference)
            })
            .cloned()
            .collect();

        let add: Element<'_, Message> = match addable.is_empty() {
            true => text("Every channel is on this plot.").size(13).into(),
            false => pick_list(addable, None::<ChannelRef>, move |reference| {
                Message::ChannelAddedToPlot(index, reference)
            })
            .placeholder("Add a channel")
            .style(field_pick_style)
            .style(field_pick_style)
            .text_size(14)
            .width(Fill)
            .into(),
        };

        column![
            field_label("Name"),
            text_input("Plot name", &plot.name)
                .style(field_style)
                .on_input(move |name| Message::PlotRenamed(index, name)),
            field_label("Channels"),
            // The list and the thing that adds to it share one tinted panel,
            // so they read as one control rather than as a list with a
            // stray dropdown under it. The contents are inset so the tint
            // shows around them and the grouping is obvious.
            container(column![channels, add].spacing(8).padding(6))
                .width(Fill)
                .style(|theme: &Theme| container::Style {
                    background: Some(field_colour(theme).into()),
                    border: Border {
                        radius: FIELD_RADIUS.into(),
                        width: 1.0,
                        color: theme.extended_palette().background.weak.color,
                    },
                    ..container::Style::default()
                }),
        ]
        .spacing(8)
        .into()
    }

    /// One plot as a pane: its name and legend on the title bar, its traces
    /// below, sharing a background so each reads as one block.
    /// The `'a` is spelled out because the returned widgets borrow from *both*
    /// arguments — the channel names from `plot`, the traces from `self` —
    /// while elision would tie the result to `&self` alone.
    /// What a trace is called in the legend: its channel, and its unit.
    ///
    /// The unit comes from the setup rather than from the trace, because a
    /// trace knows only what it is drawn from. A measured or calculated
    /// channel is found by name in the device tree; a recorded one by the row
    /// it was read from, since names repeat between runs.
    ///
    /// "-" counts as no unit as much as an empty string does: it is what older
    /// configurations wrote where this one leaves the field out, and printing
    /// "Sine wave (-)" tells nobody anything.
    fn legend_label(&self, plotted: &PlottedChannel) -> String {
        let unit = match plotted.source {
            Some(source) => self
                .runs
                .iter()
                .flat_map(|run| run.devices.iter())
                .flat_map(|device| device.channels.iter())
                .find(|channel| channel.id == source)
                .map(|channel| channel.unit.clone()),
            None => self
                .devices
                .iter()
                .filter(|device| device.name == plotted.reference.device)
                .flat_map(|device| device.channels.iter())
                .find(|channel| channel.name == plotted.reference.channel)
                .map(|channel| channel.unit.clone()),
        };

        match unit {
            Some(unit) if !unit.is_empty() && unit != "-" => {
                format!("{} ({})", plotted.reference.channel, unit)
            }
            _ => plotted.reference.channel.clone(),
        }
    }

    /// What having this plot selected looks like, in the mode it belongs to.
    fn plot_selection(&self, index: usize) -> Selection {
        match self.mode {
            Mode::Record => Selection::Plot(index),
            Mode::View => Selection::ViewPlot(index),
        }
    }

    fn plot_card<'a>(
        &'a self,
        index: usize,
        plot: &'a AppPlot,
    ) -> pane_grid::Content<'a, Message> {
        let name = text(&plot.name);

        // Our own legend, since iced_plot's sits over the y axis and offers no
        // way to move it. A short rule in the trace colour, then the channel
        // it belongs to.
        let legend = row(plot.channels.iter().map(|plotted| {
            let colour = plotted.colour;

            row![
                container(space::horizontal().width(14).height(3)).style(
                    move |_theme: &Theme| container::Style {
                        background: Some(colour.into()),
                        ..container::Style::default()
                    }
                ),
                text(self.legend_label(plotted)).size(13),
            ]
            .spacing(6)
            .align_y(Center)
            .into()
        }))
        .spacing(16)
        .align_y(Center);

        let traces: Element<'_, Message> = match &plot.widget {
            Some(widget) => widget.view().map(move |message| Message::Plot(index, message)),
            None => container(text("No channels on this plot").size(14))
                .center_x(Fill)
                .center_y(Fill)
                .into(),
        };

        let is_selected = self.selected == Some(self.plot_selection(index));
        // Where a drop would land, which is worth saying while one is in the
        // air and means nothing otherwise.
        let is_target = self.dragging.is_some() && self.over_plot == Some(index);

        // The name and the legend go in the title bar's two slots rather than
        // in one row that spans it. iced works out where a pane may be picked
        // up as "the title bar, less whatever is in it" — see
        // `TitleBar::is_over_pick_area` — so a full width row leaves only the
        // padding to drag by. Two things at the ends leave the space between
        // them belonging to neither, and that space is the handle.
        //
        // Right click opens the plot's menu. Only the right button, so the
        // left one is still free to start a drag.
        let title_bar = pane_grid::TitleBar::new(
            MouseArea::new(row![name].align_y(Center))
                .on_right_press(Message::ContextOpened(ContextMenu::Plot(index))),
        )
        .controls(pane_grid::Controls::new(
            MouseArea::new(row![legend].align_y(Center))
                .on_right_press(Message::ContextOpened(ContextMenu::Plot(index))),
        ))
        // Without this the legend appears only while the pointer is over the
        // pane: `TitleBar` treats its controls as pane furniture, shown on
        // hover like a close button. A legend is not furniture - it is what
        // says which trace is which.
        .always_show_controls()
        .padding(padding::all(10).bottom(4));
        // No background of its own. Painting one draws a strip over the top of
        // the card's rounded border, which is what made the header read as a
        // bar sitting above the plot and made a selected pane look as though
        // its outline stopped below the title.

        // Only while something is being dragged. A `MouseArea` reporting every
        // crossing the rest of the time would be a rebuild per plot per
        // pointer sweep, for an answer nothing is asking for.
        // The rule belongs to the body rather than the title bar, because the
        // title bar is drawn by iced and there is nowhere in it to put one.
        // Same rule as every other heading in the interface has under it.
        let under = column![pane_rule(), container(traces).padding(padding::all(10).top(6))];

        let body: Element<'_, Message> = match self.dragging.is_some() {
            false => under.into(),
            true => MouseArea::new(under)
                .on_enter(Message::PlotHovered(index))
                .on_exit(Message::PlotLeft(index))
                .into(),
        };

        pane_grid::Content::new(body)
            .title_bar(title_bar)
            .style(move |theme: &Theme| {
                let palette = theme.extended_palette();

                container::Style {
                    background: Some(card_colour(theme).into()),
                    border: Border {
                        radius: 8.0.into(),
                        // The selected plot is the one the settings panel is
                        // talking about, so it has to be obvious which that is.
                        // A plot about to be dropped on says so the same way,
                        // in the colour that means "this one".
                        width: if is_selected || is_target { 2.0 } else { 0.0 },
                        color: match is_target {
                            true => palette.success.base.color,
                            false => palette.primary.base.color,
                        },
                    },
                    ..container::Style::default()
                }
            })
    }

    /// The rig as a tree: every device, and the channels under the open ones.
    fn devices_pane(&self) -> Element<'_, Message> {
        let add_device_button = hint(
            button(circle_plus())
                .style(button::text)
                .padding(4)
                .on_press(Message::AddDeviceOpened),
            "Add device",
        );

        let device_list =
            column(self.devices.iter().enumerate().map(|(index, device)| {
                // Expanding and inspecting are different intentions,
                // so they get different targets: the chevron opens
                // the device, the name selects it.
                let chevron: Element<'_, Message> = match device.channels.is_empty() {
                    true => space::horizontal().width(22).into(),
                    false => button(match device.expanded {
                        true => chevron_down().size(14),
                        false => chevron_right().size(14),
                    })
                    .style(button::text)
                    .padding(4)
                    .on_press(Message::ToggleDevice(index))
                    .into(),
                };

                let (selected, press, _add) = match device.kind {
                    DeviceKind::Measured(at) => (
                        self.selected == Some(Selection::Device(at)),
                        Message::DeviceSelected(at),
                        Some(Message::ChannelAdded(at)),
                    ),
                    DeviceKind::Calculated => (
                        self.selected == Some(Selection::Calculated),
                        Message::CalculatedSelected,
                        Some(Message::CalculatedChannelAdded),
                    ),
                };

                let device_header = row![
                    chevron,
                    button(text(&device.name))
                        .style(match selected {
                            true => button::primary,
                            false => button::text,
                        })
                        .padding(4)
                        .on_press(press),
                    // Held off the name rather than butted against
                    // it: it is a fact about the device, not part
                    // of what it is called.
                    container(connection_dot(device.health()))
                        .padding(padding::left(6)),
                    // space::horizontal(),
                    // Adding a channel is a change to the rig like
                    // any other, so it waits for the run to stop.
                    // hint(
                    //     button(add_channel_mark(self.rig_editable()))
                    //         .style(button::text)
                    //         .padding(4)
                    //         .on_press_maybe(match self.rig_editable() {
                    //             true => add,
                    //             false => None,
                    //         }),
                    //     "Add channel",
                    // ),
                ]
                .align_y(Center);

                let mut entry = column![match device.kind {
                    DeviceKind::Measured(at) => Element::from(
                        MouseArea::new(device_header)
                            .on_right_press(Message::ContextOpened(
                                ContextMenu::Device(at)
                            ))
                    ),
                    DeviceKind::Calculated => Element::from(
                        MouseArea::new(device_header).on_right_press(
                            Message::ContextOpened(ContextMenu::Calculated),
                        ),
                    ),
                }];

                if device.expanded {
                    entry = entry.push(
                        column(device.channels.iter().enumerate().map(
                            |(position, channel)| {
                                let reading = match channel.latest {
                                    Some(value) => {
                                        format!("{:.3} {}", value, channel.unit)
                                    }
                                    None => "—".to_string(),
                                };

                                // The same two questions as the
                                // device row, asked of the channel.
                                let (selected, press) = match device.kind {
                                    DeviceKind::Measured(at) => (
                                        self.selected
                                            == Some(Selection::Channel(at, position)),
                                        Message::ChannelSelected(at, position),
                                    ),
                                    DeviceKind::Calculated => (
                                        self.selected
                                            == Some(Selection::CalculatedChannel(
                                                position,
                                            )),
                                        Message::CalculatedChannelSelected(position),
                                    ),
                                };

                                let row: Element<'_, Message> =
                                    row![
                                        // The icon does the
                                        // indenting, so a channel
                                        // reads as belonging to
                                        // the device above it
                                        // without a margin as well.
                                        // Which icon says where the
                                        // value came from: read in,
                                        // or worked out.
                                        match device.kind {
                                            DeviceKind::Measured(_) =>
                                                square_arrow_right().size(11),
                                            DeviceKind::Calculated =>
                                                square_equal().size(11),
                                        },
                                        text(&channel.name).size(14),
                                        space::horizontal(),
                                        // Dressed as a field, since
                                        // that is what it is: a
                                        // value belonging to this
                                        // channel rather than more
                                        // of its name.
                                        container(text(reading).size(13))
                                            .padding([2, 6])
                                            .style(|theme: &Theme| container::Style {
                                                background: Some(
                                                    field_colour(theme).into(),
                                                ),
                                                border: Border {
                                                    radius: FIELD_RADIUS.into(),
                                                    width: 1.0,
                                                    color: theme
                                                        .extended_palette()
                                                        .background
                                                        .weak
                                                        .color,
                                                },
                                                ..container::Style::default()
                                            }),
                                    ]
                                    // Keeps the name off the
                                    // reading when the pane is
                                    // dragged narrow: the flexible
                                    // space between them goes to
                                    // nothing, this does not.
                                    .spacing(8)
                                    .align_y(Center)
                                .into();

                                let on_right = match device.kind {
                                    DeviceKind::Measured(at) => {
                                        ContextMenu::Channel(at, position)
                                    }
                                    DeviceKind::Calculated => {
                                        ContextMenu::CalculatedChannel(position)
                                    }
                                };

                                channel_row(
                                    row,
                                    match selected {
                                        true => RowLook::Picked,
                                        false => RowLook::Plain,
                                    },
                                    press,
                                    Message::ContextOpened(on_right),
                                )
                            },
                        ))
                        .spacing(4)
                        // Lined up with the device name above, not
                        // merely inboard of it: the chevron is 22
                        // wide and the name button pads by 4, so
                        // its text starts at 26 - and the row here
                        // pads by 4 of its own, leaving 22.
                        .padding(padding::left(22)),
                    );
                }

                entry.into()
            }))
            .spacing(5);

        // Asking is a press rather than something that happens on a timer:
        // opening a serial port asserts DTR and resets most boards, so a rig
        // is checked when somebody wants to know, not every half minute.
        let check_button = hint(
            button(refresh_cw().size(15))
                .style(button::text)
                .padding(4)
                .on_press_maybe(match self.run == RunState::Stopped {
                    true => Some(Message::ConnectionsChecked),
                    // A run owns the devices; there is nothing to ask.
                    false => None,
                }),
            "Check devices",
        );

        self.pane(
            row![text("Devices"), space::horizontal(), check_button, add_device_button]
                .spacing(4)
                .align_y(Center),
            device_list.into(),
        )
    }

    /// Say which devices are pointed at hardware another one has claimed.
    ///
    /// Said at the moments a clash can arrive without somebody watching for
    /// it - opening a project, loading devices, choosing a port - rather than
    /// on every edit. The border on the field is what says it is still true;
    /// this is what says what it is.
    fn note_address_clashes(&mut self) {
        for clash in self.config.address_clashes() {
            self.note(format!(
                "{} is on {}, which {} is already using",
                clash.device, clash.address, clash.taken_by
            ));
        }
    }

    /// Rebuild the tree from the configuration, keeping what is known.
    ///
    /// Devices arriving several at a time is what makes this a rebuild rather
    /// than an append. A rebuild loses the part that is not in the config -
    /// whether a device answered when something last looked - so that is
    /// carried across by name. Not the concern beside it: `rig_changed` clears
    /// those on any edit, a verdict being about settings that may have moved.
    ///
    /// Nothing is re-checked afterwards. A device that has just arrived has
    /// never been looked at and shows no dot, which is honest, and opening the
    /// ones already here to tell somebody what they already knew would reset
    /// every board that takes DTR as a reset.
    fn reload_devices(&mut self) {
        let answered: Vec<(String, Option<bool>)> = self
            .devices
            .iter()
            .map(|device| (device.name.clone(), device.connected))
            .collect();

        self.devices = devices_from(&self.config);

        for device in self.devices.iter_mut() {
            if let Some((_, connected)) = answered.iter().find(|(name, _)| *name == device.name) {
                device.connected = *connected;
            }
        }
    }

    /// Adding a channel to whatever is selected, where that means anything.
    ///
    /// Only a device takes a channel. A plot, a channel, and anything read
    /// back out of a recording do not, so they are offered nothing rather
    /// than a control that would have to explain itself.
    fn add_to_selected(&self) -> Option<Message> {
        let editable = self.rig_editable();

        match self.selected? {
            Selection::Device(index) => editable.then_some(Message::ChannelAdded(index)),
            Selection::Calculated => editable.then_some(Message::CalculatedChannelAdded),
            _ => None,
        }
    }

    /// Whatever is selected, and what can be changed about it.
    fn config_pane(&self) -> Element<'_, Message> {
        // Absent rather than greyed. A greyed control is worth
        // having where its absence would be a surprise - the
        // transport buttons keep their places - but nothing is
        // owed an explanation for why a recorded run has no delete.
        // Left of the delete, in the order the two sit in a device's row.
        let add = self.add_to_selected().map(|message| {
            Element::from(
                hint(
                    button(add_channel_mark(true))
                        .style(button::text)
                        .padding(4)
                        .on_press(message),
                    "Add channel",
                ),
            )
        });

        let remove = self.delete_selected().map(|message| {
            Element::from(
                button(trash_two().size(16))
                    .style(danger_on_hover)
                    .padding(4)
                    .on_press(message),
            )
        });

        // Not named `settings`: that is the gear icon's function.
        let panel: Element<'_, Message> = match self.selected {
            Some(Selection::Plot(index)) => match self.plots.get(index) {
                Some(plot) => self.plot_settings(index, plot),
                // The selected plot was deleted from under us.
                None => text("Nothing selected.").size(14).into(),
            },
            Some(Selection::AllPlots) => self.all_plots_settings(),
            Some(Selection::ViewPlot(index)) => self.view_plot_settings(index),
            Some(Selection::Run(run)) => self.run_settings(run),
            Some(Selection::RunDevice(run, device)) => {
                self.run_device_settings(run, device)
            }
            Some(Selection::RunChannel(run, device, channel)) => {
                self.run_channel_settings(run, device, channel)
            }
            Some(Selection::Device(index)) => self.device_settings(index),
            Some(Selection::Channel(device, channel)) => {
                self.channel_settings(device, channel)
            }
            Some(Selection::Calculated) => self.calculated_settings(),
            Some(Selection::CalculatedChannel(channel)) => {
                self.calculated_channel_settings(channel)
            }
            None => {
                text("Select a device, channel or plot to configure it.").size(14).into()
            }
        };

        self.pane(
            row![text("Configuration"), space::horizontal()]
                .extend(add)
                .extend(remove)
                .spacing(4)
                .align_y(Center),
            panel,
        )
    }

    /// What the run has had to say.
    ///
    /// Builds its own `Content` rather than returning an element, because it
    /// is the one pane that draws no heading of its own when it is shut.
    fn log_pane(&self) -> pane_grid::Content<'_, Message> {
        let samples: usize = self
            .devices
            .iter()
            .flat_map(|device| device.channels.iter())
            .map(|channel| channel.samples)
            .sum();

        let chevron = button(match self.log_open {
            true => chevron_down().size(14),
            false => chevron_right().size(14),
        })
        .style(button::text)
        .padding(2)
        .on_press(Message::LogToggled);

        // Shut, the pane is its heading and nothing else — no rule
        // either, since there is nothing under it to divide from.
        // The newest line rides along on the bar, so a run can be
        // watched without opening it.
        if !self.log_open {
            let latest = match self.log.last() {
                Some(entry) => entry.text.clone(),
                None => String::new(),
            };

            return pane_grid::Content::new(self.pane_heading(
                row![chevron, text("Log"), text(latest).size(13), space::horizontal()]
                    // So the newest line reads as a separate thing
                    // from the title rather than running on from it.
                    .spacing(12)
                    // Bottom rather than centre: the line is
                    // smaller than the title, and centring it
                    // leaves it floating above the title's
                    // baseline instead of sitting on it.
                    .align_y(Bottom),
            ))
            .style(container::rounded_box);
        }

        let lines = column(self.log.iter().map(|entry| {
            // Local time and the date with it. The stamp is kept
            // in UTC, which is right for a record and wrong for
            // reading: a run either side of midnight otherwise
            // shows two times that sort the wrong way by eye.
            text(format!(
                "{} - {}",
                entry.at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"),
                entry.text
            ))
            .size(13)
            .into()
        }))
        .spacing(2);

        pane_grid::Content::new(
            column![
                self.pane_heading(
                    row![
                        chevron,
                        text("Log"),
                        space::horizontal(),
                        text(format!("{} samples", samples)).size(14),
                    ]
                    .align_y(Center),
                ),
                pane_rule(),
                // Anchored to the bottom so the newest line is the
                // one in view, the way a terminal behaves. Cheaper
                // and steadier than scrolling there on every line.
                scrollable(lines.width(Fill).padding(10))
                    .anchor_bottom()
                    .height(Fill),
            ]
            .width(Fill),
        )
        .style(container::rounded_box)
    }

    /// The plots, in a grid of their own.
    fn data_pane(&self) -> Element<'_, Message> {
        let add_plot_button = hint(
            button(circle_plus())
                .style(button::text)
                .padding(4)
                .on_press(Message::AddPlot),
            "Add a plot",
        );

        // Whichever set of plots this mode is about. The panes and
        // the plots are swapped together, so a layout arranged for
        // watching a rig is still there when recording comes back.
        let (panes, plots) = match self.mode {
            Mode::Record => (&self.plot_panes, &self.plots),
            Mode::View => (&self.view_panes, &self.view_plots),
        };

        // A grid of its own inside this pane. The plots arrange
        // against each other; the sidebar is not somewhere a plot
        // can be dragged.
        let cards = PaneGrid::new(panes, |_pane, number, _maximised| {
            let found =
                plots.iter().enumerate().find(|(_, plot)| plot.number == *number);

            match found {
                Some((index, plot)) => self.plot_card(index, plot),
                // A pane whose plot has gone. Closed on the next
                // deletion rather than drawn as an error.
                None => pane_grid::Content::new(space::horizontal().width(0)),
            }
        })
        .width(Fill)
        .height(Fill)
        .spacing(8)
        .on_click(Message::PlotClicked)
        .on_drag(Message::PlotDragged)
        .on_resize(10, Message::PlotsResized);

        self.pane_filling_pressable(
            row![
                // The heading is the way to the settings for every
                // plot at once, the same as selecting a device is
                // the way to its own. A gear beside it would be a
                // second way to reach one panel.
                text("Data"),
                space::horizontal(),
                add_plot_button
            ]
            .align_y(Center),
            Message::AllPlotsSelected,
            cards.into(),
        )
    }

    /// The strip along the top: what the application is, what the rig is
    /// doing, and which half of the application you are looking at.
    fn title_bar(&self) -> Element<'_, Message> {
        // The app name is the way into the file menu, with the chevron saying
        // so. Styled as text rather than as a button, because a menu bar is
        // read as a name until it is used.
        let title = button(
            row![
                mark(LOGO, MODE_ICON),
                // Set in the brand face, and a little smaller than the sans
                // it replaces: a monospaced face runs noticeably wider at the
                // same size, and this is a wordmark rather than a heading.
                text("LUMBERJACK").size(24).font(BRAND),
                // Always down: it says "there is a menu here", which stays
                // true while the menu is open.
                chevron_down().size(14),
            ]
            .spacing(14)
            .align_y(Center),
        )
        .style(button::text)
        .padding(4)
        .on_press(Message::FileMenuToggled);

        // Disabled while stopping rather than hidden, so the control does not
        // move about and it is clear why pressing it does nothing.
        // Three buttons that are always the same three buttons. What changes is
        // their colour, which says what the rig is doing rather than what the
        // press would do — so the header is a state to read at a glance, not a
        // control that rearranges itself under the cursor.
        let run_control = row![
            transport(
                // Lit while starting as well as while running: the press
                // did something, and a control that stays dark for a second
                // after it is pressed reads as one that did not take.
                transport_mark(PLAY, self.run != RunState::Stopped, |palette| {
                    palette.success.base.color
                }),
                "Start acquisition",
                Message::RunStarted,
                [4, 4],
            ),
            transport(
                // Never lit. Play says it is reading and record says it is
                // recording; stopped is the absence of both, not a third lamp.
                transport_mark(STOP, false, |palette| palette.background.base.text),
                "Stop acquisition and recording",
                Message::RunStopped,
                [4, 4],
            ),
            transport(
                transport_mark(RECORD, self.recording(), |palette| palette.danger.base.color),
                "Record to disk",
                Message::RecordPressed,
                [4, 4],
            ),
        ]
        // Close together: the three of them are one control with three
        // buttons, not three controls that happen to be adjacent.
        .spacing(1);

        // Recording is a thing you do to a run, so it is only offered while
        // one is going. `on_press_maybe(None)` leaves the button there but
        // dead, rather than having the header rearrange itself.
        // Application example
        // Which job the window is doing. Lit like the transport buttons and
        // for the same reason: the pair is a state to read at a glance, not a
        // control that changes shape depending on where you are.
        let modes = row![
            transport(
                tinted_mark(RECORD_MODE, MODE_ICON, {
                    let here = self.mode == Mode::Record;
                    move |theme: &Theme| match here {
                        true => BRAND_RED,
                        false => theme.extended_palette().background.weak.text,
                    }
                }),
                "Set up and record",
                Message::ModeChosen(Mode::Record),
                [4, 4],
            ),
            transport(
                tinted_mark(DATA_MODE, MODE_ICON, {
                    let here = self.mode == Mode::View;
                    // The brand's own red rather than a palette colour: which
                    // half of the application you are in is a fact about the
                    // product, not about the theme.
                    move |theme: &Theme| match here {
                        true => BRAND_RED,
                        false => theme.extended_palette().background.weak.text,
                    }
                }),
                "Look at what was recorded",
                Message::ModeChosen(Mode::View),
                [4, 4],
            ),
        ]
        .spacing(0);

        container(
            row![
                title,
                space::horizontal(),
                run_control,
                // Held off the transport group rather than spaced evenly with
                // it: these five are two controls, not five, and the gap is
                // what says so. Held off the window edge as well, so the row
                // ends where the panes below it do rather than running out to
                // the frame.
                container(modes).padding(padding::left(14).right(2)),
            ]
            .spacing(14)
            .padding(6)
            .align_y(Center),
        )
        .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let header = self.title_bar();


        // Each pane's content is built fresh here rather than once above,
        // since this closure runs once per pane and a Button/Element isn't
        // Copy — there's nothing to share between the three arms anyway.
        let pane_grid = PaneGrid::new(&self.panes, |_id, kind, _is_maximized| {
            let content: Element<'_, Message> = match kind {
                PaneKind::Devices if self.mode == Mode::View => self.run_tree(),
                PaneKind::Devices => self.devices_pane(),
                PaneKind::Config => self.config_pane(),
                PaneKind::Log => return self.log_pane(),
                PaneKind::Data => self.data_pane(),
            };

            pane_grid::Content::new(content).style(container::rounded_box)
        })
        .width(Fill)
        .height(Fill)
        .spacing(4)
        .on_resize(10, Message::PaneResized);

        let screen = column![header, container(pane_grid).padding(4).height(Fill)];

        // Floated over the interface at the pointer rather than pushed into the
        // list, so opening one does not move the thing it was opened on.
        let screen: Element<'_, Message> = match self.context {
            None => screen.into(),
            // The menu sits inside a layer covering the whole window, so a
            // click anywhere but the menu lands on that layer and closes it.
            // The same shape as the dialog, and for the same reason: what
            // catches the click has to be above everything that would
            // otherwise take it first.
            Some(target) => stack![
                screen,
                opaque(
                    MouseArea::new(
                        container(opaque(
                            row![self.context_entries(target)]
                                // Beside the menu, not over it: the channel
                                // being added stays named while its
                                // destination is chosen.
                                .extend(
                                    (self.plot_menu && target.plottable())
                                        .then(|| self.plot_menu_entries())
                                )
                                .spacing(2),
                        ))
                        .width(Fill)
                        .height(Fill)
                        .padding(padding::left(self.context_at.x).top(self.context_at.y))
                    )
                    .on_press(Message::ContextDismissed)
                ),
            ]
            .into(),
        };

        // Anchored under the name it drops from rather than at the pointer:
        // this one belongs to a place on screen, not to whatever was clicked.
        let screen: Element<'_, Message> = match self.file_menu {
            false => screen,
            true => stack![
                screen,
                opaque(
                    MouseArea::new(
                        container(opaque(look::menu(
                            vec![
                                MenuItem::Entry("New project", Some(Message::ProjectCreated)),
                                MenuItem::Entry("Load project", Some(Message::ProjectOpened)),
                                // Only with somewhere to put them. Nothing
                                // else in this menu needs a project either,
                                // and Export is greyed the same way.
                                MenuItem::Entry(
                                    "Load devices",
                                    self.project.as_ref().map(|_| Message::DevicesImported),
                                ),
                                MenuItem::Entry(
                                    "Export",
                                    self.project.as_ref().map(|_| Message::Exported),
                                ),
                                MenuItem::Divider,
                                MenuItem::Entry("Settings", Some(Message::SettingsOpened)),
                            ],
                            170.0,
                        )))
                        .width(Fill)
                        .height(Fill)
                        .padding(padding::left(8).top(44))
                    )
                    .on_press(Message::ContextDismissed)
                ),
            ]
            .into(),
        };

        // What is being dragged, under the cursor that is dragging it. Read
        // live from `POINTER` rather than held in state, because following the
        // pointer is the whole point of it - unlike the context menu, which is
        // frozen where it was opened so it does not wander off.
        //
        // Not `opaque`: this sits over everything including the plot it is
        // heading for, and a layer that swallowed events would stop that plot
        // ever noticing the pointer arrive.
        let screen: Element<'_, Message> = match self.dragged_label() {
            None => screen,
            Some(label) => {
                let at = pointer();
                stack![
                    screen,
                    container(
                        container(text(label).size(13))
                            .padding([4, 8])
                            .style(|theme: &Theme| {
                                let palette = theme.extended_palette();

                                container::Style {
                                    background: Some(palette.primary.base.color.into()),
                                    text_color: Some(palette.primary.base.text),
                                    border: Border {
                                        radius: FIELD_RADIUS.into(),
                                        ..Border::default()
                                    },
                                    ..container::Style::default()
                                }
                            })
                    )
                    .width(Fill)
                    .height(Fill)
                    // Down and to the right of the cursor, so it does not sit
                    // on the thing being pointed at.
                    .padding(padding::left(at.x + 12.0).top(at.y + 12.0)),
                ]
                .into()
            }
        };

        // With nothing open there is nothing to look at, so the interface asks
        // for a project rather than showing empty panels and leaving somebody
        // to find the menu. Not dismissable, because there is no state behind
        // it to work in.
        let screen: Element<'_, Message> = match self.project.is_some() {
            true => screen,
            false => stack![
                screen,
                opaque(
                    // Centred both ways: there is nothing behind it to keep in
                    // view, and nothing in it that opens downwards, so the
                    // reasons the other dialogs sit near the top do not apply.
                    container(opaque(self.welcome()))
                        .width(Fill)
                        .height(Fill)
                        .center_x(Fill)
                        .center_y(Fill)
                ),
            ]
            .into(),
        };

        // Above the settings dialog in the stack, because it is opened from a
        // field behind that one is never over.
        let screen: Element<'_, Message> = match self.formula_help {
            None => screen,
            Some(which) => stack![
                screen,
                opaque(
                    MouseArea::new(
                        // Centred both ways, unlike the dialogs that hold a
                        // dropdown: those sit high because a list opens into
                        // whatever room is below it. There is nothing here to
                        // open, and a page of reading belongs in the middle.
                        //
                        // The padding is what stops it touching the edges of a
                        // short window, since the height below is a maximum
                        // rather than a fixed size.
                        container(opaque(self.formula_help_dialog(which)))
                            .width(Fill)
                            .height(Fill)
                            .center_x(Fill)
                            .center_y(Fill)
                            .padding(40)
                    )
                    .on_press(Message::FormulaHelpClosed)
                ),
            ]
            .into(),
        };

        let screen: Element<'_, Message> = match self.settings_open {
            false => screen,
            // Dismissed by clicking away as well as by the x, like the others.
            // Nothing in here is half-finished, so leaving is never a loss.
            true => stack![
                screen,
                opaque(
                    MouseArea::new(
                        container(opaque(self.settings_dialog()))
                            .width(Fill)
                            .height(Fill)
                            .center_x(Fill)
                            .align_y(Top)
                            .padding(60)
                    )
                    .on_press(Message::SettingsClosed)
                ),
            ]
            .into(),
        };

        match self.adding_device {
            None => screen,
            // Laid over the window rather than shown beside it, so nothing
            // behind can be clicked while a device is half-made. Clicking away
            // cancels, the same as the x.
            //
            // Held near the top rather than centred, which is what decides
            // which way its dropdown opens: iced compares the room below a
            // pick_list against the room above, and the room below has the
            // control's own height taken off it. A centred control therefore
            // always has less room below and always opens upwards.
            Some(chosen) => stack![
                screen,
                opaque(
                    MouseArea::new(
                        container(opaque(self.add_device_dialog(chosen)))
                            .width(Fill)
                            .height(Fill)
                            .center_x(Fill)
                            .align_y(Top)
                            .padding(60)
                    )
                    .on_press(Message::AddDeviceCancelled)
                )
            ]
            .into(),
        }
        }

        fn theme(&self) -> Option<Theme> {
        Some(self.settings.theme())
    }

    /// How much bigger or smaller than usual to draw everything.
    fn scale_factor(&self) -> f32 {
        self.settings.scale_factor()
    }

    /// Keep what was just chosen, and say so if it could not be kept.
    ///
    /// Written straight away rather than on exit: settings changed and then
    /// lost to a crash are worse than a handful of small writes.
    fn remember_settings(&mut self) {
        if let Err(problem) = self.settings.save() {
            self.note(format!("could not save the settings: {}", problem));
        }
    }
}

impl Drop for AppDaq {
    /// Ask the run to end. The process is going away anyway, but a recording
    /// run would want its sink flushed rather than cut off mid-batch.
    fn drop(&mut self) {
        if let Some(acquisition) = self.acquisition.as_ref() {
            acquisition.stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumberdaq::calculated::{CalculatedChannel, CalculatedDevice};
    use lumberdaq::channel::ChannelInfo;
    use lumberdaq::device::DeviceInfo;
    use std::collections::BTreeMap;

    /// A setup whose calculated channels carry the given equations, each
    /// declaring one input called `v`.
    fn with_equations(equations: &[&str]) -> DaqConfig {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "v".to_string(),
            ChannelRef {
                device: "Signal generator".to_string(),
                channel: "sine".to_string(),
            },
        );

        DaqConfig {
            info: DaqInfo { name: "a project".to_string(), author: String::new() },
            devices: vec![],
            calculated: Some(CalculatedDevice {
                info: DeviceInfo { name: "Calculated".to_string() },
                channels: equations
                    .iter()
                    .enumerate()
                    .map(|(index, equation)| CalculatedChannel {
                        info: ChannelInfo {
                            name: format!("channel {}", index),
                            ..Default::default()
                        },
                        inputs: inputs.clone(),
                        equation: equation.to_string(),
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn equations_that_build_leave_the_calculated_device_with_no_dot() {
        // It is worked out rather than read, so there is nothing for it to be
        // connected to and a healthy one says nothing at all.
        assert_eq!(calculated_health(&with_equations(&["v * 2"])), None);
    }

    #[test]
    fn an_equation_that_will_not_build_shows_the_device_as_down() {
        // `w` is not among the declared inputs. The failure this is here to
        // catch: it loads, the run starts, and the channel silently produces
        // nothing, which looks exactly like hardware that is not sending.
        assert_eq!(calculated_health(&with_equations(&["w * 2"])), Some(false));
    }

    #[test]
    fn an_equation_nobody_has_written_yet_is_not_an_error() {
        // A channel is added with an empty equation on purpose, so going red
        // before somebody has typed a character would be crying wolf.
        assert_eq!(calculated_health(&with_equations(&["", "v * 2"])), None);
        // One that is written and wrong still counts, whatever it sits beside.
        assert_eq!(calculated_health(&with_equations(&["", "w * 2"])), Some(false));
    }

    /// The help is markdown now, which means it can be malformed in ways a
    /// column of text widgets could not: an unclosed fence turns the whole
    /// page into one code block, and nothing about that fails to compile.
    #[test]
    fn both_help_documents_parse_into_a_page_rather_than_one_lump() {
        for which in [FormulaHelp::Scale, FormulaHelp::Equation] {
            let items: Vec<markdown::Item> = markdown::parse(which.text()).collect();

            let headings = items
                .iter()
                .filter(|item| matches!(item, markdown::Item::Heading(..)))
                .count();
            let code = items
                .iter()
                .filter(|item| matches!(item, markdown::Item::CodeBlock { .. }))
                .count();

            assert!(headings >= 6, "{:?} has {} headings", which, headings);
            // The examples. The function list is inline spans rather than a
            // block, because a block cannot wrap and the dialog is narrow.
            assert!(code >= 1, "{:?} has {} code blocks", which, code);
            assert!(
                items.len() > headings + code,
                "{:?} is all headings and code, with no prose between them",
                which
            );
        }
    }

    #[test]
    fn a_setup_with_no_calculated_device_has_nothing_to_judge() {
        let mut config = with_equations(&[]);
        config.calculated = None;
        assert_eq!(calculated_health(&config), None);
    }
}
