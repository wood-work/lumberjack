# lumbergui

The graphical interface to lumberdaq, built with [iced](https://iced.rs). The
only one of the three that can *edit* a setup: the CLI and the terminal monitor
read a `config.json` somebody else wrote, and this writes it.

```cmd
cargo run -p lumbergui -- lumberdaq/test_projects/scaled
```

From the repository root, as above. Started with no argument it comes up asking
for a project and offering the last one that was open, which is the only state
in which there is nothing to show. `mock_sine` and `scaled` need nothing plugged
in.

## What it is for

Four panes, arranged as a grid you can drag about:

- **Devices** — the rig as a tree, every device with its channels and their
  latest readings, and a dot saying whether each answered.
- **Configuration** — whatever is selected in that tree, and what can be changed
  about it. Ports, baud rates, channel indices, scales, equations.
- **Plots** — live traces while recording, or recorded ones in data mode.
- **Log** — what has happened, the same lines that go to the log file.

The transport is play, stop and record. Play opens the devices and starts
reading; record writes what is read into `results.db`. They are separate because
watching a rig and recording from it are different intentions, and a session
only being watched should leave no results file behind.

## Two modes

The mark at the top right switches between them.

**Record mode** is a live rig: devices, their settings, and plots that scroll.

**Data mode** reads `results.db` back — a tree of runs, the devices in each and
their channels, plotted from what was stored. It opens results written by older
versions of lumberdaq as well, so a release does not orphan what came before.

Channels reach a plot the same way in both: drag one onto a plot, or right click
it and choose a plot by name.

## Settings and the log

Both live beside each other, in `%APPDATA%\lumberjack` on Windows and
`$XDG_CONFIG_HOME/lumberjack` or `~/.config/lumberjack` elsewhere:

```
settings.json      theme, font size, the last project opened
lumberjack.log     what happened, kept across sessions
```

The log pane holds the last couple of hundred lines and loses them when the
window closes, which is right while somebody is watching and no use at all
afterwards — and afterwards is when a log is usually wanted. The file is rolled
aside at a megabyte, so there are at most two of them. Panics are written there
too, with a backtrace: a panic closes the window and prints to a stderr that a
program started from Explorer does not have, so without this what somebody sees
is the application vanishing.

## Knowing whether the hardware is there

The dot beside a device has four states rather than two, because "nobody has
looked" and "looked and it is not there" are different answers:

| | |
|---|---|
| no dot | nothing has tried yet |
| green | it answered |
| amber | reading, but with something to complain about |
| red | it is not there |

Amber is for a device that is working and unhappy — serial frames it cannot
read, or a stream that does not match the frame pattern. Data is still arriving,
so calling it broken would be wrong.

The devices are checked when a project opens, when a run stops, and when the
refresh beside the Devices heading is pressed. Deliberately **not** on a timer:
opening a serial port asserts DTR, which resets an Arduino and most boards like
it, so a check that ran itself every half minute would reset somebody's hardware
all afternoon. Instead the interface listens for Windows saying something was
plugged in or unplugged, and asks the rig then.

## Sorting out a serial device

A baud rate cannot be worked out by asking, only by listening. The tick beside
the rate opens the port for a few seconds and says whether these settings read
this device; picking a rate starts the same test. If bytes arrive and none of
them are frames, the field is bordered amber and the log says so.

Give it a moment. Opening the port resets an Arduino, and its bootloader is
silent for a second or two before the sketch says anything.

Two devices pointed at one port is caught the same way, wherever it came from —
a dropdown, a loaded project, a merged library or a hand edited file — and the
Port field says which other device has it.

## Building a release

```cmd
cargo build --release -p lumbergui
```

`target/release/lumbergui.exe` runs on its own: the C runtime is linked
statically, so there is no redistributable to install beside it, and the icon is
compiled in. It still needs the vendor driver for whatever hardware a project
actually uses — see the [repository README](../README.md#drivers).

## How it is put together

The interface owns no acquisition. It builds a `Daq` from the library, hands it
a sink and runs it on a thread, and hears about readings and events through a
channel — the same public surface choptui uses. Nothing here reaches into
lumberdaq's internals, so a setting this cannot express is a gap in the library
rather than something to work around here.

Everything is threads and channels. There is no async runtime.

```
src/main.rs         state, messages, update and view
src/look.rs         styles, colours, the marks and the widget helpers
src/acquisition.rs  the bridge to a running Daq
src/settings.rs     what this person prefers, kept between sessions
src/logbook.rs      the log file, and the panic hook
src/devicewatch.rs  hearing from Windows that a device came or went
```

`main.rs` is much the largest, holding the message enum and `view`. Splitting it
further has to follow the Elm shape rather than fight it: state and its messages
belong together.
