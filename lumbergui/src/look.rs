//! How the interface is drawn: its colours, its shapes, and the small pieces
//! of furniture built from them.
//!
//! Everything here is a free function or a plain value. None of it reaches
//! into the application's state, which is what lets it sit apart from the
//! rest: a style is a fact about the theme, and a widget helper is a fact
//! about what it is handed.
//!
//! Several are free functions where a method would read better, and
//! deliberately: `Tooltip` is invariant over its lifetime, so building one
//! inside a method ties the result to the borrow of `self` rather than to the
//! interface it belongs in.

use crate::Message;
use iced::widget::{
    button, column, container, pick_list, space, svg, text, text_input, tooltip, MouseArea,
};
use iced::{padding, Border, Color, Element, Fill, Theme};

/// How much of the recent past a plot shows comes from the saved layout's
/// `history_seconds`.
///
/// However long it is, the viewport is slid by wall-clock time on every frame
/// while points are only ever added when real samples arrive. Keeping those
/// two apart is what makes the scroll continuous without drawing any value
/// that wasn't measured: between batches the line simply stops short of the
/// right edge.
///
/// Enough colours to tell a handful of traces apart. Cycled if there are more channels
/// than colours, which is a legend problem to solve when it happens.
pub(crate) const PALETTE: [Color; 6] = [
    Color::from_rgb(0.30, 0.70, 1.00),
    Color::from_rgb(1.00, 0.50, 0.20),
    Color::from_rgb(0.45, 0.85, 0.45),
    Color::from_rgb(0.95, 0.45, 0.75),
    Color::from_rgb(0.85, 0.80, 0.35),
    Color::from_rgb(0.65, 0.55, 1.00),
];

/// Corner rounding shared by every field, so they look like one family.
pub(crate) const FIELD_RADIUS: f32 = 6.0;

/// The fill behind anything that can be typed into or chosen from.
///
/// One colour for text fields, dropdowns and the lists that hold them, so a
/// panel reads as a set of inputs rather than as several unrelated widgets.
pub(crate) fn field_colour(theme: &Theme) -> Color {
    theme.extended_palette().background.weakest.color
}

/// Text fields: the shared fill, and corners rounded to match everything else.
pub(crate) fn field_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let palette = theme.extended_palette();
    let mut style = text_input::default(theme, status);

    style.background = iced::Background::Color(field_colour(theme));
    style.border = Border {
        radius: FIELD_RADIUS.into(),
        width: 1.0,
        color: match status {
            text_input::Status::Focused { .. } => palette.primary.base.color,
            _ => palette.background.weak.color,
        },
    };
    style
}

/// The same field, when what is in it will not do.
///
/// Only the border changes. The message itself is a tooltip rather than a line
/// of red under the field: these messages are a sentence or two long, they
/// appear and vanish as somebody types, and a panel that grows and shrinks
/// under the cursor while they are still typing is harder to use than one that
/// stays put and says so quietly.
pub(crate) fn field_error_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let mut style = field_style(theme, status);

    style.border.color = theme.extended_palette().danger.base.color;
    style.border.width = 2.0;
    style
}

/// Dropdowns, styled to match the text fields beside them.
/// The same, for a field holding a value that has been tried and did not work.
///
/// Only the border changes. Recolouring the whole control would say the value
/// is unusable, and it may well be right - what is known is that it did not
/// read this device just now.
pub(crate) fn field_pick_warning_style(
    theme: &Theme,
    status: pick_list::Status,
) -> pick_list::Style {
    let mut style = field_pick_style(theme, status);
    style.border.color = theme.extended_palette().warning.base.color;
    style
}

pub(crate) fn field_pick_style(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let palette = theme.extended_palette();
    let mut style = pick_list::default(theme, status);

    style.background = iced::Background::Color(field_colour(theme));
    style.border = Border {
        radius: FIELD_RADIUS.into(),
        width: 1.0,
        color: match status {
            pick_list::Status::Hovered | pick_list::Status::Opened { .. } => {
                palette.primary.base.color
            }
            _ => palette.background.weak.color,
        },
    };
    style
}

