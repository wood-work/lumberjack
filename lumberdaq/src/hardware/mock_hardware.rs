use crate::{ Error, Result };
use crate::datapoint::DataPoint;
use crate::channel::ChannelInfo;
use crate::device::{ Device, DeviceInterface };
use crate::hardware::{HardwareDataAcquisition, Hardware };
use chrono::{ DateTime, Utc };
use serde::{Deserialize, Serialize};
use rand::random;

/// A device with nothing behind it, for running with no hardware attached.
///
/// It can stream as well as poll, which is the point of it: the streaming path
/// through the rest of the system - batches with many samples, timestamps from
/// the device rather than from us, draining slower than sampling - can be
/// exercised on any machine, with no instrument plugged in.

/// The most samples one drain will produce.
///
/// A drain that arrives very late, or an interval rounded down to nothing,
/// would otherwise generate without bound. Hitting this drops the oldest scans,
/// which is what a real device with a full buffer does.
const MAX_SCANS_PER_DRAIN: usize = 100_000;

/// How samples are taken. The same choice the Pico backend offers, so the
/// streaming path can be exercised without an instrument.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Acquisition {
    /// One sample per channel, generated when asked.
    Polled,
    /// Samples on a fixed schedule, whether or not anyone is draining.
    ///
    /// Timestamps are computed from the schedule rather than from when the
    /// drain happened, so they come out exactly evenly spaced the way a real
    /// streaming instrument's do.
    Streaming { sample_interval_ms: u64 },
}

impl Default for Acquisition {
    fn default() -> Acquisition {
        Acquisition::Polled
    }
}

/// Everything needed to describe a mock device in a config file.
#[derive(Serialize, Deserialize, Clone)]
pub struct MockHardwareConfig {
    #[serde(default)]
    pub acquisition: Acquisition,
    pub channels: Vec<MockHardwareChannel>,
}

/// One channel: what it is, and what generates its values.
///
/// Description and binding live together so they cannot be listed in different
/// orders, which is what used to decide silently whose data went where.
#[derive(Serialize, Deserialize, Clone)]
pub struct MockHardwareChannel {
    #[serde(flatten)]
    pub info: ChannelInfo,
    pub input: MockHardwareInput,
}

impl Default for MockHardwareConfig {
    fn default() -> MockHardwareConfig {
        MockHardwareConfig {
            acquisition: Acquisition::default(),
            channels: vec![],
        }
    }
}

/// The running device.
///
/// Owns no hardware resource, but it does own a clock: time based inputs and
/// the streaming schedule are both measured from the moment it connected.
pub struct MockHardware {
    config: MockHardwareConfig,
    /// When this device connected. The origin for time based inputs, so a sine
    /// starts at zero rather than at some arbitrary phase.
    started: Option<DateTime<Utc>>,
    /// The next scan the schedule owes, when streaming.
    ///
    /// Advanced by exactly one interval per scan produced rather than being
    /// reset to the current time, so the schedule does not drift and a late
    /// drain catches up instead of losing samples.
    next_scan: Option<DateTime<Utc>>,
}

impl MockHardware {
    pub fn new() -> Result<MockHardware> {
        MockHardware::from_config(MockHardwareConfig::default())
    }

    pub fn from_config(config: MockHardwareConfig) -> Result<MockHardware> {
        Ok(MockHardware { config: config, started: None, next_scan: None })
    }

    pub fn config(&self) -> MockHardwareConfig {
        self.config.clone()
    }

    pub fn add_channel(&mut self, channel: MockHardwareChannel) {
        self.config.channels.push(channel);
    }

    /// Generate everything the schedule owes up to `now`.
    ///
    /// Split out so it can be tested against a clock we control, rather than
    /// against whatever the machine happened to do.
    fn scans_until(&mut self, now: DateTime<Utc>, sample_interval_ms: u64) -> Vec<Vec<DataPoint>> {
        let channel_count = self.config.channels.len();
        let mut readings: Vec<Vec<DataPoint>> = vec![Vec::new(); channel_count];

        let started = match self.started {
            Some(started) => started,
            None => return readings,
        };
        // An interval of zero would owe infinitely many samples, so treat it as
        // the fastest the millisecond schedule can express.
        let interval = chrono::Duration::milliseconds(sample_interval_ms.max(1) as i64);

        let mut scan_time = self.next_scan.unwrap_or(started);
        let mut produced = 0usize;
        while scan_time <= now && produced < MAX_SCANS_PER_DRAIN {
            let elapsed = (scan_time - started).num_milliseconds() as f64 / 1000.0;
            for (index, channel) in self.config.channels.iter().enumerate() {
                readings[index].push(DataPoint {
                    // The schedule's time, not the drain's. This is what makes
                    // the spacing exact no matter when we got round to asking.
                    datetime: scan_time,
                    value: channel.input.value_at(elapsed),
                });
            }
            scan_time = scan_time + interval;
            produced += 1;
        }

        // If the cap was hit there is still a backlog; skip it rather than
        // spending every future drain catching up on samples nobody wanted.
        if produced >= MAX_SCANS_PER_DRAIN {
            scan_time = now + interval;
        }
        self.next_scan = Some(scan_time);
        readings
    }
}

