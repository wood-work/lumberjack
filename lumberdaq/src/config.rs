use crate::calculated::{ CalculatedChannel, CalculatedDevice, ChannelRef };
use crate::daq::DaqInfo;
use crate::device::DeviceInfo;
use crate::hardware::{ HardwareConfig, SampleRate };
use serde::{ Deserialize, Serialize };
use std::collections::{ BTreeMap, BTreeSet };

/// The description of a measurement setup: which devices, which channels,
/// which ports. Enough to rebuild the whole thing, and nothing else.
///
/// These types are the file format rather than a second object model. Nothing
/// holds one while running: `Daq::config()` builds one on demand to save, and
/// `Daq::from_config()` consumes one to build the running system. That is why
/// `DeviceInfo` appearing here and in `Device` is not two copies of the same
/// state - at runtime only the `Device` exists.

/// Note there is no channel list here. Channels are defined inside the hardware
/// config, alongside the binding that says where each one's data comes from.
/// Keeping them apart meant two lists matched by position, which a hand edited
/// config could silently put out of order.
#[derive(Serialize, Deserialize, Clone)]
pub struct DeviceConfig {
    pub info: DeviceInfo,
    /// How often to collect from this device, in milliseconds.
    ///
    /// A collection rate, not a sample rate. For a polled device the two are
    /// the same, because a read takes a sample. For a device that samples on
    /// its own schedule this only decides how often the results are gathered
    /// up for saving; how fast it samples is the hardware's setting, and the
    /// timestamps are the same whatever this is.
    ///
    /// Per device rather than per system: a thermocouple logger sampling once a
    /// second and a serial rig at 50Hz belong in the same test, and each gets
    /// its own thread so neither waits for the other.
    #[serde(default = "default_read_interval_ms", alias = "sample_interval_ms")]
    pub read_interval_ms: u64,
    /// Whether this device is used at all.
    ///
    /// A device that is switched off is not connected to, not read and not
    /// reported on. It keeps its place in the setup and every one of its
    /// settings, so switching it back on is one click rather than typing a
    /// port and a channel list again.
    ///
    /// Defaulted to on and left out of the file when it is, as a channel's is.
    #[serde(default = "crate::channel::on", skip_serializing_if = "crate::channel::is_on")]
    pub enabled: bool,
    pub hardware: HardwareConfig,
}

/// 10Hz, matching the serial device this was built against. Also the fallback
/// for configs written before the setting existed.
pub fn default_read_interval_ms() -> u64 {
    100
}

/// Two devices pointed at one piece of hardware.
///
/// Worth finding before a run rather than during one. What happens otherwise
/// is that the first device opens the port and the second is refused by the
/// operating system, which reports it as access being denied - a message that
/// sends somebody to look at a cable when the fault is in the setup.
#[derive(Debug, Clone, PartialEq)]
pub struct AddressClash {
    /// The device that cannot have it.
    pub device: String,
    /// The port, or unit name, they are both pointed at.
    pub address: String,
    /// The device that got there first, by its place in the setup.
    pub taken_by: String,
}

/// What a merge took from another configuration, and what it left behind.
///
/// Handed back rather than printed. Nothing in this crate writes to stdout,
/// and whoever asked for the merge is the one who knows where a line goes -
/// a log pane, a terminal, or nowhere at all.
#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    /// Devices taken, by name.
    pub devices: Vec<String>,
    /// Calculated channels taken, by name.
    pub calculated: Vec<String>,
    /// What was left behind, each with the reason it was.
    pub skipped: Vec<(String, String)>,
}

