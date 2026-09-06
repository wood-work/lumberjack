use crate::config::DaqConfig;
use crate::storage::{ Batch, DataSink };
use crate::{ Error, Result };
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

/// Records to a SQLite database instead of a csv file plus a json sidecar.
///
/// The schema is normalised, which is the main difference from the csv. The
/// long format csv repeats the device name, the channel name and a formatted
/// timestamp on every row; here a reading is three numbers referring to a
/// channel that is described once.
///
/// ```text
/// runs      one row per recording session: name, author, when it started
/// devices   one row per device, with the hardware config it ran with
/// channels  one row per channel, pointing at its device
/// readings  channel_id, timestamp, value
/// ```
///
/// Because the description lives in the same file as the data, there is no
/// sidecar to keep in step with it, and a results file can say what produced
/// it without needing the config.json it was recorded next to.
///
/// A database holds many runs rather than one. Recording again appends a new
/// run instead of failing or overwriting, which is what a csv does today: it
/// truncates, so a second run silently destroys the first.
///
/// Devices and channels are deliberately *not* shared between runs even when
/// their names match. A name is a label, not an identity: the same channel name
/// can read a different frame index, a different port, or a different physical
/// sensor. Storing each run's own rows keeps what that run actually believed,
/// and the hardware column is there so two runs can be compared properly rather
/// than assumed equal. Queries across runs match on name and still work.
///
/// Timestamps are stored as microseconds since the epoch rather than as text:
/// smaller, and no string to format per row.
pub struct SqliteSink {
    connection: Connection,
    /// device name -> channel name -> row id, built once from the header so a
    /// batch does not have to look its channel up by string on every write.
    channel_ids: HashMap<String, HashMap<String, i64>>,
    /// Whether there is an open transaction waiting to be committed.
    uncommitted: bool,
}

/// Stamped into the file so a database written by a different version of the
/// schema is refused with an explanation rather than a raw SQL error about a
/// missing column. Bump it whenever the tables change.
pub(crate) const SCHEMA_VERSION: i32 = 6;

/// The oldest schema this build can still read.
///
/// Writing and reading want different things of a file, and conflating them
/// makes every old recording unreadable the moment this number moves. A run
/// is appended to, so writing needs the schema it expects, exactly. Reading
/// needs only the columns it selects to be there — and every version since 2
/// has them, because what changed after that was columns going away that
/// nothing reads.
///
/// What each version did, and whether a reader can live with it:
///
/// | Version | Change                                    | Readable now |
/// |---------|-------------------------------------------|--------------|
/// | 1       | the first sink                            | no: no `devices.hardware` |
/// | 2       | recorded each device's configuration      | yes |
/// | 3       | one timestamp per reading                 | yes |
/// | 4       | per channel scaling                       | yes |
/// | 5       | dropped `devices.description`             | yes |
/// | 6       | dropped `channels.description`            | yes |
///
/// Raise this only when a change genuinely stops an older file being read.
/// Extra columns are harmless: every query names the columns it wants.
pub(crate) const READABLE_FROM: i32 = 2;

impl SqliteSink {
    pub fn new(path: &Path) -> Result<SqliteSink> {
        let connection = Connection::open(path)?;

        // Write ahead logging, for two reasons. Readers do not block the
        // writer, so a UI can plot a run while it is still recording; and
        // commits append to a log rather than rewriting pages.
        connection.pragma_update(None, "journal_mode", "WAL")?;

        // NORMAL means a commit does not wait for the disk. A crashed process
        // still loses nothing, since the log is already handed to the OS; a
        // power cut loses the last few seconds. FULL would make commits durable
        // against power loss too, at roughly a millisecond each.
        connection.pragma_update(None, "synchronous", "NORMAL")?;

        Ok(SqliteSink {
            connection: connection,
            channel_ids: HashMap::new(),
            uncommitted: false,
        })
    }

    /// Refuse a database whose tables were written by a different schema.
    ///
    /// An empty file is stamped with the current version. A file that already
    /// has tables but does not match gets a clear error, because the failure
    /// otherwise surfaces much later as a missing column.
    fn check_schema_version(&mut self) -> Result<()> {
        let tables: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |row| row.get(0),
        )?;
        let found: i32 =
            self.connection
                .pragma_query_value(None, "user_version", |row| row.get(0))?;

