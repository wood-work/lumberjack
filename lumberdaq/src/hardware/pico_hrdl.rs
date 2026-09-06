use crate::channel::ChannelInfo;
use crate::datapoint::DataPoint;
use crate::device::{ Device, DeviceInterface };
use crate::hardware::{ Hardware, HardwareDataAcquisition };
use crate::{ Error, Result };
use picolog::hrdl::{
    can_be_differential, counts_to_volts, differential_partner, ConversionTime, Hrdl,
    MAX_CHANNEL, MIN_CHANNEL,
};
// Re-exported because a channel's range is part of this backend's public
// configuration: anything editing one has to be able to name the type, and
// making it reach for picolog to do so would spread the dependency further
// than the one backend that needs it.
pub use picolog::hrdl::VoltageRange;
pub use picolog::hrdl::{MAX_CHANNEL as HIGHEST_CHANNEL, MIN_CHANNEL as LOWEST_CHANNEL};
use serde::{ Deserialize, Serialize };
use std::time::Duration;

/// A Pico Technology ADC-20 or ADC-24 high resolution logger.
///
/// Channels are read one at a time, each waiting a full conversion. That is
/// slow by the standards of the other backends here - 60ms is the fastest the
/// hardware offers, and eight channels is most of half a second - but it is
/// only slow on this device's own thread, so nothing else waits for it.
///
/// Unlike the serial backend, the channels of one scan do *not* share a
/// timestamp. They genuinely are not simultaneous: the last channel of an
/// eight channel scan is read most of a second after the first, and pretending
/// otherwise would put a lie in the results.

/// How samples are taken from the unit.
///
/// The difference is who decides when a sample happens. Polled means we ask and
/// the unit converts, one value per channel per ask. Streaming means the unit
/// keeps its own schedule and we drain what it produced.
///
/// Streaming is better wherever it fits, for two measured reasons. Every scan
/// carries the time the *unit* took it, so lateness in getting round to draining
/// does not become timestamp error. And there is no per channel switching cost:
/// polled reads of a second channel measured 114ms against a 60ms conversion,
/// because the driver re-selects the input each time, where a streaming unit
/// sweeps them itself.
///
/// Polled is kept because it needs no interval agreed up front, which makes it
/// the simpler thing to reach for when checking a rig.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Acquisition {
    /// Ask for one value per channel, waiting for each conversion.
    Polled,
    /// Let the unit scan on its own schedule and take what it produced.
    Streaming {
        /// How often the unit takes a complete scan of every channel.
        ///
        /// Not the same as the device's `sample_interval_ms`, which becomes how
        /// often we *drain*. Draining slower than this simply returns more
        /// scans at a time, which is the point.
        sample_interval_ms: u64,
        /// How many scans the driver buffers behind us.
        ///
        /// What bounds how long a reader can be away before samples are lost,
        /// so it wants to hold comfortably more than one drain's worth.
        #[serde(default = "default_buffer_scans")]
        buffer_scans: u32,
    },
}

impl Default for Acquisition {
    /// Polled, so a config written before this setting existed keeps behaving
    /// exactly as it did.
    fn default() -> Acquisition {
        Acquisition::Polled
    }
}

fn default_buffer_scans() -> u32 {
    1000
}

/// Everything needed to describe an ADC-20/24 in a config file.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PicoHrdlConfig {
    // Every field below either has a Default of its own or is a String or a
    // Vec, so a new unit can be described by `PicoHrdlConfig::default()` and
    // then have its channels added.
    /// How long the converter integrates for, per channel. Longer is quieter
    /// and slower.
    #[serde(default)]
    pub conversion_time: ConversionTime,
    /// Reject 60Hz mains hum rather than 50Hz.
    #[serde(default)]
    pub mains_sixty_hertz: bool,
    #[serde(default)]
    pub acquisition: Acquisition,
    pub channels: Vec<PicoHrdlChannel>,
}

