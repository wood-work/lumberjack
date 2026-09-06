use crate::{ Error, Result };
use crate::config::DaqConfig;
use std::fs::File;
use std::io::{ Read, Write }; // ErrorKind
// use std::io::BufReader;
// use std::path::Path;
// use serde::{Deserialize, Serialize};
use serde_json::to_string_pretty;
use std::ffi::OsStr;

// #[derive(Serialize, Deserialize)]
// pub struct ConfigFileDevice {
//     pub device_type: DeviceType,
//     pub info: DeviceInfo,
// }

// #[derive(Serialize, Deserialize)]
// pub struct ConfigFile {
//     pub devices: Vec<ConfigFileDevice>,
// }

// pub fn read_configuration_file<P: AsRef<Path>>(path: P) -> Result<()> {
//     let file = File::open(path)?;
//     // Wrap the file reader in BufReader for efficiency.
//     let reader = BufReader::new(file);

//     let config = serde_json::from_reader(reader)?;

//     Ok(())
// }

pub fn write_configuration_file(path: &std::path::PathBuf, config: &DaqConfig) -> Result<()> {
    check_file_extension(path, OsStr::new("json"))?;
    let mut file = File::create(path)?;
    file.write_all(to_string_pretty(config)?.as_bytes())?;
    return Ok(());
}

/// Read a saved setup. This returns a `DaqConfig`, not a `Daq`: what comes off
/// disk is a description, and turning it into something connected to hardware
/// is `Daq::from_config`.
pub fn read_configuration_file(path: &std::path::PathBuf) -> Result<DaqConfig> {
    check_file_extension(path, OsStr::new("json"))?;

    let mut file = File::open(path).map_err(|error| match error.kind() {
        // By far the most likely way to get here is running somewhere that is
        // not a project, so say that rather than repeating the operating
        // system's "cannot find the file specified" with no file named.
        std::io::ErrorKind::NotFound => Error::NoProjectHere {
            directory: describe_parent(path),
        },
        _ => Error::UnreadableFile { path: path.display().to_string(), source: error },
    })?;

    let mut data = String::new();
    file.read_to_string(&mut data)
        .map_err(|error| Error::UnreadableFile {
            path: path.display().to_string(),
            source: error,
        })?;

    // serde reports a line and column, which is not much use without knowing
    // which file they are in.
    serde_json::from_str(&data).map_err(|error| Error::UnreadableConfig {
        path: path.display().to_string(),
        source: error,
    })
}

