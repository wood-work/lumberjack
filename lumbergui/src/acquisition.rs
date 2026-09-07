//! Reading a rig on a thread, and getting what it reads back to the interface.
//!
//! The library is synchronous and reads a device by blocking on it, which an
//! interface cannot do and stay drawable. So a run owns a thread, the
//! interface owns a channel, and the two only ever meet as messages - no
//! locks, and no async runtime to introduce.

use crate::CHANNEL_DEPTH;
use lumberdaq::config::DaqConfig;
use lumberdaq::project::Project;
use lumberdaq::datapoint::DataPoint;
use lumberdaq::session::DeviceEvent;
use lumberdaq::storage::{Batch, DataSink, Fanout, Recorder};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

/// What the acquisition thread has to say. Data and status share one channel
/// so they arrive in the order they happened, the same reason `DeviceMessage`
/// does it that way inside lumberdaq.
pub(crate) enum FromAcquisition {
    Data { device: String, channel: String, datapoints: Vec<DataPoint> },
    Status(String),
    /// Every device has been tried, and reading is about to begin.
    ///
    /// Sent between connecting and running. Opening hardware takes time — a
    /// Pico is about a second — and until this arrives there is nothing to
    /// draw, so the interface waits rather than scrolling an empty chart.
    Ready,
    /// Whether one device is talking to its hardware.
    ///
    /// Kept apart from `Status` rather than read back out of a log line: the
    /// library says which device and whether it came or went, and turning that
    /// into a sentence and then parsing the sentence would be throwing the
    /// answer away and guessing it back.
    Connection { device: String, connected: bool },
    /// A device that is reading but has something to complain about, or - with
    /// `None` - one that has stopped complaining.
    Trouble { device: String, concern: Option<String> },
}

/// A `DataSink` feeding the interface instead of a file.
///
/// A sink is how lumberdaq already offers data to something other than
/// storage — `Fanout`'s documentation calls this case out as "a display fed
/// the same data as the file it is being written to" — so watching a run
/// needs no change to the library.
pub(crate) struct DisplaySink {
    to_interface: SyncSender<FromAcquisition>,
}

impl DataSink for DisplaySink {
    fn init(&mut self, _config: &DaqConfig) -> lumberdaq::Result<()> {
        Ok(())
    }

    /// Never fails, on purpose.
    ///
    /// `Fanout` reports a failing sink and that ends the run, which is the
    /// right answer for a disk that has filled up and the wrong one for a
    /// window somebody closed. So a full queue drops the batch and a vanished
    /// receiver is ignored: the display losing points costs nothing, while
    /// blocking here would stall collection, and the writing to disk behind it
    /// in a run that was recording.
    fn write_batch(&mut self, batch: &Batch) -> lumberdaq::Result<()> {
        let _ = self.to_interface.try_send(FromAcquisition::Data {
            device: batch.device.clone(),
            channel: batch.channel.clone(),
            datapoints: batch.datapoints.clone(),
        });
        Ok(())
    }

    fn flush(&mut self) -> lumberdaq::Result<()> {
        Ok(())
    }
}

/// A run in progress: how to hear from it, how to stop it, and how to tell
/// when it has finished stopping.
pub(crate) struct Acquisition {
    pub(crate) from_acquisition: Receiver<FromAcquisition>,
    /// Set to end the run. Read only by the acquisition thread.
    pub(crate) stop: Arc<AtomicBool>,
    /// Set to record what is being read. The `Recorder` on the other side
    /// follows it: raising it builds a sink, lowering it flushes and drops
    /// one, so watching a run leaves no results file behind at all.
    pub(crate) recording: Arc<AtomicBool>,
    pub(crate) thread: thread::JoinHandle<()>,
}

/// Whether a run is going, and whether one is on its way out.
///
/// Stopping is not instant: the thread notices the flag when it next comes
/// round its read loop, so there is a moment where the run is neither going
/// nor finished. Starting another one before the last has let go of its
/// devices would mean two threads holding the same serial port.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunState {
    /// Asked for, and opening the hardware. Nothing is being read yet.
    ///
    /// Between pressing play and the first reading there is real work: a
    /// `Daq` is built, sinks are attached, and every device is opened, which
    /// for a Pico means loading a driver and initialising a unit over USB.
    /// Treating that as running made the chart scroll against a clock while
    /// nothing arrived, so the trace began a second adrift of the axis.
    Starting,
    Running,
    Stopping,
    Stopped,
}

