use crate::{ Error, Result };
use crate::calculated::{ Calculator, ChannelRef };
use crate::config::{ DaqConfig, DeviceConfig };
use crate::device::Device;
use crate::hardware::HardwareDataAcquisition;
use crate::session::{ run_device, DeviceEvent, DeviceMessage };
use crate::storage::DataSink;
use serde::{ Deserialize, Serialize };
use std::sync::atomic::{ AtomicBool, Ordering };
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::{ Duration, Instant };

#[derive(Serialize, Deserialize, Clone)]
pub struct DaqInfo {
    pub name: String,
    pub author: String,
}

/// The outcome of trying to connect a whole setup.
#[derive(Clone, Debug)]
pub struct ConnectionReport {
    pub connected: Vec<String>,
    /// Device name and the reason it could not be connected.
    pub failed: Vec<(String, String)>,
}

impl ConnectionReport {
    pub fn all_connected(&self) -> bool {
        self.failed.is_empty()
    }
}

/// A measurement session as it exists while running. Owns the devices and the
/// sink, so it is neither serializable nor cloneable; `config()` is how you
/// get something that is.
pub struct Daq {
    pub info: DaqInfo,
    pub devices: Vec<Device>,
    /// Channels worked out from the measured ones.
    ///
    /// Not in `devices`: it owns no hardware and gets no thread. It runs where
    /// every device's data already meets, in `collect`.
    pub calculated: Option<Calculator>,
    pub sink: Option<Box<dyn DataSink>>,
}
impl Daq {
    /// Build a whole running system from a saved setup.
    ///
    /// Where results get written is not decided here. That comes from the
    /// `Project` the config was loaded out of, and reaches the Daq as a sink.
    pub fn from_config(config: DaqConfig) -> Result<Daq> {
        // What the setup says about how fast each channel samples, gathered
        // before the device configs are consumed. Anything absent is a device
        // that streams at a rate of its own, which the calculator measures.
        let mut declared: BTreeMap<ChannelRef, Duration> = BTreeMap::new();
        for device in config.devices.iter() {
            if let Some(interval) = device.sample_interval() {
                for channel in device.hardware.channel_infos() {
                    declared.insert(
                        ChannelRef {
                            device: device.info.name.clone(),
                            channel: channel.name,
                        },
                        interval,
                    );
                }
            }
        }

        let mut devices: Vec<Device> = Vec::new();
        for device_config in config.devices.into_iter() {
            devices.push(Device::from_config(device_config)?);
        }
        let mut daq = Daq::new(config.info.name, config.info.author, devices)?;

        if let Some(calculated) = config.calculated {
            let calculator = Calculator::with_rates(calculated, &declared)?;
            // An equation naming a channel nothing provides is a typo, and
            // would otherwise show up as a calculated channel that silently
            // recorded nothing for the whole run.
            for source in calculator.sources() {
                let exists = daq.devices.iter().any(|device| {
                    device.info.name == source.device
                        && device.channels.iter().any(|c| c.info.name == source.channel)
                });
                if !exists {
                    return Err(Error::EquationSourceMissing {
                        channel: calculator.device_name().to_string(),
                        reads: source.to_string(),
                    });
                }
            }
            daq.calculated = Some(calculator);
        }
        Ok(daq)
    }

    /// Describe this setup so it can be saved. Contains no measurement data,
    /// no file paths and no open handles - only what is needed to rebuild it.
    pub fn config(&self) -> DaqConfig {
        DaqConfig {
            info: self.info.clone(),
            calculated: self.calculated.as_ref().map(|calc| calc.config()),
            devices: self.devices.iter().map(|device| device.config()).collect::<Vec<DeviceConfig>>(),
        }
    }

    pub fn new(name: String, author: String, devices: Vec<Device>) -> Result<Daq> {
        let mut daq = Daq {
            info: DaqInfo {
                name: name,
                author: author,
            },
            devices: vec![],
            calculated: None,
            sink: None,
        };
        // daq.add_device(devices.pop().unwrap());
        for device in devices.into_iter() {
            daq.add_device(device)?;
        }
        Ok(daq)
    }

    pub fn add_device(&mut self, device: Device) -> Result<()> {
        for existing_device in self.devices.iter() {
            if existing_device.info.name == device.info.name {
                return Err("Device name must be unique".into());
            }
        }
        self.devices.push(device);
        Ok(())
    }
    