/// One channel: what it is, and which input it reads.
#[derive(Serialize, Deserialize, Clone)]
pub struct PicoHrdlChannel {
    #[serde(flatten)]
    pub info: ChannelInfo,
    /// Analog input number, 1 to 16.
    pub channel: u16,
    /// Full scale range for this input.
    pub range: VoltageRange,
    /// Single ended measures against ground. Differential pairs this channel
    /// with the one above it, so only odd numbered channels can be used.
    #[serde(default = "default_single_ended")]
    pub single_ended: bool,
}

fn default_single_ended() -> bool {
    true
}

/// The running device: its settings, and the open unit once connected.
pub struct PicoHrdl {
    config: PicoHrdlConfig,
    unit: Option<Hrdl>,
    /// Full scale counts per channel, in config order, read from the unit at
    /// connect. Needed to turn counts into volts, and it varies with the
    /// conversion time, so it cannot be a constant.
    full_scale_counts: Vec<i32>,
    /// Wall clock at the moment streaming began.
    ///
    /// The unit timestamps its scans from the start of the run, so this is what
    /// turns those into real times. Taken once, so every scan in a run is
    /// placed against the same origin rather than against whenever we drained.
    stream_started: Option<chrono::DateTime<chrono::Utc>>,
}

impl PicoHrdl {
    pub fn new() -> Result<PicoHrdl> {
        PicoHrdl::from_config(PicoHrdlConfig {
            conversion_time: ConversionTime::default(),
            mains_sixty_hertz: false,
            acquisition: Acquisition::default(),
            channels: vec![],
        })
    }

    pub fn from_config(mut config: PicoHrdlConfig) -> Result<PicoHrdl> {
        check_channels(&config)?;
        // An analog input measures volts and nothing else, so a channel
        // need not say so. One that claims otherwise without a scale to
        // make it true is refused here rather than mislabelling a run.
        for channel in config.channels.iter_mut() {
            let named = channel.info.name.clone();
            channel.info.settle_voltage_unit(&named)?;
        }
        Ok(PicoHrdl {
            config: config,
            unit: None,
            full_scale_counts: vec![],
            stream_started: None,
        })
    }

    pub fn config(&self) -> PicoHrdlConfig {
        self.config.clone()
    }

    pub fn add_channel(&mut self, channel: PicoHrdlChannel) {
        self.config.channels.push(channel);
    }

    /// A lower bound on how fast a full scan can be taken.
    ///
    /// Every channel waits its own conversion, so this is the conversion time
    /// multiplied by the channel count.
    ///
    /// Known to be optimistic. Measured against a real ADC-20 with two channels
    /// at a 60ms conversion, the second channel cost 114ms rather than 60, so
    /// switching input appears to cost about a conversion on top of the
    /// conversion itself. Treat this as a floor that a real scan will exceed,
    /// not as a prediction.
    pub fn minimum_interval(&self) -> Duration {
        Duration::from_millis(
            self.config.conversion_time.millis() * self.config.channels.len().max(1) as u64,
        )
    }
}

/// Check a channel list against how the hardware pairs its inputs.
///
/// Done when the setup is built rather than at connect, so a rig that cannot
/// work is refused before anything is plugged in or recorded. The driver would
/// reject most of this too, but only once a run was being started, and with
/// less to say about why.
/// What the attached unit says it is.
pub struct Unit {
    /// The variant it reports: `20` for an ADC-20, `24` for an ADC-24.
    pub variant: String,
    /// How many analog inputs that variant has, where the variant is one we
    /// know. An ADC-20 has eight where an ADC-24 has sixteen.
    ///
    /// Separate from the variant so that a unit we do not recognise can still
    /// say what it calls itself, which is the one case where being told the
    /// model matters most.
    pub inputs: Option<u16>,
}