/// Run the acquisition on a thread of its own, reporting back over a channel.
///
/// The `Daq` stays on that thread: `run` takes `&mut self` and blocks until
/// stopped, so while a run is in progress the devices are unreachable from
/// here. Everything the interface knows arrives through the channel, which is
/// why the device list is built from the config rather than from live devices.
pub(crate) fn start_acquisition(config: DaqConfig, directory: PathBuf) -> Acquisition {
    let (sender, receiver) = mpsc::sync_channel(CHANNEL_DEPTH);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_in_thread = Arc::clone(&stop);
    let recording = Arc::new(AtomicBool::new(false));
    let recording_in_thread = Arc::clone(&recording);
    // Read before the config is handed over, so the recorder knows what format
    // to write when somebody eventually asks it to.

    let thread = thread::spawn(move || {
        let report = |text: String| {
            let _ = sender.try_send(FromAcquisition::Status(text));
        };

        // from_config rather than lumberdaq::open: open attaches the project's
        // storage sink up front, which creates the results file whether or not
        // anybody records. The Recorder below builds one only when armed.
        let mut daq = match lumberdaq::daq::Daq::from_config(config) {
            Ok(daq) => daq,
            Err(error) => return report(format!("could not build the setup: {}", error)),
        };

        let recorder = Recorder::new(
            recording_in_thread,
            Box::new(move || {
                // Every recording goes in the one database and its runs table
                // tells them apart, so starting again needs nothing said about
                // where it should go.
                Project::new(&directory).sink()
            }),
        );

        // A run has one sink, so watching *and* recording means a sink that is
        // both. The display is first: a storage failure ends the run, and the
        // interface should have been given the batch before that happens.
        let sink = Fanout::new()
            .and("display", Box::new(DisplaySink { to_interface: sender.clone() }))
            .and("recording", Box::new(recorder));

        if let Err(error) = daq.set_sink(Box::new(sink)) {
            return report(format!("could not attach the sinks: {}", error));
        }

        let connected = daq.connect();
        report(format!(
            "{} of {} devices connected",
            connected.connected.len(),
            daq.devices.len()
        ));

        // What the first attempt found, device by device. Without this a rig
        // that connected cleanly would show nothing until something changed.
        for device in connected.connected.iter() {
            let _ = sender.try_send(FromAcquisition::Connection {
                device: device.clone(),
                connected: true,
            });
        }
        for (device, cause) in connected.failed.iter() {
            let _ = sender.try_send(FromAcquisition::Connection {
                device: device.clone(),
                connected: false,
            });
            // Why, and not merely that. A device that will not connect for
            // want of a driver says so here, in the words the backend uses,
            // which name the software to install.
            report(format!("{} did not connect: {}", device, cause));
        }

        // Everything that had to be opened has been tried. What follows is
        // reading, which produces data straight away.
        let _ = sender.try_send(FromAcquisition::Ready);

        let outcome = daq.run(&stop_in_thread, &mut |event| {
            // Both: the line for the log, and the fact for the dot beside the
            // device. A problem is neither - the device is still connected,
            // it just could not make sense of what it read.
            if let DeviceEvent::Connected { device } | DeviceEvent::Disconnected { device, .. } =
                &event
            {
                let _ = sender.try_send(FromAcquisition::Connection {
                    device: device.clone(),
                    connected: matches!(event, DeviceEvent::Connected { .. }),
                });
            }

            // Working but unhappy is its own state, and the dot beside the
            // device is where it shows.
            if let DeviceEvent::Concern { device, concern } = &event {
                let _ = sender.try_send(FromAcquisition::Trouble {
                    device: device.clone(),
                    concern: concern.clone(),
                });
            }

            let _ = sender.try_send(FromAcquisition::Status(match event {
                DeviceEvent::Problem { device, error } => format!("{}: {}", device, error),
                DeviceEvent::Concern { device, concern } => match concern {
                    Some(concern) => format!("{}: {}", device, concern),
                    None => format!("{} is reading cleanly again", device),
                },
                DeviceEvent::Connected { device } => format!("{} came back", device),
                DeviceEvent::Disconnected { device, cause } => {
                    format!("lost {}: {}", device, cause.unwrap_or_default())
                }
                DeviceEvent::Said { device, line } => format!("{} said: {}", device, line),
            }));
        });

        match outcome {
            Ok(()) => report("run finished".to_string()),
            Err(error) => report(format!("run ended: {}", error)),
        }
    });

    Acquisition { from_acquisition: receiver, stop, recording, thread }
}