/// The label above a field: smaller than what it labels, and quieter, so the
/// value is what the eye lands on.
pub(crate) fn field_label(label: &str) -> Element<'_, Message> {
    text(label)
        .size(12)
        // Softened from the theme's own text colour rather than a fixed grey,
        // which would be invisible on one theme and harsh on another.
        .style(|theme: &Theme| text::Style {
            color: Some(Color { a: 0.7, ..theme.extended_palette().background.base.text }),
        })
        .into()
}

/// One line of a menu: something to do, or a rule between groups of them.
pub(crate) enum MenuItem<'a> {
    /// A label and what it does. `None` is shown greyed rather than hidden, so
    /// the menu keeps its shape and its items do not move about — an entry
    /// being unavailable is worth saying, where a menu that silently loses one
    /// just looks different.
    Entry(&'a str, Option<Message>),
    /// The same, when the label is worked out rather than written here — a
    /// plot's name, for instance, which nothing static can know.
    Owned(String, Option<Message>),
    Divider,
}

/// The menu a right click opens: a short list of things to do to one item.
/// A rule rather than an entry, written where an entry would go.
///
/// The list is pairs of label and message, which has no room for "and a line
/// here". A label nothing would ever use stands in, and `context_menu` turns
/// it back into a divider. Cheaper than making every caller build `MenuItem`s
/// for the sake of the one place that wants a rule.
pub(crate) const DIVIDER: (&str, Option<Message>) = ("---", None);

pub(crate) fn context_menu<'a>(entries: Vec<(&'a str, Option<Message>)>) -> Element<'a, Message> {
    menu(
        entries
            .into_iter()
            .map(|(label, message)| match label == DIVIDER.0 {
                true => MenuItem::Divider,
                false => MenuItem::Entry(label, message),
            })
            .collect(),
        150.0,
    )
}

/// How a dialog is drawn: a panel lifted clear of the interface behind it.
///
/// Shared so the three of them cannot drift apart — they are the same kind of
/// thing and should not be three slightly different panels.
pub(crate) fn dialog_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();

    container::Style {
        background: Some(palette.background.base.color.into()),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: palette.background.strong.color,
        },
        ..container::Style::default()
    }
}

/// A floating list of things to do.
pub(crate) fn menu<'a>(items: Vec<MenuItem<'a>>, width: f32) -> Element<'a, Message> {
    container(
        column(items.into_iter().map(|item| match item {
            MenuItem::Entry(label, message) => button(text(label).size(13))
                .style(button::text)
                .padding([2, 6])
                .width(Fill)
                .on_press_maybe(message)
                .into(),
            MenuItem::Owned(label, message) => button(text(label).size(13))
                .style(button::text)
                .padding([2, 6])
                .width(Fill)
                .on_press_maybe(message)
                .into(),
            // Groups what is above it apart from what is below, which is the
            // whole of its job.
            MenuItem::Divider => divider(4.0),
        }))
        .spacing(2),
    )
    // Wide enough for its longest entry and no wider. Without this the menu
    // takes whatever the layer it floats on offers, which is the window.
    .width(width)
    .padding(4)
    .style(|theme: &Theme| {
        let palette = theme.extended_palette();

        container::Style {
            background: Some(palette.background.base.color.into()),
            border: Border {
                radius: FIELD_RADIUS.into(),
                width: 1.0,
                color: palette.background.strong.color,
            },
            ..container::Style::default()
        }
    })
    .into()
}