    /// Try to connect every device, and say what happened.
    ///
    /// Deliberately does not stop at the first failure and does not return
    /// `Result`: one unreachable device should not decide whether the others
    /// get to record. Whether a partial setup is good enough to run is a
    /// judgement for the caller, who can ask the user; all this does is make
    /// sure the caller has the facts to decide with.
    pub fn connect(&mut self) -> ConnectionReport {
        let mut report = ConnectionReport { connected: vec![], failed: vec![] };
        for device in self.devices.iter_mut() {
            if device.connect() {
                report.connected.push(device.info.name.clone());
            } else {
                let cause = match device.disconnection_cause() {
                    Some(error) => error.to_string(),
                    None => "unknown".to_string(),
                };
                report.failed.push((device.info.name.clone(), cause));
            }
        }
        report
    }

    /// Read every device, returning whatever went wrong rather than stopping.
    ///
    /// A disconnected device is not an error here; it retries on its own
    /// schedule and contributes no data. What comes back is genuine trouble:
    /// a read that failed, or a retry that did not take.
    pub fn read(&mut self) -> Vec<(String, String)> {
        let mut problems: Vec<(String, String)> = Vec::new();
        for device in self.devices.iter_mut() {
            let was_connected = device.is_connected();
            let was_troubled = device.hardware.concern().is_some();
            if let Err(error) = device.read() {
                problems.push((device.info.name.clone(), error.to_string()));
            }
            // Losing a device is state rather than an error, so report the
            // moment it happens instead of every cycle it stays down.
            if was_connected && !device.is_connected() {
                let cause = match device.disconnection_cause() {
                    Some(error) => error.to_string(),
                    None => "unknown".to_string(),
                };
                problems.push((device.info.name.clone(), format!("lost connection: {}", cause)));
            }

            // A device that is working but unhappy. Reported when it starts
            // and when it stops, not for every cycle in between: a device
            // grumbling ten times a second would bury everything else in the
            // log, and the state is available to ask for at any time.
            //
            // Whether rather than what, so a fault that reports a different
            // frame each time is still one complaint. What it says is carried
            // by the line written when it started.
            match (was_troubled, device.hardware.concern()) {
                (false, Some(concern)) => {
                    problems.push((device.info.name.clone(), concern));
                }
                (true, None) => {
                    problems.push((
                        device.info.name.clone(),
                        "reading cleanly again".to_string(),
                    ));
                }
                _ => {}
            }
        }
        problems
    }

    /// Devices that are working but have something to complain about, paired
    /// with what it is.
    ///
    /// Separate from `disconnected_devices` because it is a different state
    /// and wants different treatment: these are returning data, and a caller
    /// showing them as failed would be wrong. Asked for rather than pushed,
    /// so an interface can paint a warning for as long as it is true.
    pub fn troubled_devices(&self) -> Vec<(String, String)> {
        self.devices
            .iter()
            .filter_map(|device| {
                device
                    .hardware
                    .concern()
                    .map(|concern| (device.info.name.clone(), concern))
            })
            .collect()
    }

    /// Devices that are not currently usable, paired with what went wrong.
    ///
    /// The cause is None for a device that has not been attempted yet, which is
    /// a different situation from one that was tried and failed.
    pub fn disconnected(&self) -> Vec<(&str, Option<&Error>)> {
        self.devices
            .iter()
            .filter(|device| !device.is_connected())
            .map(|device| (device.info.name.as_str(), device.disconnection_cause()))
            .collect()
    }

    /// Attach somewhere to record to, and write the header describing this test.
    ///
    /// Daq does not know or care which format that is.
    pub fn set_sink(&mut self, mut sink: Box<dyn DataSink>) -> Result<()> {
        sink.init(&self.config())?;
        self.sink = Some(sink);
        Ok(())
    }

    /// Drain every device's acquired data into the sink. Does nothing if no
    /// sink is attached, so acquisition without recording is not an error.
    pub fn write(&mut self) -> Result<()> {
        let sink = match &mut self.sink {
            Some(sink) => sink,
            None => return Ok(()),
        };
        for device in self.devices.iter_mut() {
            device.write(sink.as_mut())?;
        }
        Ok(())
    }

