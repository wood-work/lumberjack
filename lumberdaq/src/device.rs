use crate::{ Error, Result };
use crate::channel::Channel;
use crate::config::{ default_read_interval_ms, DeviceConfig };
use crate::hardware::{ Hardware, HardwareDataAcquisition };
use crate::storage::{ Batch, DataSink };
use serde::{Deserialize, Serialize};
use std::time::{ Duration, Instant };

/// How long to leave a failed device alone before trying it again.
///
/// Retrying every read cycle would be worse than useless: opening a port that
/// is not there is slow, so a dead device would stall every other device on
/// every cycle.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub trait DeviceInterface {
    fn connect(&mut self) -> Result<()>;
    // fn read(&mut self) -> Result<()>;
}

#[derive(Serialize, Deserialize, Clone)]
/// What a device is called.
///
/// The name and nothing else. What the hardware *is* — a USB-6001, an ADC-24 —
/// is the hardware's to say and changes with what is plugged in, so it is asked
/// at the time rather than written down here and left to go stale.
pub struct DeviceInfo {
    pub name: String,
}

#[derive(Debug, Default)]
pub enum ConnectionStatus {
    Connected,
    /// Set up but never yet attempted.
    #[default]
    NeverConnected,
    /// Tried and failed. Carries the error itself rather than its message, so
    /// a caller can tell an unavailable port from a bad frame pattern and offer
    /// something useful about it, and the time of the attempt so retries stay
    /// spaced out.
    Disconnected {
        cause: Error,
        last_attempt: Instant,
    },
}

/// Whether a device is due another connection attempt.
///
/// A device that has never been tried is due immediately; one that failed is
/// due once `RETRY_INTERVAL` has passed since the last attempt.
fn retry_due(status: &ConnectionStatus) -> bool {
    match status {
        ConnectionStatus::Connected => false,
        ConnectionStatus::NeverConnected => true,
        ConnectionStatus::Disconnected { last_attempt, .. } => {
            last_attempt.elapsed() >= RETRY_INTERVAL
        }
    }
}

/// A device as it exists while running: its description, the data acquired so
/// far, and the hardware it is talking to. Not serializable - saving one means
/// asking it for its `DeviceConfig`.
pub struct Device {
    pub info: DeviceInfo,
    /// Whether this device is used at all. See `DeviceConfig::enabled`.
    ///
    /// Kept on the running device as well as in the config because the running
    /// system is what `Daq::config` writes back out, so a device that forgot
    /// it was switched off would switch itself on again the next time the
    /// project was saved.
    pub enabled: bool,
    /// How often this device's thread reads it.
    pub read_interval: Duration,
    pub channels: Vec<Channel>,
    pub hardware: Hardware,
    pub connection: ConnectionStatus,
}

impl Device {
    /// Build a running device from its description.
    ///
    /// Channels go through `add_channel`, so a config with duplicate channel
    /// names is rejected here rather than surfacing later.
    pub fn from_config(config: DeviceConfig) -> Result<Device> {
        let mut device = Device {
            info: config.info,
            enabled: config.enabled,
            read_interval: Duration::from_millis(config.read_interval_ms),
            channels: vec![],
            hardware: Hardware::from_config(config.hardware)?,
            connection: ConnectionStatus::default(),
        };
        device.rebuild_channels()?;
        Ok(device)
    }

    /// Describe this device so it can be saved.
    ///
    /// The channels are not listed here; they live in the hardware config,
    /// which is the one place they are defined.
    pub fn config(&self) -> DeviceConfig {
        DeviceConfig {
            info: self.info.clone(),
            read_interval_ms: self.read_interval.as_millis() as u64,
            hardware: self.hardware.config(),
            enabled: self.enabled,
        }
    }

    /// Mirror the hardware's channel definitions onto this device.
    ///
    /// The hardware config decides which channels exist and in what order;
    /// this builds the matching buffers. Discards any data already collected,
    /// so it belongs to setting a device up, not to running it.
    pub fn rebuild_channels(&mut self) -> Result<()> {
        self.channels = vec![];
        for info in self.hardware.config().channel_infos().into_iter() {
            self.add_channel(Channel::from_info(info)?)?;
        }
        Ok(())
    }