/// The soft line under a pane's heading.
///
/// A free function rather than a method: it borrows nothing, and as a method
/// its result would be tied to the borrow of `self`, which is shorter than the
/// widgets it has to sit alongside.
pub(crate) fn pane_rule<'a>() -> Element<'a, Message> {
    container(space::horizontal().height(1))
        .width(Fill)
        .style(|theme: &Theme| container::Style {
            background: Some(
                Color { a: 0.4, ..theme.extended_palette().background.strong.color }.into(),
            ),
            ..container::Style::default()
        })
        .into()
}

/// The bubble a tooltip is drawn in. Solid, so what is underneath does not
/// show through the explanation.
/// The same, for something that is wrong but is still working.
///
/// Amber rather than red, matching the border on the field it explains: a
/// setting that has to be sorted out before a run, not one that has already
/// broken something.
pub(crate) fn warning_tip_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    let mut style = tip_style(theme);

    style.border.color = palette.warning.base.color;
    style.text_color = Some(palette.warning.base.color);
    style
}

pub(crate) fn error_tip_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    let mut style = tip_style(theme);

    style.border.color = palette.danger.base.color;
    style.text_color = Some(palette.danger.base.color);
    style
}

pub(crate) fn tip_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();

    container::Style {
        background: Some(palette.background.base.color.into()),
        text_color: Some(palette.background.base.text),
        border: Border {
            radius: FIELD_RADIUS.into(),
            width: 1.0,
            color: palette.background.strong.color,
        },
        ..container::Style::default()
    }
}

/// One of the three transport buttons.
///
/// Always the same three, always in the same place. What changes is whether
/// this one is lit: the colour says what the rig is doing, where the icon says
/// what the button is for.
///
/// A function with a named lifetime rather than a closure, because `Tooltip`
/// is invariant over its lifetime — a closure taking `Element<'_, _>` gets a
/// fresh lifetime that cannot then be shortened to match what it returns.
pub(crate) fn transport<'a>(
    icon: Element<'a, Message>,
    tip: &'a str,
    message: Message,
    // The button's own padding is most of what separates one icon from the
    // next, so how tight a group is belongs to the group rather than here.
    padding: impl Into<iced::Padding>,
) -> Element<'a, Message> {
    tooltip(
        // No chrome. The state lives in the shape's own colour, so a button
        // drawn around it would be a second background saying the same thing
        // less clearly.
        button(icon).style(button::text).padding(padding).on_press(message),
        container(text(tip).size(13)).padding(4).style(tip_style),
        tooltip::Position::Bottom,
    )
    .into()
}

/// One of the three transport shapes, in the colour its state calls for.
///
/// Lit says what the rig is doing — reading, or recording — and unlit is the
/// ordinary text colour, so the three of them read as a row of controls rather
/// than as three lamps of which two are off. The shape is filled, so its
/// colour is the whole of the signal.
pub(crate) fn transport_mark<'a>(
    bytes: &'static [u8],
    lit: bool,
    colour: fn(&iced::theme::palette::Extended) -> Color,
) -> Element<'a, Message> {
    tinted_mark(bytes, TITLE_ICON, move |theme: &Theme| {
        let palette = theme.extended_palette();

        match lit {
            true => colour(palette),
            false => palette.background.base.text,
        }
    })
}

/// The mark for adding a channel, dimmed when there is nothing to press.
///
/// The lucide icons beside this one are text, so a button fades them with
/// itself when it has no message. An svg has no such connection to the button
/// holding it - it would stay bright against faded neighbours and read as the
/// one thing still available - so the state is handed to it.
pub(crate) fn add_channel_mark<'a>(offered: bool) -> Element<'a, Message> {
    tinted_mark(ADD_CHANNEL, ROW_ICON, move |theme: &Theme| {
        let text = theme.extended_palette().background.base.text;

        match offered {
            true => text,
            // The same halving iced fades a disabled button's own text by.
            false => Color { a: text.a * 0.5, ..text },
        }
    })
}