/// Ask the attached unit what it is.
///
/// The unit's own answer rather than anything the config claims, which is the
/// point: a project written for one model opened with the other should say so
/// rather than looking right until a channel is out of range. How many inputs
/// it has decides both which channels can be chosen and which one a
/// differential pair takes.
///
/// `None` when there is none attached, no driver, or it is already open — a
/// unit being read by a run in progress cannot also be asked.
///
/// One function rather than two because opening the unit is the expensive part
/// by a wide margin: it loads the driver and initialises the unit over USB,
/// which takes long enough to be felt. Both answers come from the same string
/// the unit returns, so asking twice meant paying that twice for one question.
pub fn identify() -> Result<Unit> {
    // A `Result` rather than an `Option`, so that "there is no driver
    // installed" and "there is no unit plugged in" can be told apart by
    // whoever asked. Both look the same from here and mean quite different
    // things to somebody trying to use the hardware.
    let unit = Hrdl::open()?;

    let variant = unit.info(picolog::hrdl::Info::Variant)?;

    // Cheap next to the open: the unit is already there to be asked.
    let inputs = unit.channel_count().ok();

    Ok(Unit { variant, inputs })
}

pub(crate) fn check_channels(config: &PicoHrdlConfig) -> Result<()> {
    let mut configured: Vec<u16> = Vec::new();
    for channel in config.channels.iter() {
        // Guarded before the arithmetic below, which would otherwise wrap.
        if channel.channel < MIN_CHANNEL || channel.channel > MAX_CHANNEL {
            return Err(Error::ChannelOutOfRange {
                channel: channel.channel,
                lowest: MIN_CHANNEL,
                highest: MAX_CHANNEL,
            });
        }
        if configured.contains(&channel.channel) {
            return Err(Error::DuplicateChannelNumber { channel: channel.channel });
        }
        configured.push(channel.channel);
    }

    for channel in config.channels.iter() {
        if channel.single_ended {
            continue;
        }
        // A differential input measures between a channel and the one above
        // it, so the first of the pair is always odd.
        if !can_be_differential(channel.channel) {
            return Err(Error::DifferentialNeedsOddChannel {
                channel: channel.channel,
                // The pair that actually includes this channel starts below it.
                pair_starts_at: channel.channel - 1,
            });
        }
        // The other half is consumed by the pair and cannot be used again.
        let partner = differential_partner(channel.channel);
        if configured.contains(&partner) {
            return Err(Error::DifferentialPartnerInUse {
                primary: channel.channel,
                secondary: partner,
            });
        }
    }
    Ok(())
}

impl DeviceInterface for PicoHrdl {
    fn connect(&mut self) -> Result<()> {
        let mut unit = Hrdl::open()?;
        unit.set_mains_rejection(self.config.mains_sixty_hertz)?;

        // How many inputs this unit has depends on which model it is, and only
        // the unit knows. An ADC-20 has eight where an ADC-24 has sixteen, so a
        // config written for one refuses clearly on the other.
        let available = unit.channel_count()?;
        let variant = unit.info(picolog::hrdl::Info::Variant).unwrap_or_default();
        for channel in self.config.channels.iter() {
            let highest = if channel.single_ended {
                channel.channel
            } else {
                differential_partner(channel.channel)
            };
            if highest > available {
                return Err(Error::ChannelNotOnThisUnit {
                    channel: channel.channel,
                    variant: variant.clone(),
                    available: available,
                });
            }
        }

        let mut full_scale_counts = Vec::with_capacity(self.config.channels.len());
        for channel in self.config.channels.iter() {
            unit.enable_channel(channel.channel, channel.range, channel.single_ended)?;
            // Ask the unit rather than assuming: the count range depends on the
            // conversion time, so it is a property of this configuration.
            let (_minimum, maximum) = unit.count_range(channel.channel)?;
            full_scale_counts.push(maximum);
        }

        // Streaming has to be told its schedule before it starts. The driver
        // refuses an interval too short for the channels to convert in, which
        // arrives here as ConversionTimeTooSlow rather than as a device that
        // quietly runs late.
        if let Acquisition::Streaming { sample_interval_ms, buffer_scans } = self.config.acquisition {
            unit.set_interval(Duration::from_millis(sample_interval_ms), self.config.conversion_time)?;
            unit.start_streaming(buffer_scans)?;
            // Taken after the call that starts the clock, so a scan at unit
            // time zero maps to roughly now rather than to before the run.
            self.stream_started = Some(chrono::Utc::now());
        }

        self.full_scale_counts = full_scale_counts;
        self.unit = Some(unit);
        Ok(())
    }
}

