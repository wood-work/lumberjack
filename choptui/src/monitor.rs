//! What the display is told, and the sink that tells it.

use lumberdaq::config::DaqConfig;
use lumberdaq::datapoint::DataPoint;
use lumberdaq::session::DeviceEvent;
use lumberdaq::storage::{ Batch, DataSink };
use lumberdaq::Result;
use std::sync::mpsc::Sender;

/// One thing the display needs to know about.
///
/// Errors become text here rather than being passed on. The display only ever
/// prints them, and a string crossing to another thread asks nothing of the
/// error type, which is the same reason `DeviceEvent::Disconnected` already
/// carries a message rather than the error itself.
pub enum Update {
    Data {
        device: String,
        channel: String,
        /// Every reading in the batch, not just the newest.
        ///
        /// A number on screen only needs the last one, but a plot drawn from
        /// one point per batch is a picture of the drain rate rather than of
        /// the signal: a 5 Hz sine collected ten times a second would come out
        /// as something slow and wrong. This is one allocation per batch,
        /// which is the same order as writing the batch to disk.
        points: Vec<DataPoint>,
    },
    Connected {
        device: String,
    },
    Disconnected {
        device: String,
        cause: Option<String>,
    },
    Problem {
        device: String,
        message: String,
    },
    /// A line the device sent that was not a reading. Its own state is
    /// untouched by it: a device that talks is not a device in trouble.
    Said {
        device: String,
        line: String,
    },
}

impl Update {
    /// Whatever a run reports about a device, in the form the display wants.
    pub fn from_event(event: DeviceEvent) -> Update {
        match event {
            DeviceEvent::Connected { device } => Update::Connected { device: device },
            DeviceEvent::Disconnected { device, cause } => {
                Update::Disconnected { device: device, cause: cause }
            }
            DeviceEvent::Problem { device, error } => {
                Update::Problem { device: device, message: error.to_string() }
            }
            // The display has no third state for a device that is reading but
            // unhappy, so it says so in words rather than in colour.
            DeviceEvent::Concern { device, concern } => Update::Problem {
                device: device,
                message: concern.unwrap_or_else(|| "reading cleanly again".to_string()),
            },
            DeviceEvent::Said { device, line } => Update::Said { device: device, line: line },
        }
    }
}

/// A sink that draws nothing.
///
/// It summarises each batch and hands it to the thread that does the drawing,
/// so a slow redraw can never hold up a write to disk, and the display is fed
/// by exactly the data that was recorded rather than by a second route through
/// the library.
pub struct Monitor {
    updates: Sender<Update>,
}

impl Monitor {
    pub fn new(updates: Sender<Update>) -> Monitor {
        Monitor { updates: updates }
    }
}

impl DataSink for Monitor {
    fn init(&mut self, _config: &DaqConfig) -> Result<()> {
        Ok(())
    }

    fn write_batch(&mut self, batch: &Batch) -> Result<()> {
        if batch.datapoints.is_empty() {
            return Ok(());
        }
        // Deliberately ignored. If the display has gone, the recording must
        // carry on: a monitor failing is no reason to stop a run, and this is
        // the whole of that policy.
        let _ = self.updates.send(Update::Data {
            device: batch.device.clone(),
            channel: batch.channel.clone(),
            points: batch.datapoints.clone(),
        });
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