/// Something known about the thing selected, said rather than edited.
///
/// The reporting counterpart to `rig_field`: same shape on the page, no field,
/// because nothing here is something to change. What was recorded is what was
/// recorded.
pub(crate) fn fact<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![field_label(label), text(value).size(14)].spacing(2).into()
}

/// What a recorded device was, from the configuration stored with the run.
///
/// The backend's own name where the json gives one up, and the raw text where
/// it does not: a file written by a later lumberjack may hold a shape this one
/// has never heard of, and showing it is more use than admitting nothing.
pub(crate) fn recorded_hardware(hardware: &str) -> String {
    serde_json::from_str::<serde_json::Value>(hardware)
        .ok()
        .as_ref()
        .and_then(|json| json.get("type"))
        .and_then(|kind| kind.as_str())
        .map(|kind| kind.to_string())
        .unwrap_or_else(|| hardware.to_string())
}

/// How a channel row in a tree is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowLook {
    /// Nothing special about it.
    Plain,
    /// What the settings panel is talking about.
    Picked,
    /// On a plot, but not what is selected.
    Drawn,
}

/// A channel row: something to click, and something a drag can start from.
///
/// Not a `button`, deliberately. A button captures the press and publishes on
/// the *release*, and only if the pointer is still over it — so a press that
/// travels to a plot and lets go there never produces a message at all, which
/// is exactly the gesture a drag is. It also captures the press, so a
/// `MouseArea` wrapped round one never sees it either.
///
/// A `MouseArea` over a styled container reports the press when it happens,
/// which is what arms a drag, and leaves the release to be seen wherever it
/// lands.
pub(crate) fn channel_row<'a>(
    content: Element<'a, Message>,
    look: RowLook,
    on_press: Message,
    on_right_press: Message,
) -> Element<'a, Message> {
    let body = container(content).padding(4).width(Fill).style(move |theme: &Theme| {
        let palette = theme.extended_palette();

        match look {
            RowLook::Plain => container::Style::default(),
            RowLook::Picked => container::Style {
                background: Some(palette.primary.base.color.into()),
                text_color: Some(palette.primary.base.text),
                border: Border { radius: FIELD_RADIUS.into(), ..Border::default() },
                ..container::Style::default()
            },
            RowLook::Drawn => container::Style {
                background: Some(palette.secondary.base.color.into()),
                text_color: Some(palette.secondary.base.text),
                border: Border { radius: FIELD_RADIUS.into(), ..Border::default() },
                ..container::Style::default()
            },
        }
    });

    MouseArea::new(body).on_press(on_press).on_right_press(on_right_press).into()
}

/// The brand marks, kept beside the project rather than inside the interface
/// that happens to draw them: the same logo belongs on the readme.
pub(crate) const LOGO: &[u8] = include_bytes!("../../assets/Lumberjack.svg");
pub(crate) const BRAND_FONT: &[u8] = include_bytes!("../../assets/IBMPlexMono-Medium.ttf");
pub(crate) const RECORD_MODE: &[u8] = include_bytes!("../../assets/RecordMode.svg");
/// The red of the logo, for the one thing that is the brand's rather than the
/// theme's. Everything else takes its colour from the palette so it follows
/// whichever theme is chosen; a brand mark does not change with the furniture.
pub(crate) const BRAND_RED: Color = Color::from_rgb(205.0 / 255.0, 100.0 / 255.0, 98.0 / 255.0);

/// How tall the transport shapes are drawn.
pub(crate) const TITLE_ICON: f32 = 26.0;

/// How tall the logo and the two mode marks are drawn.
///
/// Larger than the transport shapes, and larger for a reason: those are bare
/// silhouettes where these are filled tiles with a glyph knocked out of them,
/// so the same box height gives the tiles far less room for their detail.
pub(crate) const MODE_ICON: f32 = 32.0;