impl DeviceInterface for MockHardware {
    fn connect(&mut self) -> Result<()> {
        let now = Utc::now();
        self.started = Some(now);
        self.next_scan = Some(now);
        Ok(())
    }
}

impl HardwareDataAcquisition for MockHardware {
    fn read(&mut self) -> Result<Vec<Vec<DataPoint>>> {
        let started = match self.started {
            Some(started) => started,
            None => return Err(Error::NoHardware),
        };

        match self.config.acquisition {
            Acquisition::Polled => {
                let now = Utc::now();
                let elapsed = (now - started).num_milliseconds() as f64 / 1000.0;
                let mut readings: Vec<Vec<DataPoint>> =
                    Vec::with_capacity(self.config.channels.len());
                for channel in self.config.channels.iter() {
                    readings.push(vec![DataPoint {
                        datetime: now,
                        value: channel.input.value_at(elapsed),
                    }]);
                }
                Ok(readings)
            }
            Acquisition::Streaming { sample_interval_ms } => {
                Ok(self.scans_until(Utc::now(), sample_interval_ms))
            }
        }
    }
}

/// What generates a channel's values.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum MockHardwareInput {
    Random,
    Constant(f64),
    /// A sine of amplitude 1, starting at zero when the device connects.
    ///
    /// Useful for more than looking like a signal: because the value is a
    /// function of the timestamp, a recording can be checked against the wave
    /// it should be. That tests the timestamps and the values together, which
    /// a random channel cannot.
    Sine { frequency_hz: f64 },
}

impl MockHardwareInput {
    /// This input's value at a given time since the device connected.
    ///
    /// Taking the time rather than reading a clock is what lets the streaming
    /// path generate a backlog: samples that should have happened while nobody
    /// was draining still get the values they would have had.
    fn value_at(&self, elapsed_seconds: f64) -> f64 {
        match self {
            MockHardwareInput::Random => random(),
            MockHardwareInput::Constant(value) => *value,
            MockHardwareInput::Sine { frequency_hz } => {
                (2.0 * std::f64::consts::PI * frequency_hz * elapsed_seconds).sin()
            }
        }
    }
}

pub fn create_device(name: String) -> Result<Device> {
    let hardware = MockHardware::new()?;
    Ok(Device::new(name, Hardware::MockHardware(hardware)))
}

fn add_mock_channel(
    device: &mut Device,
    info: ChannelInfo,
    input: MockHardwareInput,
) -> Result<()> {
    match &mut device.hardware {
        Hardware::MockHardware(hardware) => {
            hardware.add_channel(MockHardwareChannel { info: info, input: input });
        }
        _ => return Err(Error::WrongHardwareType { expected: "mock hardware".to_string() }),
    }
    // The hardware config is the definition; the device mirrors it.
    device.rebuild_channels()
}

pub fn add_channel_random(device: &mut Device, name: String) -> Result<()> {
    add_mock_channel(
        device,
        ChannelInfo {
            name: name,
            unit: "-".to_string(),
        scale: None,
        },
        MockHardwareInput::Random,
    )
}

