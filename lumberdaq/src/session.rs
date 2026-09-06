use crate::device::Device;
use crate::hardware::HardwareDataAcquisition;
use crate::storage::Batch;
use crate::Error;
use std::sync::atomic::{ AtomicBool, Ordering };
use std::sync::mpsc::Sender;
use std::time::Instant;

/// Something worth telling the caller about while a run is in progress.
///
/// Once a device is being read by its own thread, nothing else can look at it:
/// the thread holds the only reference. So state that used to be readable from
/// `Daq` has to be reported as it happens instead.
pub enum DeviceEvent {
    /// A read failed for a reason that is not the device going away: a frame
    /// that would not parse, or a channel pointing somewhere it should not.
    Problem { device: String, error: Error },
    Connected { device: String },
    /// Something wrong that is not stopping the device reading: frames it had
    /// to skip, a stream that does not look like the config says it does.
    /// `None` means whatever it was has cleared.
    ///
    /// Sent on the change rather than every cycle, like `Connected`, because
    /// it stands until it is contradicted and repeating it says nothing new.
    /// A caller wanting the current answer at any moment asks the device.
    Concern { device: String, concern: Option<String> },
    /// The cause is a message rather than the error, because the device keeps
    /// the original in its connection status and errors cannot be cloned.
    Disconnected { device: String, cause: Option<String> },
}

/// What a device thread sends back. Data and events share one channel so they
/// stay in order relative to each other.
pub enum DeviceMessage {
    Data(Batch),
    Event(DeviceEvent),
}

/// Read one device until told to stop.
///
/// This is the whole of what a device thread does. It owns the device for the
/// duration, which is why no locking is involved anywhere: exclusive access is
/// a consequence of the borrow, not something enforced at runtime.
pub fn run_device(device: &mut Device, sender: Sender<DeviceMessage>, stop: &AtomicBool) {
    let interval = device.read_interval;
    let name = device.info.name.clone();
    let mut was_connected = device.is_connected();
    let mut was_troubled = false;
    // Sleep to a deadline rather than for a fixed duration. Sleeping for the
    // interval after doing the work would make the real period the interval
    // plus however long a read took, and that error accumulates.
    let mut next_read = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        next_read += interval;

        if let Err(error) = device.read() {
            let message = DeviceMessage::Event(DeviceEvent::Problem {
                device: name.clone(),
                error: error,
            });
            if sender.send(message).is_err() {
                return; // nobody is listening any more
            }
        }

        // Report the transition, so a device dropping out is heard once rather
        // than every cycle it stays down.
        let connected = device.is_connected();
        if connected != was_connected {
            let event = if connected {
                DeviceEvent::Connected { device: name.clone() }
            } else {
                DeviceEvent::Disconnected {
                    device: name.clone(),
                    cause: device.disconnection_cause().map(|error| error.to_string()),
                }
            };
            was_connected = connected;
            if sender.send(DeviceMessage::Event(event)).is_err() {
                return;
            }
        }

        // The same shape again for a device that is working but unhappy.
        //
        // Whether rather than what: a complaint that quotes the frame it
        // choked on says something different every cycle, and comparing the
        // text would report every one of them. The first is the one that
        // explains; the rest are the same fault continuing.
        let concern = device.hardware.concern();
        if concern.is_some() != was_troubled {
            was_troubled = concern.is_some();
            let event = DeviceEvent::Concern { device: name.clone(), concern };
            if sender.send(DeviceMessage::Event(event)).is_err() {
                return;
            }
        }

        for batch in device.drain_batches() {
            if batch.datapoints.is_empty() {
                continue;
            }
            if sender.send(DeviceMessage::Data(batch)).is_err() {
                return;
            }
        }

        // If a read overran the interval we are already late, so go straight
        // round again rather than sleeping a negative amount.
        let now = Instant::now();
        if next_read > now {
            std::thread::sleep(next_read - now);
        } else {
            next_read = now;
        }
    }
}