pub(crate) const PLAY: &[u8] = include_bytes!("../../assets/Play.svg");
pub(crate) const STOP: &[u8] = include_bytes!("../../assets/Stop.svg");
pub(crate) const RECORD: &[u8] = include_bytes!("../../assets/Record.svg");
pub(crate) const DATA_MODE: &[u8] = include_bytes!("../../assets/DataMode.svg");
pub(crate) const ADD_CHANNEL: &[u8] = include_bytes!("../../assets/AddChannel.svg");

/// How tall a mark drawn in a row of controls is.
///
/// Matched to the `.size(14)` the lucide glyphs beside it are set at. The two
/// are measured differently - a glyph leaves room inside its box and this file
/// draws to its own edges - so if it ever reads a shade large beside them, this
/// is the number to nudge rather than the drawing.
pub(crate) const ROW_ICON: f32 = 14.0;

/// The face the name is set in.
///
/// Named by its typographic family rather than by the file: the name table
/// calls the family "IBM Plex Mono Medium" and the *typographic* family "IBM
/// Plex Mono" with a subfamily of Medium, and it is the latter pair that a
/// font database matches on. Asking for the file's own family name would find
/// nothing and fall back to the default face without saying so.
pub(crate) const BRAND: iced::Font = iced::Font {
    family: iced::font::Family::Name("IBM Plex Mono"),
    weight: iced::font::Weight::Medium,
    stretch: iced::font::Stretch::Normal,
    style: iced::font::Style::Normal,
};

/// What a value that cannot be edited says about itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    /// Ordinary: a fact, neither good nor bad.
    Plain,
    Good,
    Bad,
}

/// Something the interface knows, laid out like a field but not one.
///
/// Shaped like the fields around it so a panel reads as one list rather than
/// as prose interrupted by inputs, and dimmed so it does not invite typing.
/// The dimming is the whole of the distinction, which is why it has to be
/// visible: a read only box that looks editable teaches the wrong thing about
/// every other box on the panel.
pub(crate) fn read_only<'a>(label: &'a str, value: String, tone: Tone) -> Element<'a, Message> {
    let field = container(text(value).size(14).style(move |theme: &Theme| {
        let palette = theme.extended_palette();

        text::Style {
            color: Some(match tone {
                Tone::Plain => palette.background.weak.text,
                Tone::Good => palette.success.base.color,
                Tone::Bad => palette.danger.base.color,
            }),
        }
    }))
    .width(Fill)
    .padding([5, 8])
    .style(|theme: &Theme| {
        let palette = theme.extended_palette();

        container::Style {
            // No fill at all. A filled box is what an editable field looks
            // like here, so the distinction is that this one is only an
            // outline - and the outline is the application's own background
            // colour, which reads as an inset line against the lighter pane
            // and stays right in whatever theme is chosen.
            background: None,
            border: Border {
                radius: FIELD_RADIUS.into(),
                width: 1.0,
                color: palette.background.base.color,
            },
            ..container::Style::default()
        }
    });

    column![field_label(label), field].spacing(2).into()
}

/// A hairline across the width, with `gap` of room either side of it.
///
/// The line is the inner container and the padding belongs to the outer one.
/// Painting a padded container makes the padding part of the line, which is how
/// a one pixel rule once came out five thick.
pub(crate) fn divider<'a>(gap: f32) -> Element<'a, Message> {
    container(
        container(space::horizontal().height(1)).width(Fill).style(|theme: &Theme| {
            container::Style {
                background: Some(theme.extended_palette().background.strong.color.into()),
                ..container::Style::default()
            }
        }),
    )
    .width(Fill)
    .padding(padding::top(gap).bottom(gap))
    .into()
}

/// A control that is plain until the pointer is on it, and then is a warning.
///
/// Deleting is a press away wherever it appears, so it should not shout while
/// somebody is reading past it — but it must not be mistaken for something
/// harmless at the moment it is about to be pressed either.
pub(crate) fn danger_on_hover(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let mut style = button::text(theme, status);

    style.text_color = match status {
        button::Status::Hovered | button::Status::Pressed => palette.danger.base.color,
        button::Status::Active => palette.background.base.text,
        button::Status::Disabled => palette.background.weak.text,
    };
    style
}

