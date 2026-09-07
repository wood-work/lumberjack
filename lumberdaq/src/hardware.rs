// Add module 'hardware' in hardware folder
pub mod mock_hardware;
pub mod ni_daqmx;
pub mod pico_hrdl;
pub mod serial_stream;

use crate::channel::ChannelInfo;
use crate::datapoint::DataPoint;
use crate::{ Error, Result };
use mock_hardware::{ MockHardware, MockHardwareConfig };
use ni_daqmx::{ NiDaqmx, NiDaqmxConfig };
use pico_hrdl::{ PicoHrdl, PicoHrdlConfig };
use serial_stream:: { SerialStream, SerialStreamConfig };
use crate::device::DeviceInterface;
use serde::{ Deserialize, Serialize };

/// Remove one item from a channel list, if it is there.
///
/// One function rather than the same bounds check written out per backend,
/// since the lists differ only in what they hold.
fn remove_at<T>(channels: &mut Vec<T>, index: usize) -> bool {
    match index < channels.len() {
        true => {
            channels.remove(index);
            true
        }
        false => false,
    }
}

pub trait HardwareDataAcquisition {
    fn read(&mut self) -> Result<Vec<Vec<DataPoint>>>;

    /// Something wrong that is not bad enough to fail a read.
    ///
    /// A device can be connected, returning data, and still have something to
    /// say - frames it had to skip, a stream that does not look like what the
    /// config claims. That is not an error, because erroring would throw away
    /// the good data alongside the bad, and it is not silence either.
    ///
    /// State rather than an event: it stands until the hardware is happy
    /// again, so a caller can show it for as long as it is true. Most hardware
    /// has nothing to say, hence the default.
    fn concern(&self) -> Option<String> {
        None
    }

    /// Lines the device sent that were not readings.
    ///
    /// Some hardware talks. A serial instrument prints a banner when it starts,
    /// a warning when a range is exceeded, whatever its firmware author thought
    /// worth saying - in among the frames and in the same stream. None of it is
    /// data: it has no channel to belong to and no value to be, so it is never
    /// recorded. It is worth reading, though, and it used to be discarded
    /// unseen on the way to the next frame.
    ///
    /// Taken rather than read, unlike `concern`: these are things that
    /// happened rather than a state that holds, so each line is handed over
    /// once and then gone. Most hardware says nothing, hence the default.
    fn said(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// How a piece of hardware is described in a config file.
///
/// This is the serializable half. It holds settings only, never an open port
/// or a driver handle, so it can be written to disk and read back without any
/// #[serde(skip)] to hide fields that would not survive the trip.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type")]  // Adds "type: MockHardware" identifies to serilaized output, https://serde.rs/enum-representations.html
pub enum HardwareConfig {
    MockHardware(MockHardwareConfig),
    PicoHrdl(PicoHrdlConfig),
    NiDaqmx(NiDaqmxConfig),
    SerialStream(SerialStreamConfig),
    None,
}

/// How often a device's channels produce a sample.
///
/// Needed to pair samples from different devices: a calculated channel takes
/// each input's nearest sample within half its own period, so that period has
/// to come from somewhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleRate {
    /// Every read takes a sample, so the device's read interval is the period.
    PerRead,
    /// The hardware keeps its own schedule at this interval, whatever the
    /// device's read interval is.
    Fixed(std::time::Duration),
    /// The device sends when it likes and nothing declares how often, so it can
    /// only be measured from what arrives.
    Unknown,
}

impl HardwareConfig {
    /// Which piece of hardware this points at, where the settings name one.
    ///
    /// A serial device is its port; a DAQmx device is the name NI MAX gave it.
    /// Both are exclusive - one program has a serial port open or it does not,
    /// and two devices addressing `Dev1` are addressing one box - so two
    /// devices naming the same one is a mistake rather than a preference.
    ///
    /// `None` where nothing in the config picks a particular piece. A mock has
    /// no hardware at all, and a Pico takes the first unit it finds rather
    /// than one it is told about, so two of those cannot be told apart from
    /// here and this says nothing about them.
    pub fn address(&self) -> Option<String> {
        let address = match self {
            HardwareConfig::SerialStream(serial) => serial.port.clone(),
            HardwareConfig::NiDaqmx(ni) => ni.device.clone(),
            HardwareConfig::MockHardware(_)
            | HardwareConfig::PicoHrdl(_)
            | HardwareConfig::None => return None,
        };

        // A setting nobody has filled in yet points at nothing, and nothing
        // cannot be taken twice.
        (!address.trim().is_empty()).then_some(address)
    }