pub fn add_channel_sine(device: &mut Device, name: String, frequency_hz: f64) -> Result<()> {
    add_mock_channel(
        device,
        ChannelInfo {
            name: name,
            unit: "-".to_string(),
        scale: None,
        },
        MockHardwareInput::Sine { frequency_hz: frequency_hz },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_device(frequency_hz: f64, acquisition: Acquisition) -> MockHardware {
        MockHardware::from_config(MockHardwareConfig {
            acquisition: acquisition,
            channels: vec![MockHardwareChannel {
                info: ChannelInfo {
                    name: "Sine".to_string(),
                    unit: "-".to_string(),
                scale: None,
                },
                input: MockHardwareInput::Sine { frequency_hz: frequency_hz },
            }],
        })
        .unwrap()
    }

    #[test]
    fn a_sine_traces_its_own_wave() {
        let input = MockHardwareInput::Sine { frequency_hz: 1.0 };
        assert!((input.value_at(0.0) - 0.0).abs() < 1e-12);
        assert!((input.value_at(0.25) - 1.0).abs() < 1e-12);
        assert!((input.value_at(0.5) - 0.0).abs() < 1e-12);
        assert!((input.value_at(0.75) + 1.0).abs() < 1e-12);
        assert!((input.value_at(1.0) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn frequency_sets_the_period() {
        let fast = MockHardwareInput::Sine { frequency_hz: 4.0 };
        // Four cycles a second means a quarter cycle every 62.5 ms.
        assert!((fast.value_at(0.0625) - 1.0).abs() < 1e-12);
        assert!((fast.value_at(0.25) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn amplitude_stays_within_one() {
        let input = MockHardwareInput::Sine { frequency_hz: 3.3 };
        for step in 0..1000 {
            let value = input.value_at(step as f64 / 100.0);
            assert!(value >= -1.0 && value <= 1.0);
        }
    }

    /// The point of mock streaming: samples land exactly on the schedule, not
    /// on whenever the drain happened.
    #[test]
    fn streaming_samples_are_evenly_spaced_however_late_the_drain() {
        let mut device = sine_device(1.0, Acquisition::Streaming { sample_interval_ms: 100 });
        let started = Utc::now();
        device.started = Some(started);
        device.next_scan = Some(started);

        // A drain 350ms in, then one 1000ms in. Uneven on purpose.
        let first = device.scans_until(started + chrono::Duration::milliseconds(350), 100);
        let second = device.scans_until(started + chrono::Duration::milliseconds(1000), 100);

        let times: Vec<i64> = first[0]
            .iter()
            .chain(second[0].iter())
            .map(|point| (point.datetime - started).num_milliseconds())
            .collect();
        assert_eq!(times, vec![0, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000]);
    }

    /// Values must match the wave at their own timestamp, including the ones
    /// generated for a period when nobody was draining.
    #[test]
    fn a_backlog_carries_the_values_it_should_have_had() {
        let mut device = sine_device(1.0, Acquisition::Streaming { sample_interval_ms: 250 });
        let started = Utc::now();
        device.started = Some(started);
        device.next_scan = Some(started);

        // One drain covering a whole cycle at once.
        let scans = device.scans_until(started + chrono::Duration::milliseconds(1000), 250);
        let values: Vec<f64> = scans[0].iter().map(|point| point.value).collect();
        assert_eq!(values.len(), 5);
        for (index, expected) in [0.0, 1.0, 0.0, -1.0, 0.0].iter().enumerate() {
            assert!(
                (values[index] - expected).abs() < 1e-9,
                "sample {} was {} not {}",
                index,
                values[index],
                expected
            );
        }
    }

    /// Nothing new since the last drain is normal, not an error.
    #[test]
    fn draining_twice_in_a_row_yields_nothing_the_second_time() {
        let mut device = sine_device(1.0, Acquisition::Streaming { sample_interval_ms: 100 });
        let started = Utc::now();
        device.started = Some(started);
        device.next_scan = Some(started);

        let at = started + chrono::Duration::milliseconds(250);
        assert_eq!(device.scans_until(at, 100)[0].len(), 3);
        assert_eq!(device.scans_until(at, 100)[0].len(), 0);
    }

    /// An interval of zero owes infinitely many samples; it must not hang.
    #[test]
    fn a_zero_interval_does_not_generate_without_end() {
        let mut device = sine_device(1.0, Acquisition::Streaming { sample_interval_ms: 0 });
        let started = Utc::now();
        device.started = Some(started);
        device.next_scan = Some(started);
        let scans = device.scans_until(started + chrono::Duration::milliseconds(50), 0);
        assert_eq!(scans[0].len(), 51);
    }

    #[test]
    fn reading_before_connecting_is_rejected() {
        let mut device = sine_device(1.0, Acquisition::Polled);
        assert!(matches!(device.read(), Err(Error::NoHardware)));
    }

    /// A config written before these settings existed must keep working.
    #[test]
    fn older_configs_still_load_and_still_poll() {
        let json = r#"{
            "description": "Mock",
            "channels": [
                { "name": "Random 1", "unit": "-", "description": "-", "input": "Random" }
            ]
        }"#;
        let config: MockHardwareConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.acquisition, Acquisition::Polled);
        assert_eq!(config.channels[0].input, MockHardwareInput::Random);
    }

    #[test]
    fn a_sine_channel_round_trips_through_a_config() {
        let json = r#"{
            "description": "Mock",
            "acquisition": { "mode": "streaming", "sample_interval_ms": 20 },
            "channels": [
                { "name": "Sine", "unit": "-", "description": "-",
                  "input": { "Sine": { "frequency_hz": 2.5 } } }
            ]
        }"#;
        let config: MockHardwareConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.acquisition, Acquisition::Streaming { sample_interval_ms: 20 });
        assert_eq!(
            config.channels[0].input,
            MockHardwareInput::Sine { frequency_hz: 2.5 }
        );
    }
}