/// How wide across the connection dot is.
pub(crate) const DOT: f32 = 8.0;

/// Whether a device is talking to its hardware, said in one dot.
///
/// Directly after the name rather than at the end of the row: it belongs to
/// the name, and a column of dots down the right hand edge would read as a
/// separate thing to scan.
pub(crate) fn connection_dot<'a>(health: Health) -> Element<'a, Message> {
    let colour = match health {
        // Nothing has looked, so there is nothing to say. A dot in any colour
        // would be an answer, and the honest one is no dot at all.
        Health::Unknown => return space::horizontal().width(0).into(),
        Health::Fine => |palette: &iced::theme::palette::Extended| palette.success.base.color,
        Health::Troubled => |palette: &iced::theme::palette::Extended| palette.warning.base.color,
        Health::Down => |palette: &iced::theme::palette::Extended| palette.danger.base.color,
    };

    // A drawn circle rather than a text glyph. A glyph sits where the font
    // puts it in the line box, which is neither centred on the row nor the
    // size it was asked for - a filled circle at size 22 draws about eight
    // pixels of ink somewhere above the baseline. A box of a known size,
    // rounded until it is a circle, is exactly as tall as it looks, so it
    // centres with the row like anything else does.
    container(space::horizontal().width(DOT).height(DOT))
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();

            container::Style {
                background: Some(colour(palette).into()),
                border: Border { radius: (DOT / 2.0).into(), ..Border::default() },
                ..container::Style::default()
            }
        })
        .into()
}

/// How a device is doing, as far as anything has looked.
///
/// Four states rather than a `bool`, because "nobody has tried" and "tried and
/// failed" are different answers, and so are "reading" and "reading, but".
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Health {
    /// Nothing has tried yet.
    Unknown,
    /// Tried, and it is not there.
    Down,
    /// Reading, with something to complain about: frames it cannot use, or a
    /// stream that does not look like the config says. Amber rather than red
    /// because data is still arriving and calling that broken would be wrong.
    Troubled,
    Fine,
}

/// One of the marks, drawn at a given size.
///
/// Vector rather than a bitmap so it stays sharp at every text size the
/// settings offer, and so it follows `scale_factor` without a second file.
pub(crate) fn mark<'a>(bytes: &'static [u8], size: f32) -> Element<'a, Message> {
    svg(svg::Handle::from_memory(bytes)).width(size).height(size).into()
}

/// How wide one of the marks is for its height, from its own viewBox.
///
/// Parsed rather than written down beside each icon: the files are the source
/// of truth for their own shape, and a number copied out of one is a number
/// that goes stale when the icon is redrawn.
pub(crate) fn aspect_of(bytes: &[u8]) -> f32 {
    let text = std::str::from_utf8(bytes).unwrap_or_default();
    let Some(box_start) = text.find("viewBox=\"") else { return 1.0 };
    let rest = &text[box_start + 9..];
    let Some(end) = rest.find('"') else { return 1.0 };

    let numbers: Vec<f32> =
        rest[..end].split_whitespace().filter_map(|part| part.parse().ok()).collect();

    match numbers.as_slice() {
        [_, _, width, height] if *height > 0.0 => width / height,
        _ => 1.0,
    }
}