impl Merged {
    /// Whether anything at all came across.
    ///
    /// Skipping everything is a perfectly ordinary outcome - merging a file
    /// that is already in the project does exactly that - but it is worth
    /// saying differently from having added something.
    pub fn took_nothing(&self) -> bool {
        self.devices.is_empty() && self.calculated.is_empty()
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct DaqConfig {
    pub info: DaqInfo,
    pub devices: Vec<DeviceConfig>,
    /// Channels worked out from measured ones rather than read from hardware.
    ///
    /// Separate from `devices` because it is not one: it owns no hardware, is
    /// not connected to, and is not read on a thread. It appears in the results
    /// as a device because that is what it is to anyone reading them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calculated: Option<CalculatedDevice>,
}

impl DaqConfig {
    /// Every measured channel that is switched off, as an equation names one.
    ///
    /// A channel of its own accord, or every channel of a device that is off:
    /// to anything downstream those are the same thing, since neither will
    /// produce a reading.
    pub fn disabled_inputs(&self) -> BTreeSet<ChannelRef> {
        let mut off = BTreeSet::new();

        for device in self.devices.iter() {
            for channel in device.hardware.channel_infos() {
                if !device.enabled || !channel.enabled {
                    off.insert(ChannelRef {
                        device: device.info.name.clone(),
                        channel: channel.name,
                    });
                }
            }
        }

        off
    }

    /// Whether a calculated channel can produce anything.
    ///
    /// Switched off in its own right, or reading something that is. A
    /// calculated channel is only ever as available as its inputs: one whose
    /// source has been switched off has nothing to be worked out from, and
    /// saying so is better than leaving it waiting for a sample that is never
    /// coming.
    pub fn calculated_is_enabled(&self, channel: &CalculatedChannel) -> bool {
        if !channel.info.enabled {
            return false;
        }

        let off = self.disabled_inputs();
        !channel.inputs.values().any(|source| off.contains(source))
    }

    /// Every measured channel, in the form an equation refers to one.
    ///
    /// What a user interface offers when someone is choosing an input for a
    /// calculated channel, so the choice comes from the setup rather than from
    /// them typing a device and channel name correctly.
    pub fn available_inputs(&self) -> Vec<ChannelRef> {
        let mut inputs = Vec::new();
        for device in self.devices.iter() {
            for channel in device.hardware.channel_infos() {
                inputs.push(ChannelRef {
                    device: device.info.name.clone(),
                    channel: channel.name,
                });
            }
        }
        inputs
    }

    /// Every channel the setup produces, measured and calculated alike.
    ///
    /// The counterpart to `available_inputs`, and deliberately a different
    /// list. That one answers "what may an equation read", which is measured
    /// channels only: a calculated channel's output goes to the sink rather
    /// than back through the calculator, so one cannot feed another. This one
    /// answers "what is there to look at or record", which includes them —
    /// what a calculated channel works out is usually the quantity somebody
    /// actually wanted, and the last thing to hide from a plot.
    ///
    /// Devices pointed at hardware another device has already claimed.
    ///
    /// Checked over the whole setup rather than at the moment somebody picks
    /// from a list, because the list is not the only way in: a configuration
    /// loaded from a file, one merged in from a library, and one edited by
    /// hand can all arrive with the same port named twice, and none of them
    /// passes through a dropdown.
    ///
    /// The first device to name it keeps it and later ones are the clashes,
    /// which is arbitrary but stable: the answer does not change while nothing
    /// is edited, so what is reported does not move around by itself.
    pub fn address_clashes(&self) -> Vec<AddressClash> {
        let mut taken: BTreeMap<String, String> = BTreeMap::new();
        let mut clashes = Vec::new();

        for device in self.devices.iter() {
            let Some(address) = device.hardware.address() else { continue };

            // Compared without case, because Windows does not tell COM3 from
            // com3 and a hand written config may say either.
            match taken.get(&address.to_lowercase()) {
                Some(first) => clashes.push(AddressClash {
                    device: device.info.name.clone(),
                    address,
                    taken_by: first.clone(),
                }),
                None => {
                    taken.insert(address.to_lowercase(), device.info.name.clone());
                }
            }
        }

        clashes
    }

    /// Take everything from another configuration that this one has room for.
    ///
    /// The point is a library: a file of devices somebody keeps to hand and
    /// adds to whatever they are setting up. So only the devices and the
    /// calculated channels cross over. The other file's name, its author and
    /// anything else about the project it came from stay where they are.
    ///
    /// This configuration wins every disagreement. A device whose name is
    /// already here is left alone *entirely* rather than merged into, because
    /// a channel's binding - which field of a serial frame, which input of a
    /// Pico - only means something beside the hardware settings it was written
    /// against. Adding a channel bound to field 3 to a device reading a
    /// different frame would record the wrong number without complaining,
    /// which is the one failure worth going out of the way to avoid.
    ///
    /// Nothing is renamed and nothing is overwritten, so merging the same file
    /// twice does nothing the second time.
    pub fn merge(&mut self, other: DaqConfig) -> Merged {
        let mut report = Merged::default();

        for device in other.devices {
            let name = device.info.name.clone();
            if self.devices.iter().any(|have| have.info.name == name) {
                report
                    .skipped
                    .push((name, "a device of that name is already here".to_string()));
                continue;
            }
            self.devices.push(device);
            report.devices.push(name);
        }

        if let Some(incoming) = other.calculated {
            self.merge_calculated(incoming, &mut report);
        }

        report
    }

    /// The calculated half of a merge, once the devices have arrived.
    ///
    /// Its own rule, because a calculated channel is not a device: it reads
    /// measured channels by name, and one whose inputs are not here would
    /// compile, load, and then quietly never produce a sample, because the
    /// trigger it waits on would never arrive. Silence like that is worse than
    /// an error, so it is left behind and said so.
    fn merge_calculated(&mut self, incoming: CalculatedDevice, report: &mut Merged) {
        // Worked out after the devices above, so a channel may read one that
        // arrived in the same merge. A name that matched an existing device
        // resolves to *that* device's channel, which is the same rule applied
        // one level down: what is already here wins.
        let available: BTreeSet<ChannelRef> = self.available_inputs().into_iter().collect();

        // Whether there was one before decides whether to put one back: an
        // empty calculated device is still a deliberate part of a setup, and a
        // merge that added nothing should not quietly remove it.
        let existed = self.calculated.is_some();
        let mut here = self
            .calculated
            .take()
            .unwrap_or(CalculatedDevice { info: incoming.info, channels: Vec::new() });

        for channel in incoming.channels {
            let name = channel.info.name.clone();

            if here.channels.iter().any(|have| have.info.name == name) {
                report.skipped.push((
                    name,
                    "a calculated channel of that name is already here".to_string(),
                ));
                continue;
            }

            let missing: Vec<String> = channel
                .inputs
                .values()
                .filter(|input| !available.contains(input))
                .map(|input| input.to_string())
                .collect();

            if !missing.is_empty() {
                report.skipped.push((name, format!("it reads {}", missing.join(" and "))));
                continue;
            }

            here.channels.push(channel);
            report.calculated.push(name);
        }

        if existed || !here.channels.is_empty() {
            self.calculated = Some(here);
        }
    }

    /// Calculated channels come last, as they do in the tree and in the
    /// results: they are worked out from what precedes them.
    pub fn all_channels(&self) -> Vec<ChannelRef> {
        let mut channels = self.available_inputs();

        if let Some(calculated) = self.calculated.as_ref() {
            for channel in calculated.channels.iter() {
                channels.push(ChannelRef {
                    device: calculated.info.name.clone(),
                    channel: channel.info.name.clone(),
                });
            }
        }
        channels
    }
}

impl DeviceConfig {
    /// How often this device's channels produce a sample, where the setup says.
    ///
    /// None for a device that streams at a rate of its own choosing: it has to
    /// be measured from what arrives.
    pub fn sample_interval(&self) -> Option<std::time::Duration> {
        match self.hardware.sample_rate() {
            SampleRate::Fixed(interval) => Some(interval),
            SampleRate::PerRead => Some(std::time::Duration::from_millis(self.read_interval_ms)),
            SampleRate::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calculated::CalculatedChannel;
    use crate::channel::ChannelInfo;
    use crate::hardware::mock_hardware::{
        MockHardwareChannel, MockHardwareConfig, MockHardwareInput,
    };
    use std::collections::BTreeMap;

    fn setup(devices: Vec<DeviceConfig>) -> DaqConfig {
        DaqConfig {
            info: DaqInfo { name: "a project".to_string(), author: "somebody".to_string() },
            devices,
            calculated: None,
        }
    }

    /// A mock device of a given name, with the named channels on it.
    fn device(name: &str, channels: &[&str]) -> DeviceConfig {
        DeviceConfig {
            info: DeviceInfo { name: name.to_string() },
            read_interval_ms: 100,
            enabled: true,
            hardware: HardwareConfig::MockHardware(MockHardwareConfig {
                acquisition: Default::default(),
                channels: channels
                    .iter()
                    .map(|channel| MockHardwareChannel {
                        info: ChannelInfo {
                            name: channel.to_string(),
                            ..Default::default()
                        },
                        input: MockHardwareInput::Constant(1.0),
                    })
                    .collect(),
            }),
        }
    }

    /// A calculated channel reading one measured channel.
    fn calculated(name: &str, reads: (&str, &str)) -> CalculatedChannel {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "v".to_string(),
            ChannelRef { device: reads.0.to_string(), channel: reads.1.to_string() },
        );

        CalculatedChannel {
            info: ChannelInfo { name: name.to_string(), ..Default::default() },
            inputs,
            parameters: BTreeMap::new(),
            equation: "v * 2".to_string(),
        }
    }

    fn with_calculated(mut config: DaqConfig, channels: Vec<CalculatedChannel>) -> DaqConfig {
        config.calculated = Some(CalculatedDevice {
            info: DeviceInfo { name: "Calculated".to_string() },
            channels,
        });
        config
    }

    fn on_port(name: &str, port: &str) -> DeviceConfig {
        DeviceConfig {
            info: DeviceInfo { name: name.to_string() },
            read_interval_ms: 100,
            hardware: HardwareConfig::SerialStream(
                crate::hardware::serial_stream::SerialStreamConfig {
                    port: port.to_string(),
                    baudrate: 115200,
                    frame_pattern: r"#([^#$]*)\$".to_string(),
                    channels: vec![],
                },
            ),
            enabled: true,
        }
    }

    #[test]
    fn two_devices_on_one_port_are_reported_against_the_later_one() {
        let config = setup(vec![
            on_port("Arduino 1", "COM3"),
            on_port("Arduino 2", "COM3"),
            on_port("Arduino 3", "COM7"),
        ]);

        let clashes = config.address_clashes();

        assert_eq!(clashes.len(), 1, "{:?}", clashes);
        // The first to name it keeps it, so the complaint is about the second.
        assert_eq!(clashes[0].device, "Arduino 2");
        assert_eq!(clashes[0].taken_by, "Arduino 1");
        assert_eq!(clashes[0].address, "COM3");
    }

    #[test]
    fn a_port_is_the_same_port_whatever_its_case() {
        // Windows does not tell COM3 from com3, and a config may be written
        // by hand.
        let config = setup(vec![on_port("Arduino 1", "COM3"), on_port("Arduino 2", "com3")]);

        assert_eq!(config.address_clashes().len(), 1);
    }

    #[test]
    fn devices_with_no_port_chosen_yet_do_not_clash_with_each_other() {
        // Two serial devices added and not yet pointed anywhere. Neither is
        // taking anything, so neither is in the other's way.
        let config = setup(vec![on_port("Arduino 1", ""), on_port("Arduino 2", "   ")]);

        assert!(config.address_clashes().is_empty());
    }

    #[test]
    fn hardware_that_names_no_particular_unit_is_not_judged() {
        // Two mock devices share nothing, and two Picos take whichever unit
        // they find rather than one the config names, so nothing here can say
        // whether they are the same box.
        let config = setup(vec![device("one", &["a"]), device("two", &["b"])]);

        assert!(config.address_clashes().is_empty());
    }

    #[test]
    fn a_switched_off_device_takes_its_channels_with_it() {
        // To anything downstream the two are the same: neither the device nor
        // the channel will produce a reading.
        let mut project = setup(vec![device("Rig", &["Flow", "Pressure"])]);
        assert!(project.disabled_inputs().is_empty());

        project.devices[0].enabled = false;

        let off = project.disabled_inputs();
        assert_eq!(off.len(), 2, "{:?}", off);
        assert!(off.contains(&ChannelRef {
            device: "Rig".to_string(),
            channel: "Flow".to_string()
        }));
    }

    #[test]
    fn a_calculated_channel_is_off_when_what_it_reads_is_off() {
        let project = setup(vec![device("Rig", &["Flow"])]);
        let mut project =
            with_calculated(project, vec![calculated("doubled", ("Rig", "Flow"))]);

        let channel = project.calculated.as_ref().unwrap().channels[0].clone();
        assert!(project.calculated_is_enabled(&channel), "everything is on");

        // Switching off the measured channel switches off what reads it.
        if let HardwareConfig::MockHardware(mock) = &mut project.devices[0].hardware {
            mock.channels[0].info.enabled = false;
        }
        assert!(!project.calculated_is_enabled(&channel), "its only input is off");
    }

    #[test]
    fn a_calculated_channel_can_be_switched_off_on_its_own() {
        let project = setup(vec![device("Rig", &["Flow"])]);
        let project = with_calculated(project, vec![calculated("doubled", ("Rig", "Flow"))]);

        let mut channel = project.calculated.as_ref().unwrap().channels[0].clone();
        channel.info.enabled = false;

        assert!(!project.calculated_is_enabled(&channel));
    }

    #[test]
    fn a_device_with_a_name_of_its_own_is_taken() {
        let mut project = setup(vec![device("Arduino", &["temp"])]);
        let report = project.merge(setup(vec![device("Pico", &["cold junction"])]));

        assert_eq!(report.devices, vec!["Pico".to_string()]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(project.devices.len(), 2);
    }

    #[test]
    fn a_device_whose_name_is_taken_is_left_behind_whole() {
        // Not merged into. The incoming channel is bound to hardware settings
        // that are not the ones here, so adding it would read some other
        // device's field 3.
        let mut project = setup(vec![device("Arduino", &["temp"])]);
        let report = project.merge(setup(vec![device("Arduino", &["temp", "flow"])]));

        assert!(report.devices.is_empty(), "{:?}", report.devices);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "Arduino");
        assert!(report.skipped[0].1.contains("already here"), "{}", report.skipped[0].1);

        assert_eq!(project.devices.len(), 1);
        assert_eq!(project.devices[0].hardware.channel_infos().len(), 1, "left alone entirely");
    }

    #[test]
    fn merging_the_same_file_twice_does_nothing_the_second_time() {
        let library = setup(vec![device("Pico", &["cold junction"])]);

        let mut project = setup(vec![]);
        project.merge(library.clone());
        let again = project.merge(library);

        assert!(again.took_nothing());
        assert_eq!(project.devices.len(), 1);
    }

    #[test]
    fn the_other_projects_name_does_not_come_with_its_devices() {
        // A library is a file of devices. Whose project it was written in is
        // not something to inherit.
        let mut project = setup(vec![]);
        let mut library = setup(vec![device("Pico", &["cold junction"])]);
        library.info.name = "somebody else's rig".to_string();

        project.merge(library);

        assert_eq!(project.info.name, "a project");
        assert_eq!(project.info.author, "somebody");
    }

    #[test]
    fn a_calculated_channel_may_read_a_device_arriving_beside_it() {
        // The devices land first, so a library holding a device and a channel
        // that reads it works as one piece.
        let mut project = setup(vec![]);
        let library = with_calculated(
            setup(vec![device("Pico", &["cold junction"])]),
            vec![calculated("doubled", ("Pico", "cold junction"))],
        );

        let report = project.merge(library);

        assert_eq!(report.devices, vec!["Pico".to_string()]);
        assert_eq!(report.calculated, vec!["doubled".to_string()]);
    }

    #[test]
    fn a_calculated_channel_reading_something_that_is_not_here_is_left_behind() {
        // It would compile and load, and then never produce a sample, because
        // the trigger it waits on would never arrive. Silence is worse than
        // being told.
        let mut project = setup(vec![]);
        let library =
            with_calculated(setup(vec![]), vec![calculated("doubled", ("Pico", "cold junction"))]);

        let report = project.merge(library);

        assert!(report.calculated.is_empty(), "{:?}", report.calculated);
        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0].1.contains("Pico/cold junction"),
            "the reason should name what is missing: {}",
            report.skipped[0].1
        );
        assert!(project.calculated.is_none(), "nothing to put it in");
    }

    #[test]
    fn a_calculated_channel_whose_name_is_taken_is_left_behind() {
        let project = setup(vec![device("Pico", &["cold junction"])]);
        let mut project =
            with_calculated(project, vec![calculated("doubled", ("Pico", "cold junction"))]);

        let library = with_calculated(
            setup(vec![]),
            vec![calculated("doubled", ("Pico", "cold junction"))],
        );
        let report = project.merge(library);

        assert!(report.calculated.is_empty());
        assert_eq!(report.skipped[0].0, "doubled");
        assert_eq!(project.calculated.expect("still there").channels.len(), 1);
    }
}