    /// How often this hardware produces a sample per channel.
    pub fn sample_rate(&self) -> SampleRate {
        match self {
            HardwareConfig::MockHardware(config) => match config.acquisition {
                mock_hardware::Acquisition::Polled => SampleRate::PerRead,
                mock_hardware::Acquisition::Streaming { sample_interval_ms } => {
                    SampleRate::Fixed(std::time::Duration::from_millis(sample_interval_ms))
                }
            },
            HardwareConfig::PicoHrdl(config) => match config.acquisition {
                pico_hrdl::Acquisition::Polled => SampleRate::PerRead,
                pico_hrdl::Acquisition::Streaming { sample_interval_ms, .. } => {
                    SampleRate::Fixed(std::time::Duration::from_millis(sample_interval_ms))
                }
            },
            // The device streams at whatever rate it was built for, and nothing
            // in the configuration says what that is.
            // Polled only so far: every read converts, so the device read
            // interval is the period.
            HardwareConfig::NiDaqmx(_) => SampleRate::PerRead,
            HardwareConfig::SerialStream(_) => SampleRate::Unknown,
            HardwareConfig::None => SampleRate::Unknown,
        }
    }

    /// What kind of hardware this is, spelled as the config file spells it.
    ///
    /// The same names `serde(tag = "type")` writes, so an interface naming a
    /// backend and a file naming one cannot drift apart.
    pub fn type_name(&self) -> &'static str {
        match self {
            HardwareConfig::MockHardware(_) => "MockHardware",
            HardwareConfig::PicoHrdl(_) => "PicoHrdl",
            HardwareConfig::NiDaqmx(_) => "NiDaqmx",
            HardwareConfig::SerialStream(_) => "SerialStream",
            HardwareConfig::None => "None",
        }
    }