/// The directory a path sits in, worded for someone reading an error.
fn describe_parent(path: &std::path::Path) -> String {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => "the current directory".to_string(),
        Some(parent) if parent == std::path::Path::new(".") => "the current directory".to_string(),
        Some(parent) => parent.display().to_string(),
        None => "the current directory".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeviceConfig;
    use crate::daq::DaqInfo;
    use crate::device::DeviceInfo;
    use crate::hardware::mock_hardware::MockHardwareConfig;
    use crate::hardware::HardwareConfig;

    /// A library file written, read back, and merged into a project.
    ///
    /// The merge itself is tested against configurations built by hand. This
    /// is the other half of what somebody adding a library actually does,
    /// which is that the thing being merged came off a disk: a setting that
    /// failed to survive the round trip would pass those tests and fail here.
    #[test]
    fn devices_can_be_taken_from_a_file_into_a_project() {
        let path = std::env::temp_dir().join("lumberdaq_library_test.json");
        let _ = std::fs::remove_file(&path);

        let library = DaqConfig {
            info: DaqInfo { name: "a library".to_string(), author: "somebody".to_string() },
            devices: vec![DeviceConfig {
                info: DeviceInfo { name: "Thermocouples".to_string() },
                read_interval_ms: 500,
                hardware: HardwareConfig::MockHardware(MockHardwareConfig::default()),
                enabled: true,
            }],
            calculated: None,
        };
        write_configuration_file(&path, &library).expect("a library should write");

        let read = read_configuration_file(&path).expect("and read back");

        let mut project = DaqConfig {
            info: DaqInfo { name: "a project".to_string(), author: "me".to_string() },
            devices: vec![],
            calculated: None,
        };
        let report = project.merge(read);

        assert_eq!(report.devices, vec!["Thermocouples".to_string()]);
        assert_eq!(project.devices.len(), 1);
        // Taken from the file rather than defaulted, which is what says the
        // round trip kept the settings and not only the names.
        assert_eq!(project.devices[0].read_interval_ms, 500);
        // And the library's own project details stayed behind.
        assert_eq!(project.info.name, "a project");

        let _ = std::fs::remove_file(&path);
    }

    /// The issue asks for this in as many words: a config without the key
    /// reads as enabled, and the key is not required unless something is
    /// switched off. Both halves matter — the first so every project written
    /// before this still loads, the second so adding the feature does not
    /// rewrite every file with a line saying nothing changed.
    #[test]
    fn switched_on_is_the_default_and_is_not_written_down() {
        let path = std::env::temp_dir().join("lumberdaq_enabled_test.json");
        let _ = std::fs::remove_file(&path);

        let config = DaqConfig {
            info: DaqInfo { name: "a project".to_string(), author: String::new() },
            devices: vec![DeviceConfig {
                info: DeviceInfo { name: "Rig".to_string() },
                read_interval_ms: 100,
                enabled: true,
                hardware: HardwareConfig::MockHardware(MockHardwareConfig::default()),
            }],
            calculated: None,
        };
        write_configuration_file(&path, &config).expect("it should write");

        let written = std::fs::read_to_string(&path).expect("and be readable");
        assert!(!written.contains("enabled"), "nothing is switched off:\n{}", written);

        // And a file that never mentions it reads as switched on.
        let read = read_configuration_file(&path).expect("it should read back");
        assert!(read.devices[0].enabled);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_something_off_is_written_down_and_read_back() {
        let path = std::env::temp_dir().join("lumberdaq_disabled_test.json");
        let _ = std::fs::remove_file(&path);

        let config = DaqConfig {
            info: DaqInfo { name: "a project".to_string(), author: String::new() },
            devices: vec![DeviceConfig {
                info: DeviceInfo { name: "Rig".to_string() },
                read_interval_ms: 100,
                enabled: false,
                hardware: HardwareConfig::MockHardware(MockHardwareConfig::default()),
            }],
            calculated: None,
        };
        write_configuration_file(&path, &config).expect("it should write");

        assert!(std::fs::read_to_string(&path).unwrap().contains("\"enabled\": false"));
        assert!(!read_configuration_file(&path).expect("it should read").devices[0].enabled);

        let _ = std::fs::remove_file(&path);
    }

    /// The commonest mistake is running somewhere that is not a project. The
    /// operating system only says a file was not found, without naming it, so
    /// this has to.
    #[test]
    fn a_directory_with_no_config_says_so_and_says_where() {
        let missing = std::env::temp_dir().join("lumberdaq_no_project").join("config.json");
        let error = read_configuration_file(&missing).err().unwrap();
        assert!(matches!(error, Error::NoProjectHere { .. }));
        let message = error.to_string();
        assert!(message.contains("no config.json"));
        assert!(message.contains("lumberdaq_no_project"));
    }

    /// Running with no argument looks here, and "." is not worth printing.
    #[test]
    fn the_current_directory_is_named_in_words() {
        assert_eq!(describe_parent(std::path::Path::new("config.json")), "the current directory");
        assert_eq!(describe_parent(std::path::Path::new("./config.json")), "the current directory");
        assert_eq!(describe_parent(std::path::Path::new("a/b/config.json")), "a/b");
    }

    /// serde reports a line and column, which needs a file name to be useful.
    #[test]
    fn a_malformed_config_names_the_file() {
        let directory = std::env::temp_dir().join("lumberdaq_bad_config");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        std::fs::write(&path, "{ not json ]").unwrap();

        let error = read_configuration_file(&path).err().unwrap();
        assert!(matches!(error, Error::UnreadableConfig { .. }));
        assert!(error.to_string().contains("config.json"));
        // The detail is a cause rather than part of the message.
        assert!(std::error::Error::source(&error).is_some());
    }
}

/// Refuse a path that is not the kind of file being asked for.
///
/// Lived with the csv sink until that was dropped. It is about configuration
/// files, which is here.
fn check_file_extension(path: &std::path::PathBuf, extension: &OsStr) -> Result<()> {
    if path.extension() != Some(extension) {
        let error_msg = format!(
            "Incorrect path extension. The extension must be {:?} but {:?} was provided.",
            &extension,
            &path.extension()
        );
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error_msg).into());
    }
    Ok(())
}