impl HardwareDataAcquisition for PicoHrdl {
    fn read(&mut self) -> Result<Vec<Vec<DataPoint>>> {
        let unit = match &self.unit {
            Some(unit) => unit,
            None => return Err(Error::NoHardware),
        };

        match self.config.acquisition {
            Acquisition::Polled => {
                let mut readings: Vec<Vec<DataPoint>> =
                    Vec::with_capacity(self.config.channels.len());
                for (channel, full_scale) in
                    self.config.channels.iter().zip(self.full_scale_counts.iter())
                {
                    let reading = unit.read_single(
                        channel.channel,
                        channel.range,
                        self.config.conversion_time,
                        channel.single_ended,
                    )?;
                    readings.push(vec![DataPoint {
                        // Stamped per channel, after its own conversion,
                        // because that is when this value was measured.
                        datetime: chrono::Utc::now(),
                        value: counts_to_volts(reading.counts, *full_scale, channel.range),
                    }]);
                }
                Ok(readings)
            }
            Acquisition::Streaming { buffer_scans, .. } => {
                let started = match self.stream_started {
                    Some(started) => started,
                    None => return Err(Error::NoHardware),
                };
                let channel_count = self.config.channels.len();
                let scans = unit.take_scans(channel_count, buffer_scans as usize)?;

                // One vec per channel whether or not anything arrived, so the
                // shape matches the channel list even on an empty drain.
                let mut readings: Vec<Vec<DataPoint>> = vec![Vec::new(); channel_count];
                for scan in scans.iter() {
                    // The unit's own time, not ours. A late drain moves when we
                    // hear about a scan, never when it says it happened.
                    let datetime = started
                        + chrono::Duration::milliseconds(scan.since_start.as_millis() as i64);
                    for (index, counts) in scan.counts.iter().enumerate() {
                        let channel = &self.config.channels[index];
                        readings[index].push(DataPoint {
                            datetime: datetime,
                            value: counts_to_volts(
                                *counts,
                                self.full_scale_counts[index],
                                channel.range,
                            ),
                        });
                    }
                }
                Ok(readings)
            }
        }
    }
}

pub fn create_device(name: String) -> Result<Device> {
    let hardware = PicoHrdl::new()?;
    Ok(Device::new(name, Hardware::PicoHrdl(hardware)))
}