    /// The kinds of hardware a device can be, spelled as `type_name` spells
    /// them. What an interface offers when somebody is adding a device.
    ///
    /// `None` is not among them: it is what a device with nothing configured
    /// is, not something anybody chooses.
    pub const TYPE_NAMES: [&'static str; 4] =
        ["MockHardware", "PicoHrdl", "NiDaqmx", "SerialStream"];

    /// An empty configuration of the named kind, for a device being created.
    ///
    /// The starting values come from each backend's own `Default`, so what a
    /// new serial device or a new Pico begins as is decided where the rest of
    /// that backend is, not by whoever is drawing the form.
    pub fn of_type(type_name: &str) -> Option<HardwareConfig> {
        match type_name {
            "MockHardware" => Some(HardwareConfig::MockHardware(MockHardwareConfig::default())),
            "PicoHrdl" => Some(HardwareConfig::PicoHrdl(PicoHrdlConfig::default())),
            "NiDaqmx" => Some(HardwareConfig::NiDaqmx(NiDaqmxConfig::default())),
            "SerialStream" => Some(HardwareConfig::SerialStream(SerialStreamConfig::default())),
            _ => None,
        }
    }

    /// What this hardware is, in as much as the config can say.
    ///
    /// The kind and how it samples, which is all that is knowable without the
    /// device: what model it actually is has to be asked of the hardware, and
    /// on a machine where it is not plugged in there is nothing to ask. So this
    /// is the answer to fall back on rather than showing nothing.
    pub fn describe(&self) -> String {
        match self {
            HardwareConfig::MockHardware(config) => match config.acquisition {
                mock_hardware::Acquisition::Polled => "Mock, polled".to_string(),
                mock_hardware::Acquisition::Streaming { sample_interval_ms } => {
                    format!("Mock, streaming every {} ms", sample_interval_ms)
                }
            },
            HardwareConfig::PicoHrdl(config) => match config.acquisition {
                pico_hrdl::Acquisition::Polled => "Pico ADC, polled".to_string(),
                pico_hrdl::Acquisition::Streaming { sample_interval_ms, .. } => {
                    format!("Pico ADC, streaming every {} ms", sample_interval_ms)
                }
            },
            HardwareConfig::NiDaqmx(_) => "NI DAQmx, polled".to_string(),
            HardwareConfig::SerialStream(config) => {
                format!("Serial stream, {} baud", config.baudrate)
            }
            HardwareConfig::None => "Nothing configured".to_string(),
        }
    }

    /// What is wrong with this hardware's channels, in its own words.
    ///
    /// Each backend judges its own, because the rules are not shared: a Pico
    /// differential pair starts on an odd channel and takes the one above,
    /// while NI's takes the one four above. An interface offering those
    /// settings can report the answer without knowing either rule.
    ///
    /// Only what is decidable from the config. Whether the device exists and
    /// has that many inputs needs the hardware, and is found at connect.
    pub fn channel_problem(&self) -> Option<String> {
        let checked = match self {
            HardwareConfig::PicoHrdl(config) => pico_hrdl::check_channels(config),
            // No input count: this is the desk answer, and how many inputs the
            // device has is the driver's to say. An interface showing this is
            // looking at a config, not at hardware.
            HardwareConfig::NiDaqmx(config) => ni_daqmx::check_channels(config, None),
            HardwareConfig::MockHardware(_) => Ok(()),
            HardwareConfig::SerialStream(_) => Ok(()),
            HardwareConfig::None => Ok(()),
        };
        checked.err().map(|error| error.to_string())
    }

    /// Add a channel, bound to whatever input comes next.
    ///
    /// The binding is a guess this makes so a new channel is usable rather
    /// than invalid: the input after the last one in use, or the first if
    /// there are none. Each backend counts its own inputs, because they do not
    /// count alike — a Pico numbers from 1 and NI from 0, and what "the next
    /// one" means is theirs to say.
    pub fn add_channel(&mut self, info: ChannelInfo) -> bool {
        match self {
            HardwareConfig::MockHardware(config) => {
                config.channels.push(mock_hardware::MockHardwareChannel {
                    info,
                    input: mock_hardware::MockHardwareInput::Random,
                });
                true
            }
            HardwareConfig::PicoHrdl(config) => {
                let next = config.channels.iter().map(|channel| channel.channel).max();
                config.channels.push(pico_hrdl::PicoHrdlChannel {
                    info,
                    // Analog inputs count from 1 on these units.
                    channel: next.map_or(1, |highest| highest + 1),
                    // The widest range, which cannot clip whatever is wired to
                    // it. Narrowing it is a decision about the signal, and
                    // belongs to whoever knows what that signal is.
                    range: picolog::hrdl::VoltageRange::MilliVolts2500,
                    single_ended: true,
                });
                true
            }
            HardwareConfig::NiDaqmx(config) => {
                let next = config.channels.iter().map(|channel| channel.channel).max();
                config.channels.push(ni_daqmx::NiDaqmxChannel {
                    info,
                    // ai0 upwards.
                    channel: next.map_or(0, |highest| highest + 1),
                    range: (-10.0, 10.0),
                    single_ended: true,
                });
                true
            }
            HardwareConfig::SerialStream(config) => {
                let next = config.channels.iter().map(|channel| channel.index).max();
                config.channels.push(serial_stream::SerialStreamChannel {
                    info,
                    // Fields of a frame, counted from zero.
                    index: next.map_or(0, |highest| highest + 1),
                });
                true
            }
            HardwareConfig::None => false,
        }
    }

    /// Take one channel off this hardware.
    ///
    /// Returns whether there was one to take. The binding goes with it: a
    /// channel is its description and where its data comes from together, and
    /// removing half of either would leave the other pointing at nothing.
    pub fn remove_channel(&mut self, index: usize) -> bool {
        match self {
            HardwareConfig::MockHardware(config) => remove_at(&mut config.channels, index),
            HardwareConfig::PicoHrdl(config) => remove_at(&mut config.channels, index),
            HardwareConfig::NiDaqmx(config) => remove_at(&mut config.channels, index),
            HardwareConfig::SerialStream(config) => remove_at(&mut config.channels, index),
            HardwareConfig::None => false,
        }
    }

    /// One channel's description, to read.
    ///
    /// `channel_infos` copies the lot, which is what most callers want. This
    /// is for one that needs to hold on to a channel rather than take it away:
    /// an interface showing what a channel is called, where a copy would have
    /// to be kept alive alongside the config it came from.
    pub fn channel_info(&self, index: usize) -> Option<&ChannelInfo> {
        match self {
            HardwareConfig::MockHardware(config) => {
                config.channels.get(index).map(|channel| &channel.info)
            }
            HardwareConfig::PicoHrdl(config) => {
                config.channels.get(index).map(|channel| &channel.info)
            }
            HardwareConfig::NiDaqmx(config) => {
                config.channels.get(index).map(|channel| &channel.info)
            }
            HardwareConfig::SerialStream(config) => {
                config.channels.get(index).map(|channel| &channel.info)
            }
            HardwareConfig::None => None,
        }
    }

    /// One channel's description, to change.
    ///
    /// The counterpart of `channel_infos`, which can only hand out copies.
    /// Editing what a channel is called and what its numbers mean belongs to
    /// whoever is configuring a rig, and doing it through here keeps the
    /// reaching into each backend's own channel type in this file, where the
    /// differences between vendors already live.
    pub fn channel_info_mut(&mut self, index: usize) -> Option<&mut ChannelInfo> {
        match self {
            HardwareConfig::MockHardware(config) => {
                config.channels.get_mut(index).map(|channel| &mut channel.info)
            }
            HardwareConfig::PicoHrdl(config) => {
                config.channels.get_mut(index).map(|channel| &mut channel.info)
            }
            HardwareConfig::NiDaqmx(config) => {
                config.channels.get_mut(index).map(|channel| &mut channel.info)
            }
            HardwareConfig::SerialStream(config) => {
                config.channels.get_mut(index).map(|channel| &mut channel.info)
            }
            HardwareConfig::None => None,
        }
    }

    /// The channels this hardware provides, in the order it reads them.
    ///
    /// The hardware config is the single definition of which channels exist.
    /// A device mirrors this list rather than keeping its own, so the two
    /// cannot disagree about what is being measured or in what order.
    pub fn channel_infos(&self) -> Vec<ChannelInfo> {
        match self {
            HardwareConfig::MockHardware(config) => {
                config.channels.iter().map(|channel| channel.info.clone()).collect()
            }
            HardwareConfig::PicoHrdl(config) => {
                config.channels.iter().map(|channel| channel.info.clone()).collect()
            }
            HardwareConfig::NiDaqmx(config) => {
                config.channels.iter().map(|channel| channel.info.clone()).collect()
            }
            HardwareConfig::SerialStream(config) => {
                config.channels.iter().map(|channel| channel.info.clone()).collect()
            }
            HardwareConfig::None => vec![],
        }
    }
}

/// A piece of hardware as it exists while running, owning whatever resources
/// it needs. Deliberately not Serialize and not Clone.
pub enum Hardware {
    MockHardware(MockHardware),
    PicoHrdl(PicoHrdl),
    NiDaqmx(NiDaqmx),
    SerialStream(SerialStream),
    None,
}

impl Hardware {
    /// Build the running device described by a config.
    pub fn from_config(config: HardwareConfig) -> Result<Hardware> {
        Ok(match config {
            HardwareConfig::MockHardware(config) => Hardware::MockHardware(MockHardware::from_config(config)?),
            HardwareConfig::PicoHrdl(config) => Hardware::PicoHrdl(PicoHrdl::from_config(config)?),
            HardwareConfig::NiDaqmx(config) => Hardware::NiDaqmx(NiDaqmx::from_config(config)?),
            HardwareConfig::SerialStream(config) => Hardware::SerialStream(SerialStream::from_config(config)?),
            HardwareConfig::None => Hardware::None,
        })
    }