/// One of the marks, drawn in a colour of our choosing.
///
/// `svg::Style` carries a colour filter that replaces every colour in the
/// file, so a single colour silhouette can be tinted per state. Only for marks
/// that are one colour: the logo has two, and tinting would flatten it.
pub(crate) fn tinted_mark<'a>(
    bytes: &'static [u8],
    size: f32,
    colour: impl Fn(&Theme) -> Color + 'a,
) -> Element<'a, Message> {
    svg(svg::Handle::from_memory(bytes))
        // Both dimensions, taken from the file's own aspect. Setting only the
        // height and leaving the width to shrink looks equivalent and is not:
        // `ContentFit::Contain` fits inside *both*, so whatever width the
        // layout happened to leave over would scale the icon down. Widening a
        // button's padding then made its icon smaller, which is not a
        // relationship anybody would go looking for.
        .height(size)
        .width(size * aspect_of(bytes))
        .style(move |theme: &Theme, _status| svg::Style { color: Some(colour(theme)) })
        .into()
}

/// An icon-only control, with the words it hasn't got.
///
/// A button showing only a symbol is quick to use once you know it and opaque
/// until then, and a tooltip is how it stops being opaque without taking the
/// room a label would.
///
/// A free function rather than a method for the same reason `transport` is:
/// `Tooltip` is invariant in its lifetime, so building one inside a method
/// would tie the result to the borrow of `self` rather than to the interface
/// it belongs in.
pub(crate) fn hint<'a>(control: impl Into<Element<'a, Message>>, tip: &'a str) -> Element<'a, Message> {
    tooltip(
        control,
        container(text(tip).size(13)).padding(4).style(tip_style),
        tooltip::Position::Bottom,
    )
    .into()
}

/// Something with a message waiting behind it.
///
/// Takes the message by value rather than by reference, because these are
/// worked out while drawing - the verdict on what is in a field - and so there
/// is nothing for a borrow to point at.
///
/// A free function for the same reason `transport` and `hint` are: `Tooltip`
/// is invariant in its lifetime, so building one inside a method would tie the
/// result to the borrow of `self`.
pub(crate) fn explaining<'a>(
    control: Element<'a, Message>,
    message: Option<String>,
) -> Element<'a, Message> {
    // Wrapped whether or not there is anything to say, and that is the whole
    // point. iced knows a widget's state - a text field's focus and its caret
    // among it - by where the widget sits in the tree and what it is. Wrapping
    // a field only once it is wrong changes what sits at that position, so the
    // keystroke that made it wrong threw the cursor out of the box, which made
    // an equation nearly impossible to type.
    //
    // With nothing to say the bubble is an empty, unstyled space: it is still
    // there, and it draws nothing.
    bubbled(control, message, error_tip_style)
}

/// The same, in the amber of a setting that wants sorting out rather than the
/// red of one that is already broken.
pub(crate) fn warning_about<'a>(
    control: Element<'a, Message>,
    message: Option<String>,
) -> Element<'a, Message> {
    bubbled(control, message, warning_tip_style)
}

/// A control with a bubble behind it, in whichever colour was asked for.
fn bubbled<'a>(
    control: Element<'a, Message>,
    message: Option<String>,
    style: fn(&Theme) -> container::Style,
) -> Element<'a, Message> {
    let bubble: Element<'a, Message> = match message {
        Some(message) => {
            container(text(message).size(13)).padding(6).max_width(320).style(style).into()
        }
        None => space::horizontal().width(0).into(),
    };

    tooltip(control, bubble, tooltip::Position::Bottom).into()
}

/// A label with its explanation behind it, rather than beneath it.
///
/// The paragraph a setting needs to be understood is worth having and not
/// worth the room it takes once it has been read, which is what a tooltip is
/// for.
pub(crate) fn labelled<'a>(label: &'a str, explanation: &'a str) -> Element<'a, Message> {
    tooltip(
        field_label(label),
        container(text(explanation).size(13)).padding(6).max_width(260).style(tip_style),
        tooltip::Position::Right,
    )
    .into()
}

/// The background a plot's card is drawn on.
///
/// One definition because the card and the plot inside it have to agree: the
/// point of the card is that a plot and its legend look like one thing.
pub(crate) fn card_colour(theme: &Theme) -> Color {
    theme.extended_palette().background.weaker.color
}