    /// Push buffered data out to disk. How often this is called is what bounds
    /// how much a crash can lose.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(sink) = &mut self.sink {
            sink.flush()?;
        }
        Ok(())
    }

    /// Record from every device, each on its own thread, until `stop` is set.
    ///
    /// Blocks for the length of the run. A device that samples slowly no longer
    /// holds up one that samples quickly, which is the point of the threads;
    /// each keeps its own schedule from its `sample_interval`.
    ///
    /// These are scoped threads, so a thread borrows its device rather than
    /// taking it. The Daq keeps its devices and they can be inspected again
    /// afterwards, and nothing needs a lock: exclusive access falls out of the
    /// borrow. The cost is that this call cannot return while a thread is still
    /// running, which is what `stop` is for.
    ///
    /// While a run is in progress the devices cannot be read from here, so
    /// anything worth knowing arrives through `on_event`.
    pub fn run(
        &mut self,
        stop: &AtomicBool,
        on_event: &mut dyn FnMut(DeviceEvent),
    ) -> Result<()> {
        // Split the borrow: the threads take the devices, this thread keeps the
        // sink. Disjoint fields, so the compiler allows both at once.
        let devices = &mut self.devices;
        let sink = &mut self.sink;
        let (sender, receiver) = mpsc::channel::<DeviceMessage>();

        let calculated = &mut self.calculated;

        thread::scope(|scope| {
            for device in devices.iter_mut() {
                let sender = sender.clone();
                scope.spawn(move || run_device(device, sender, stop));
            }
            // Drop this thread's copy, so the receive loop ends when the last
            // device thread finishes rather than waiting for a sender forever.
            drop(sender);

            collect(receiver, sink, calculated, on_event)
        })
    }

    /// Record for a fixed length of time.
    pub fn run_for(
        &mut self,
        duration: Duration,
        on_event: &mut dyn FnMut(DeviceEvent),
    ) -> Result<()> {
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(duration);
                stop.store(true, Ordering::Relaxed);
            });
            self.run(&stop, on_event)
        })
    }
}

/// How long data may sit in the sink's buffer before being pushed to disk.
///
/// This is what bounds a crash, so it is a time rather than a message count: a
/// fast device and a slow one lose the same amount.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Take what the device threads send, write it, and pass on anything the caller
/// should hear about.
fn collect(
    receiver: mpsc::Receiver<DeviceMessage>,
    sink: &mut Option<Box<dyn DataSink>>,
    calculated: &mut Option<Calculator>,
    on_event: &mut dyn FnMut(DeviceEvent),
) -> Result<()> {
    let mut last_flush = Instant::now();
    loop {
        // Waiting with a timeout rather than blocking means an idle rig still
        // flushes on schedule instead of holding data until something arrives.
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(DeviceMessage::Data(batch)) => {
                // Calculated first, so both the measurement and what was worked
                // out from it are written together.
                let derived = match calculated.as_mut() {
                    Some(calculator) => calculator.apply(&batch, on_event),
                    None => Vec::new(),
                };
                if let Some(sink) = sink.as_mut() {
                    sink.write_batch(&batch)?;
                    for batch in derived.iter() {
                        sink.write_batch(batch)?;
                    }
                }
            }
            Ok(DeviceMessage::Event(event)) => on_event(event),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // Every device thread has finished.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if last_flush.elapsed() >= FLUSH_INTERVAL {
            if let Some(sink) = sink.as_mut() {
                sink.flush()?;
            }
            last_flush = Instant::now();
        }
    }

    if let Some(sink) = sink.as_mut() {
        sink.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{ mock_hardware, Hardware };

    /// A mock device always connects; Hardware::None never does. Together they
    /// stand in for a rig where one instrument is unplugged.
    fn one_good_one_bad() -> Daq {
        let mut good = mock_hardware::create_device("Good".to_string()).unwrap();
        mock_hardware::add_channel_random(&mut good, "Random".to_string()).unwrap();
        let bad = Device::new("Bad".to_string(), Hardware::None);
        Daq::new("Test".to_string(), "-".to_string(), vec![good, bad]).unwrap()
    }

    #[test]
    fn connecting_reports_every_device_rather_than_stopping_at_the_first_failure() {
        let mut daq = one_good_one_bad();
        let report = daq.connect();
        assert_eq!(report.connected, vec!["Good".to_string()]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "Bad");
        assert!(!report.all_connected());
    }

    #[test]
    fn a_failed_device_does_not_stop_the_others_recording() {
        let mut daq = one_good_one_bad();
        daq.connect();

        // The failed connect above counts as the attempt, so the bad device is
        // waiting quietly rather than reporting again. What matters is that the
        // good device read regardless.
        let problems = daq.read();
        assert!(problems.is_empty());
        assert_eq!(daq.devices[0].channels[0].datapoints.len(), 1);

        // And it keeps recording on later cycles too.
        daq.read();
        assert_eq!(daq.devices[0].channels[0].datapoints.len(), 2);
    }

    #[test]
    fn a_failed_device_is_not_retried_every_cycle() {
        let mut daq = one_good_one_bad();
        daq.connect();
        // The connect above counts as the attempt, so the next read is inside
        // the retry interval and stays quiet.
        assert!(daq.read().is_empty());
        assert!(daq.read().is_empty());
    }

    #[test]
    fn disconnected_devices_can_be_listed_with_their_reasons() {
        let mut daq = one_good_one_bad();
        daq.connect();
        let down = daq.disconnected();
        assert_eq!(down.len(), 1);
        assert_eq!(down[0].0, "Bad");
        // The cause comes back as the error itself, so a caller can decide what
        // to offer the user rather than reading the message.
        assert!(matches!(down[0].1, Some(Error::NoHardware)));
    }
}