pub fn add_channel(
    device: &mut Device,
    name: String,
    channel: u16,
    range: VoltageRange,
    single_ended: bool,
) -> Result<()> {
    match &mut device.hardware {
        Hardware::PicoHrdl(hardware) => {
            hardware.add_channel(PicoHrdlChannel {
                info: ChannelInfo {
                    name: name,
                    unit: "V".to_string(),
                scale: None,
                },
                channel: channel,
                range: range,
                single_ended: single_ended,
            });
        }
        _ => {
            return Err(Error::WrongHardwareType {
                expected: "Pico high resolution logger".to_string(),
            })
        }
    }
    device.rebuild_channels()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(channels: usize, conversion: ConversionTime) -> PicoHrdlConfig {
        PicoHrdlConfig {
            conversion_time: conversion,
            mains_sixty_hertz: false,
            acquisition: Acquisition::Polled,
            channels: (0..channels)
                .map(|index| PicoHrdlChannel {
                    info: ChannelInfo {
                        name: format!("Channel {}", index + 1),
                        unit: "V".to_string(),
                    scale: None,
                    },
                    channel: index as u16 + 1,
                    range: VoltageRange::MilliVolts2500,
                    single_ended: true,
                })
                .collect(),
        }
    }

    /// A single channel read on a real ADC-20 cost 57-58ms against a 60ms
    /// conversion, so for one channel this is accurate. With more it is a floor
    /// rather than an estimate: see the note on minimum_interval.
    #[test]
    fn the_scan_floor_is_the_conversion_time_per_channel() {
        let device = PicoHrdl::from_config(config_with(8, ConversionTime::Ms60)).unwrap();
        assert_eq!(device.minimum_interval(), Duration::from_millis(480));

        let slow = PicoHrdl::from_config(config_with(2, ConversionTime::Ms660)).unwrap();
        assert_eq!(slow.minimum_interval(), Duration::from_millis(1320));
    }

    /// A device with no channels still takes some time to talk to, so the floor
    /// must not be zero.
    #[test]
    fn a_device_with_no_channels_has_a_floor() {
        let empty = PicoHrdl::from_config(config_with(0, ConversionTime::Ms60)).unwrap();
        assert_eq!(empty.minimum_interval(), Duration::from_millis(60));
    }

    /// Reading before connecting must say so rather than reaching for a unit
    /// that is not there.
    #[test]
    fn reading_before_connecting_is_rejected() {
        let mut device = PicoHrdl::from_config(config_with(1, ConversionTime::Ms60)).unwrap();
        assert!(matches!(device.read(), Err(Error::NoHardware)));
    }

    /// Settings round trip through a config file, since that is how a rig is
    /// actually described.
    #[test]
    fn settings_survive_a_config_round_trip() {
        let json = r#"{
            "description": "ADC-20",
            "conversion_time": "ms180",
            "mains_sixty_hertz": true,
            "channels": [
                {
                    "name": "Load cell",
                    "unit": "V",
                    "description": "-",
                    "channel": 3,
                    "range": "milli_volts625",
                    "single_ended": false
                }
            ]
        }"#;
        let config: PicoHrdlConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.conversion_time, ConversionTime::Ms180);
        assert!(config.mains_sixty_hertz);
        assert_eq!(config.channels[0].channel, 3);
        assert_eq!(config.channels[0].range, VoltageRange::MilliVolts625);
        assert!(!config.channels[0].single_ended);
    }

    /// A config written before acquisition existed must keep polling, which is
    /// what it was doing.
    #[test]
    fn acquisition_defaults_to_polled() {
        let json = r#"{
            "description": "ADC-20",
            "channels": []
        }"#;
        let config: PicoHrdlConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.acquisition, Acquisition::Polled);
    }

    /// The mode carries its own settings, so nothing has fields that only
    /// apply sometimes.
    #[test]
    fn streaming_carries_its_own_settings() {
        let json = r#"{
            "description": "ADC-20",
            "acquisition": { "mode": "streaming", "sample_interval_ms": 200, "buffer_scans": 500 },
            "channels": []
        }"#;
        let config: PicoHrdlConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.acquisition,
            Acquisition::Streaming { sample_interval_ms: 200, buffer_scans: 500 }
        );
    }

    #[test]
    fn a_streaming_config_need_not_size_the_buffer() {
        let json = r#"{
            "description": "ADC-20",
            "acquisition": { "mode": "streaming", "sample_interval_ms": 100 },
            "channels": []
        }"#;
        let config: PicoHrdlConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.acquisition,
            Acquisition::Streaming { sample_interval_ms: 100, buffer_scans: default_buffer_scans() }
        );
    }

    /// Streaming before connecting has no origin to place scans against, so it
    /// must refuse rather than invent one.
    #[test]
    fn streaming_before_connecting_is_rejected() {
        let mut config = config_with(1, ConversionTime::Ms60);
        config.acquisition = Acquisition::Streaming { sample_interval_ms: 200, buffer_scans: 100 };
        let mut device = PicoHrdl::from_config(config).unwrap();
        assert!(matches!(device.read(), Err(Error::NoHardware)));
    }

    fn differential(channels: &[(u16, bool)]) -> PicoHrdlConfig {
        PicoHrdlConfig {
            conversion_time: ConversionTime::Ms60,
            mains_sixty_hertz: false,
            acquisition: Acquisition::Polled,
            channels: channels
                .iter()
                .map(|(number, single_ended)| PicoHrdlChannel {
                    info: ChannelInfo {
                        name: format!("Channel {}", number),
                        unit: "V".to_string(),
                    scale: None,
                    },
                    channel: *number,
                    range: VoltageRange::MilliVolts2500,
                    single_ended: *single_ended,
                })
                .collect(),
        }
    }

    /// A differential input measures between a channel and the one above it,
    /// so the first of the pair is odd. An ADC-20 has eight inputs and so four
    /// differential pairs: 1-2, 3-4, 5-6, 7-8.
    #[test]
    fn odd_channels_can_be_differential() {
        for channel in [1u16, 3, 5, 7] {
            assert!(
                PicoHrdl::from_config(differential(&[(channel, false)])).is_ok(),
                "channel {} should be a valid differential input",
                channel
            );
        }
    }

    /// Asking for a differential on an even channel is a mistake worth catching
    /// before a run: the driver would refuse it, but only at connect.
    #[test]
    fn an_even_channel_cannot_lead_a_differential_pair() {
        let error = PicoHrdl::from_config(differential(&[(2, false)])).err().unwrap();
        assert!(matches!(error, Error::DifferentialNeedsOddChannel { .. }));
        // The pair that includes channel 2 is 1-2, not 2-3.
        assert!(
            error.to_string().contains("between 1 and 2"),
            "misleading suggestion: {}",
            error
        );
    }

    /// The even half of a pair is consumed by it, so it cannot be configured
    /// separately. Left unchecked this would silently measure something else.
    #[test]
    fn the_other_half_of_a_pair_cannot_be_used_again() {
        let error = PicoHrdl::from_config(differential(&[(1, false), (2, true)]))
            .err()
            .unwrap();
        assert!(matches!(error, Error::DifferentialPartnerInUse { primary: 1, secondary: 2 }));
    }

    /// Two differential pairs side by side are fine: 1-2 and 3-4 do not clash.
    #[test]
    fn adjacent_differential_pairs_do_not_clash() {
        assert!(PicoHrdl::from_config(differential(&[(1, false), (3, false)])).is_ok());
    }

    /// Channel zero is the digital block, and subtracting one from it would
    /// wrap round to sixty five thousand.
    #[test]
    fn channel_zero_is_refused_rather_than_wrapping() {
        let error = PicoHrdl::from_config(differential(&[(0, false)])).err().unwrap();
        assert!(matches!(error, Error::ChannelOutOfRange { .. }));
    }

    #[test]
    fn the_same_channel_twice_is_refused() {
        let error = PicoHrdl::from_config(differential(&[(1, true), (1, true)]))
            .err()
            .unwrap();
        assert!(matches!(error, Error::DuplicateChannelNumber { channel: 1 }));
    }

    /// Most inputs are single ended, so a config should not have to say so.
    #[test]
    fn single_ended_is_the_default() {
        let json = r#"{
            "description": "ADC-20",
            "channels": [
                { "name": "A", "unit": "V", "description": "-",
                  "channel": 1, "range": "milli_volts2500" }
            ]
        }"#;
        let config: PicoHrdlConfig = serde_json::from_str(json).unwrap();
        assert!(config.channels[0].single_ended);
        assert_eq!(config.conversion_time, ConversionTime::Ms60);
    }
}