    /// Describe this device so it can be saved. Each backend hands back the
    /// config it is already holding, so the two cannot drift apart.
    pub fn config(&self) -> HardwareConfig {
        match self {
            Hardware::MockHardware(device) => HardwareConfig::MockHardware(device.config()),
            Hardware::PicoHrdl(device) => HardwareConfig::PicoHrdl(device.config()),
            Hardware::NiDaqmx(device) => HardwareConfig::NiDaqmx(device.config()),
            Hardware::SerialStream(device) => HardwareConfig::SerialStream(device.config()),
            Hardware::None => HardwareConfig::None,
        }
    }
}
impl HardwareDataAcquisition for Hardware {
    fn read(&mut self) -> Result<Vec<Vec<DataPoint>>> {
        match self {
            Hardware::MockHardware(device) => device.read(),
            Hardware::PicoHrdl(device) => device.read(),
            Hardware::NiDaqmx(device) => device.read(),
            Hardware::SerialStream(device) => device.read(),
            Hardware::None => Err(Error::NoHardware)
        }
    }

    fn said(&mut self) -> Vec<String> {
        match self {
            Hardware::SerialStream(device) => device.said(),
            // Nothing else has anything to say. Listed rather than caught by a
            // wildcard so that adding a talkative device makes this fail to
            // compile rather than quietly go quiet.
            Hardware::MockHardware(_)
            | Hardware::PicoHrdl(_)
            | Hardware::NiDaqmx(_)
            | Hardware::None => Vec::new(),
        }
    }

    fn concern(&self) -> Option<String> {
        match self {
            Hardware::SerialStream(device) => device.concern(),
            // Spelled out rather than left to a wildcard, so adding a backend
            // that has something to say is a compile error rather than a
            // silence nobody notices.
            Hardware::MockHardware(_)
            | Hardware::PicoHrdl(_)
            | Hardware::NiDaqmx(_)
            | Hardware::None => None,
        }
    }
}
impl DeviceInterface for Hardware {
    fn connect(&mut self) -> Result<()> {
        match self {
            Hardware::MockHardware(device) => device.connect()?,
            Hardware::PicoHrdl(device) => device.connect()?,
            Hardware::NiDaqmx(device) => device.connect()?,
            Hardware::SerialStream(device) => device.connect()?,
            Hardware::None => return Err(Error::NoHardware)
        }
        Ok(())
    }
}