        if tables == 0 {
            self.connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
            return Ok(());
        }
        if found != SCHEMA_VERSION {
            return Err(Error::DatabaseSchemaVersion {
                found: found,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Open a transaction if one is not already open.
    ///
    /// Inserts outside a transaction get one each, and every transaction is a
    /// separate commit, which is thousands of times slower. Batching until
    /// `flush` is what keeps this fast.
    fn begin(&mut self) -> Result<()> {
        if !self.uncommitted {
            self.connection.execute_batch("BEGIN")?;
            self.uncommitted = true;
        }
        Ok(())
    }
}

impl DataSink for SqliteSink {
    fn init(&mut self, config: &DaqConfig) -> Result<()> {
        self.check_schema_version()?;
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                 id          INTEGER PRIMARY KEY,
                 name        TEXT NOT NULL,
                 author      TEXT NOT NULL,
                 started     TEXT NOT NULL
             );
             -- Device and channel names are unique within a run, not across the
             -- file, so recording the same rig again is a new run rather than a
             -- clash with the last one.
             CREATE TABLE IF NOT EXISTS devices (
                 id          INTEGER PRIMARY KEY,
                 run_id      INTEGER NOT NULL REFERENCES runs(id),
                 name        TEXT NOT NULL,
                 -- No description: a device has a name, and what the hardware
                 -- is lives in the column below.
                 --
                 -- The whole hardware configuration as json: port, baud rate,
                 -- frame pattern, channel bindings. Names alone cannot say
                 -- whether two runs measured the same thing; this can, and it
                 -- makes a results file able to describe its own setup.
                 hardware    TEXT NOT NULL,
                 UNIQUE(run_id, name)
             );
             -- Enough to label a series: what it is called and what its
             -- numbers are in. What each channel physically reads is in
             -- devices.hardware, which holds the binding already; repeating it
             -- here would be a second copy of the same fact.
             CREATE TABLE IF NOT EXISTS channels (
                 id          INTEGER PRIMARY KEY,
                 device_id   INTEGER NOT NULL REFERENCES devices(id),
                 name        TEXT NOT NULL,
                 unit        TEXT NOT NULL,
                 UNIQUE(device_id, name)
             );
             CREATE TABLE IF NOT EXISTS readings (
                 channel_id  INTEGER NOT NULL REFERENCES channels(id),
                 timestamp   INTEGER NOT NULL,
                 value       REAL NOT NULL
             );
             -- Reading one channel over a time range is what every plot does.
             CREATE INDEX IF NOT EXISTS readings_by_channel
                 ON readings(channel_id, timestamp);",
        )?;

        self.connection.execute(
            "INSERT INTO runs (name, author, started) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                config.info.name,
                config.info.author,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        let run_id = self.connection.last_insert_rowid();

        for device in config.devices.iter() {
            let hardware = serde_json::to_string(&device.hardware)?;
            self.connection.execute(
                "INSERT INTO devices (run_id, name, hardware)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, device.info.name, hardware],
            )?;
            let device_id = self.connection.last_insert_rowid();

            let mut ids: HashMap<String, i64> = HashMap::new();
            for channel in device.hardware.channel_infos().iter() {
                self.connection.execute(
                    "INSERT INTO channels (device_id, name, unit)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![device_id, channel.name, channel.unit],
                )?;
                ids.insert(channel.name.clone(), self.connection.last_insert_rowid());
            }
            self.channel_ids.insert(device.info.name.clone(), ids);
        }

        // Calculated channels are a device as far as results are concerned, and
        // they need rows here or their batches have nowhere to go. What goes in
        // the hardware column is the calculated definition rather than a
        // HardwareConfig: that column answers "where did these values come
        // from", and for these the answer is the equations.
        if let Some(calculated) = &config.calculated {
            let definition = serde_json::to_string(calculated)?;
            self.connection.execute(
                "INSERT INTO devices (run_id, name, hardware)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, calculated.info.name, definition],
            )?;
            let device_id = self.connection.last_insert_rowid();

            let mut ids: HashMap<String, i64> = HashMap::new();
            for channel in calculated.channels.iter() {
                self.connection.execute(
                    "INSERT INTO channels (device_id, name, unit)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![device_id, channel.info.name, channel.info.unit],
                )?;
                ids.insert(channel.info.name.clone(), self.connection.last_insert_rowid());
            }
            self.channel_ids.insert(calculated.info.name.clone(), ids);
        }
        Ok(())
    }

    fn write_batch(&mut self, batch: &Batch) -> Result<()> {
        if batch.datapoints.is_empty() {
            return Ok(());
        }

        let channel_id = self
            .channel_ids
            .get(batch.device.as_str())
            .and_then(|channels| channels.get(batch.channel.as_str()))
            .copied()
            .ok_or_else(|| Error::UnknownChannel {
                device: batch.device.clone(),
                channel: batch.channel.clone(),
            })?;

        self.begin()?;
        let mut statement = self
            .connection
            .prepare_cached("INSERT INTO readings (channel_id, timestamp, value) VALUES (?1, ?2, ?3)")?;
        for datapoint in batch.datapoints.iter() {
            statement.execute(rusqlite::params![
                channel_id,
                datapoint.datetime.timestamp_micros(),
                datapoint.value
            ])?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.uncommitted {
            self.connection.execute_batch("COMMIT")?;
            self.uncommitted = false;
        }
        Ok(())
    }
}

impl Drop for SqliteSink {
    /// Commit whatever is outstanding rather than rolling it back.
    ///
    /// Without this, a run that ends without a final flush would discard its
    /// last transaction, which is a surprising way to lose data.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelInfo;
    use crate::config::DeviceConfig;
    use crate::daq::DaqInfo;
    use crate::datapoint::DataPoint;
    use crate::device::DeviceInfo;
    use crate::hardware::HardwareConfig;
    use crate::hardware::serial_stream::{ SerialStreamChannel, SerialStreamConfig };

    /// A serial device on COM3 with one channel reading frame index 1.
    fn header() -> DaqConfig {
        config_on_port("COM3", 1)
    }

    /// The same setup, but parameterised on the details that decide whether two
    /// runs actually measured the same thing.
    fn config_on_port(port: &str, index: i64) -> DaqConfig {
        DaqConfig {
            info: DaqInfo { name: "Test".to_string(), author: "Nobody".to_string() },
            calculated: None,
            devices: vec![DeviceConfig {
                info: DeviceInfo {
                    name: "Serial test device".to_string(),
                },
                read_interval_ms: 100,
                enabled: true,
                hardware: HardwareConfig::SerialStream(SerialStreamConfig {
                    port: port.to_string(),
                    baudrate: 115200,
                    frame_pattern: r"#([^#$]*)\$".to_string(),
                    channels: vec![SerialStreamChannel {
                        info: ChannelInfo {
                            name: "Pressure".to_string(),
                            unit: "Pa".to_string(),
                        scale: None,
                            enabled: true,
                        },
                        index: index,
                    }],
                }),
            }],
        }
    }

    fn batch(values: &[f64]) -> Batch {
        Batch {
            device: "Serial test device".to_string(),
            channel: "Pressure".to_string(),
            datapoints: values
                .iter()
                .map(|value| DataPoint { datetime: chrono::Utc::now(), value: *value })
                .collect(),
        }
    }

    fn sink_in(dir: &std::path::Path) -> SqliteSink {
        let mut sink = SqliteSink::new(&dir.join("results.db")).unwrap();
        sink.init(&header()).unwrap();
        sink
    }

    /// A directory with nothing in it. Note the tests that check what happens
    /// on a *second* run deliberately reuse the same directory rather than
    /// calling this twice, since starting clean is what hid the bug.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lumberdaq_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn readings_land_against_the_right_channel() {
        let dir = temp_dir("sqlite_readings");
        let mut sink = sink_in(&dir);
        sink.write_batch(&batch(&[1.0, 2.0, 3.0])).unwrap();
        sink.flush().unwrap();

        let count: i64 = sink
            .connection
            .query_row(
                "SELECT COUNT(*) FROM readings r
                   JOIN channels c ON c.id = r.channel_id
                  WHERE c.name = 'Pressure'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    /// A batch names its channel by string; anything not in the header has
    /// nowhere to go and must say so rather than being dropped.
    #[test]
    fn a_channel_missing_from_the_header_is_rejected() {
        let dir = temp_dir("sqlite_unknown");
        let mut sink = sink_in(&dir);
        let mut stray = batch(&[1.0]);
        stray.channel = "Not configured".to_string();
        assert!(matches!(
            sink.write_batch(&stray),
            Err(Error::UnknownChannel { .. })
        ));
    }

    /// Nothing is committed until flush, which is what keeps the insert rate up.
    #[test]
    fn writes_are_held_until_flushed() {
        let dir = temp_dir("sqlite_flush");
        let mut sink = sink_in(&dir);
        sink.write_batch(&batch(&[1.0, 2.0])).unwrap();
        assert!(sink.uncommitted);
        sink.flush().unwrap();
        assert!(!sink.uncommitted);
        // Flushing again with nothing outstanding is not an error.
        sink.flush().unwrap();
    }

    /// The description lives in the same file as the data, so there is no
    /// sidecar that can drift away from it.
    #[test]
    fn the_setup_is_recorded_alongside_the_readings() {
        let dir = temp_dir("sqlite_header");
        let sink = sink_in(&dir);
        let (name, unit): (String, String) = sink
            .connection
            .query_row("SELECT name, unit FROM channels", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(name, "Pressure");
        assert_eq!(unit, "Pa");
    }

    /// Recording twice into the same file used to fail on the unique constraint
    /// over device names. A second run is a second run, not a clash.
    #[test]
    fn recording_again_adds_a_run_rather_than_failing() {
        let dir = temp_dir("sqlite_second_run");

        let mut first = sink_in(&dir);
        first.write_batch(&batch(&[1.0, 2.0])).unwrap();
        first.flush().unwrap();
        drop(first);

        let mut second = sink_in(&dir);
        second.write_batch(&batch(&[3.0])).unwrap();
        second.flush().unwrap();

        let runs: i64 = second
            .connection
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(runs, 2);

        // Both runs' data is present, and each is attributable to its own run.
        let per_run: Vec<(i64, i64)> = second
            .connection
            .prepare(
                "SELECT d.run_id, COUNT(*) FROM readings r
                   JOIN channels c ON c.id = r.channel_id
                   JOIN devices  d ON d.id = c.device_id
                  GROUP BY d.run_id ORDER BY d.run_id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(per_run, vec![(1, 2), (2, 1)]);
    }

    /// The binding is recoverable without a column of its own: it is in the
    /// stored hardware config, which is the one place it is written.
    #[test]
    fn the_binding_is_recoverable_from_the_stored_config() {
        let dir = temp_dir("sqlite_binding");
        let sink = sink_in(&dir);
        let index: i64 = sink
            .connection
            .query_row(
                "SELECT json_extract(channel.value, '$.index')
                   FROM devices, json_each(devices.hardware, '$.channels') AS channel
                  WHERE json_extract(channel.value, '$.name') = 'Pressure'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
    }

    /// The point of storing the config: two runs whose channels share a name
    /// can still be told apart, because the port and the frame index are there
    /// to compare. Merging them on name alone would have hidden this.
    #[test]
    fn two_runs_with_matching_names_can_still_be_told_apart() {
        let dir = temp_dir("sqlite_provenance");
        let path = dir.join("results.db");

        let mut first = SqliteSink::new(&path).unwrap();
        first.init(&config_on_port("COM3", 1)).unwrap();
        drop(first);

        // Same device name, same channel name, same unit. Different port, and
        // reading a different field of the frame: a different measurement.
        let mut second = SqliteSink::new(&path).unwrap();
        second.init(&config_on_port("COM4", 2)).unwrap();

        let configs: Vec<String> = second
            .connection
            .prepare("SELECT hardware FROM devices ORDER BY run_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(configs.len(), 2);
        assert!(configs[0].contains("COM3"));
        assert!(configs[1].contains("COM4"));
        assert_ne!(configs[0], configs[1]);

        // And the labels really are identical, which is exactly why comparing
        // on names would have called these the same channel.
        let names: Vec<String> = second
            .connection
            .prepare("SELECT name FROM channels")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(names, vec!["Pressure".to_string(), "Pressure".to_string()]);
    }

    /// A database from an older schema should say so, rather than failing later
    /// with a complaint about a missing column.
    #[test]
    fn a_database_from_another_schema_is_refused() {
        let dir = temp_dir("sqlite_schema");
        let path = dir.join("results.db");
        {
            let old = Connection::open(&path).unwrap();
            old.execute_batch("CREATE TABLE devices (id INTEGER PRIMARY KEY, name TEXT);")
                .unwrap();
            old.pragma_update(None, "user_version", 0).unwrap();
        }
        let mut sink = SqliteSink::new(&path).unwrap();
        assert!(matches!(
            sink.init(&header()),
            Err(Error::DatabaseSchemaVersion { found: 0, expected: SCHEMA_VERSION })
        ));
    }

    /// A later run must not steer its readings into the earlier run's channels.
    #[test]
    fn a_second_run_writes_to_its_own_channels() {
        let dir = temp_dir("sqlite_run_isolation");

        let mut first = sink_in(&dir);
        first.write_batch(&batch(&[1.0])).unwrap();
        first.flush().unwrap();
        drop(first);

        let mut second = sink_in(&dir);
        second.write_batch(&batch(&[2.0])).unwrap();
        second.flush().unwrap();

        let channel_id: i64 = second
            .connection
            .query_row(
                "SELECT DISTINCT r.channel_id FROM readings r
                   JOIN channels c ON c.id = r.channel_id
                   JOIN devices  d ON d.id = c.device_id
                  WHERE d.run_id = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Run 2's readings belong to run 2's channel row, not run 1's.
        assert_ne!(channel_id, 1);
    }
}