    pub fn new(name: String, hardware: Hardware) -> Device {
        Device {
            info: DeviceInfo { name },
            enabled: true,
            read_interval: Duration::from_millis(default_read_interval_ms()),
            channels: vec![],
            hardware: hardware,
            connection: ConnectionStatus::default(),
        }
    }

    pub fn add_channel(&mut self, channel: Channel) -> Result<()> {
        for existing_channel in self.channels.iter() {
            if existing_channel.info.name == channel.info.name {
                return Err("Channel name must be unique".into());
            }
        }
        self.channels.push(channel);
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.connection, ConnectionStatus::Connected)
    }

    /// What went wrong last time this device was tried.
    ///
    /// None if it is connected, or if it has never been attempted, which are
    /// distinguishable through `connection` itself.
    pub fn disconnection_cause(&self) -> Option<&Error> {
        match &self.connection {
            ConnectionStatus::Disconnected { cause, .. } => Some(cause),
            _ => None,
        }
    }

    fn mark_disconnected(&mut self, cause: Error) {
        self.connection = ConnectionStatus::Disconnected {
            cause: cause,
            last_attempt: Instant::now(),
        };
    }

    /// Attempt a connection, recording the outcome.
    ///
    /// Returns whether the device is now usable. It deliberately does not
    /// return the error: a failure is kept as state on the device, reachable
    /// through `disconnection_cause`, because a device being unavailable is an
    /// ordinary operating condition here rather than a failure of this call.
    /// Handing the error back as well would mean owning it in two places, and
    /// errors cannot be cloned.
    pub fn connect(&mut self) -> bool {
        match self.hardware.connect() {
            Ok(()) => self.connection = ConnectionStatus::Connected,
            Err(error) => self.mark_disconnected(error),
        }
        self.is_connected()
    }

    /// Read this cycle's data, or quietly work towards being able to.
    ///
    /// A disconnected device is retried at most every `RETRY_INTERVAL` and
    /// otherwise does nothing, so an unplugged cable costs one attempt every
    /// few seconds rather than stalling every cycle.
    ///
    /// A read that fails is treated as having lost the device, so the same
    /// retry path picks it up rather than the error repeating every cycle.
    pub fn read(&mut self) -> Result<()> {
        if !self.is_connected() {
            if retry_due(&self.connection) {
                self.connect();
            }
            return Ok(());
        }

        match self.hardware.read() {
            Ok(mut input_readings) => {
                for (channel, datapoints) in self.channels.iter_mut().zip(input_readings.iter_mut())
                {
                    // Dropped here rather than never read. The readings arrive
                    // in the order the channels are declared, so leaving a
                    // channel out of either list would put every reading after
                    // it under the wrong name; the pair has to stay whole and
                    // the unwanted one discarded at the end of it.
                    //
                    // Discarded rather than stored and ignored, so a channel
                    // switched off for a fortnight does not quietly fill
                    // memory with readings nobody will ever ask for.
                    if !channel.info.enabled {
                        continue;
                    }
                    channel.add_datapoints(datapoints)?;
                }
                Ok(())
            }
            // The device has gone. That becomes state for the retry path to
            // pick up, not an error from this call, and the cause is kept.
            Err(error) if error.is_connection_lost() => {
                self.mark_disconnected(error);
                Ok(())
            }
            // The port is fine; the data or the configuration is wrong. Say so,
            // and leave the connection alone: reconnecting a healthy port fixes
            // nothing and loses whatever arrives meanwhile.
            Err(error) => Err(error),
        }
    }

    /// Hand over everything acquired since the last call, one batch per channel.
    pub fn drain_batches(&mut self) -> Vec<Batch> {
        let device_name = &self.info.name;
        let mut batches = Vec::with_capacity(self.channels.len());
        for channel in self.channels.iter_mut() {
            batches.push(channel.drain_batch(device_name));
        }
        batches
    }

    pub fn write(&mut self, sink: &mut dyn DataSink) -> Result<()>{
        for batch in self.drain_batches() {
            sink.write_batch(&batch)?;
        }
        Ok(())
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each device gets its own thread, so a Device must be movable into one.
    /// This fails to compile if a field ever stops being Send.
    #[test]
    fn a_device_can_move_to_its_own_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Device>();
    }

    fn failed_at(when: Instant) -> ConnectionStatus {
        ConnectionStatus::Disconnected {
            cause: Error::NotConnected { port: "COM9".to_string() },
            last_attempt: when,
        }
    }

    #[test]
    fn a_device_never_tried_is_due_immediately() {
        assert!(retry_due(&ConnectionStatus::default()));
        assert!(matches!(ConnectionStatus::default(), ConnectionStatus::NeverConnected));
    }

    #[test]
    fn a_device_that_just_failed_is_left_alone() {
        assert!(!retry_due(&failed_at(Instant::now())));
    }

    #[test]
    fn a_device_that_failed_long_enough_ago_is_due_again() {
        let long_ago = Instant::now().checked_sub(RETRY_INTERVAL * 2).unwrap();
        assert!(retry_due(&failed_at(long_ago)));
    }

    #[test]
    fn a_connected_device_is_never_due() {
        assert!(!retry_due(&ConnectionStatus::Connected));
    }

    /// Never attempted is a different thing from attempted and failed, and only
    /// the second has a cause to report.
    #[test]
    fn a_new_device_has_not_been_tried_rather_than_having_failed() {
        let device = Device::new("Test".to_string(), Hardware::None);
        assert!(!device.is_connected());
        assert!(matches!(device.connection, ConnectionStatus::NeverConnected));
        assert!(device.disconnection_cause().is_none());
    }

    #[test]
    fn a_failed_connection_keeps_the_error_itself() {
        // Hardware::None always refuses to connect.
        let mut device = Device::new("Test".to_string(), Hardware::None);
        assert!(!device.connect());
        assert!(!device.is_connected());
        // Matching the stored variant rather than a message: the whole point of
        // keeping the error is that callers stop parsing prose.
        assert!(matches!(device.disconnection_cause(), Some(Error::NoHardware)));
    }

    #[test]
    fn a_bad_frame_does_not_stand_the_device_down() {
        // A parse failure means the data was wrong, not that the port died, so
        // the device must stay connected rather than being reconnected.
        let error = crate::Error::FieldNotNumeric {
            channel: "Pressure".to_string(),
            index: 5,
            field: "STBY".to_string(),
            frame: "1,2.00,0,1,1,STBY".to_string(),
        };
        assert!(!error.is_connection_lost());
    }

    #[test]
    fn losing_the_port_does_stand_the_device_down() {
        let error = crate::Error::NotConnected { port: "COM3".to_string() };
        assert!(error.is_connection_lost());
        let unplugged = crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "device disconnected",
        ));
        assert!(unplugged.is_connection_lost());
    }

    #[test]
    fn reading_a_disconnected_device_does_not_produce_data() {
        let mut device = Device::new("Test".to_string(), Hardware::None);
        // The first read is due a retry. It fails, but a device being
        // unavailable is a state rather than a failure of this call, so the
        // cause is recorded and Ok comes back.
        assert!(device.read().is_ok());
        assert!(!device.is_connected());
        assert!(matches!(device.disconnection_cause(), Some(Error::NoHardware)));
        // The next read is inside the retry interval, so it does nothing at all
        // rather than hammering the missing device.
        assert!(device.read().is_ok());
        assert!(!device.is_connected());
    }
}

// Device deliberately does not implement DeviceInterface. That trait is the
// contract for talking to hardware, where connect either works or hands back
// an error. A Device wraps that in a state machine, so its connect reports
// usability instead, and keeps the cause.