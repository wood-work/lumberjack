use crate::{ Error, Result };
use crate::datapoint::DataPoint;
use crate::channel::ChannelInfo;
use crate::device::{ Device, DeviceInterface };
use crate::hardware::{HardwareDataAcquisition, Hardware };
use serde::{ Deserialize, Serialize };
use serialport;
use chrono::{ DateTime, Utc };
use regex::Regex;
use std::collections::{ BTreeMap, HashSet };
use std::io::Read;
use std::sync::atomic::{ AtomicBool, AtomicU64, Ordering };
use std::sync::mpsc::{ self, Receiver, Sender, TryRecvError };
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{ Duration, Instant };


/// This device reads a stream of data from a serial port in a comma-separated format, and splits it into channels according to the config.
/// The data is in a format like the following that may mix data types and also contain unwanted characters: `#1,2.00,0,1,1,STBY,0,1,0$`
/// Data types will all have to be converted to those required for DataPoint, which is a float64. 
/// The cahnnel names must be known upfront and the index of each channel in the stream must be specified in the config. The device will read all channels together and then split them into the configured channels for storage.

/// Everything needed to describe a serial device in a config file.
#[derive(Serialize, Deserialize, Clone)]
pub struct SerialStreamConfig {
    pub port: String,
    pub baudrate: u32,
    /// A regular expression matching one complete frame, used both to find
    /// frame boundaries and to strip whatever wraps the data.
    ///
    /// If the expression has a capture group, that group is the data; if not,
    /// the whole match is. So `#([^#$]*)\$` keeps what sits between the
    /// markers, and `([^\r\n]+)\r?\n` reads a device that just sends lines.
    ///
    /// The expression must require whatever ends a frame. That is what tells
    /// us a frame has fully arrived rather than being half way through the
    /// wire. A pattern that can match without its terminator, such as `(.*)`,
    /// will happily match a partial frame and hand back truncated data.
    #[serde(default = "default_frame_pattern")]
    pub frame_pattern: String,
    pub channels: Vec<SerialStreamChannel>,
}

/// One channel: what it is, and where to find it in the frame.
///
/// Description and binding live together deliberately. When they were two
/// parallel lists, matched by position, a config that listed them in different
/// orders would quietly record each channel's data under another's name.
#[derive(Serialize, Deserialize, Clone)]
pub struct SerialStreamChannel {
    #[serde(flatten)]
    pub info: ChannelInfo,
    /// Which comma separated field of the frame this channel reads, counting
    /// from zero.
    pub index: i64,
}

/// Matches the `#...$` framing described above. Used when a config does not
/// name a pattern, so configs written before this setting existed still load.
fn default_frame_pattern() -> String {
    r"#([^#$]*)\$".to_string()
}

/// What listening to a port for a moment found.
///
/// The question being answered is narrow: does this configuration read this
/// device? A baud rate cannot be measured, only tried, so the only honest test
/// is to open the port and see whether anything readable comes out of it.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamCheck {
    /// A frame arrived and the channels read it. Nothing to complain about.
    Reads,
    /// Bytes arrived and none of them completed a frame. Far and away the
    /// usual cause is a baud rate that does not match the device, though a
    /// frame pattern describing a different protocol looks the same.
    Unreadable,
    /// Frames arrived and matched, and the channels could not read them.
    /// The stream really is frames - see `ENOUGH_FRAMES` - so the rate is
    /// right and the fault is a channel pointing at a field that is not
    /// there or is not a number.
    Mismatched { reason: String },
    /// The port opened and nothing came out of it at all.
    Silent,
}

/// Listen to a port for a moment and say whether the configuration reads it.
///
/// Opens the port, so it cannot be used while a run has it, and it asserts DTR
/// on the way in - which resets an Arduino and most boards like it. That is
/// tolerable for something somebody asked for by pressing a button, and is why
/// this is not on a timer. It also means `patience` has to outlast a bootloader
/// or the only thing heard will be the silence after the reset.
///
/// Returns the moment one frame reads, so a working device answers quickly and
/// only a broken one waits out the whole patience.
pub fn check_stream(config: &SerialStreamConfig, patience: Duration) -> Result<StreamCheck> {
    let pattern = Regex::new(&config.frame_pattern).map_err(|error| {
        Error::InvalidFramePattern {
            pattern: config.frame_pattern.clone(),
            port: config.port.clone(),
            source: error,
        }
    })?;

    let mut port = serialport::new(&config.port, config.baudrate)
        .timeout(Duration::from_millis(100))
        .open()?;

    let deadline = Instant::now() + patience;
    let mut buffer = String::new();
    let mut bytes = [0u8; 4096];
    let mut mismatch: Option<String> = None;
    // Weighed rather than counted. One frame among a hundred kilobytes of
    // noise and one frame among a hundred kilobytes of frames are the same
    // number and completely different answers.
    let mut heard_bytes = 0usize;
    let mut frame_bytes = 0usize;

    while Instant::now() < deadline {
        let count = match port.read(&mut bytes) {
            Ok(0) => continue,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
            // The port has gone mid-check. Whatever was heard before that is
            // still the best answer available, and reporting a read error
            // instead would bury it.
            Err(_) => break,
        };

        // Measured after the lossy conversion, as the frames below are, so
        // the two are the same units: an invalid byte becomes a three byte
        // replacement character and would otherwise count as one.
        let arrived = String::from_utf8_lossy(&bytes[..count]);
        heard_bytes += arrived.len();
        buffer.push_str(&arrived);

        // What the device says between frames is not this function's
        // business: it is answering whether the settings read, and a boot
        // banner is neither evidence for nor against.
        while let Some(taken) = take_next_frame(&mut buffer, &pattern) {
            let frame = taken.frame;
            frame_bytes += frame.len();
            // Strict here, where the acquisition loop is forgiving. A
            // channel pointed at a status field is a mistake to find while
            // the indices are being chosen, not one to discover later as a
            // trace with holes in it, so one channel that cannot read its
            // field is enough to fail the check.
            match parse_frame_values(&frame, &config.channels)
                .iter()
                .find_map(|outcome| outcome.as_ref().err())
            {
                // One frame that reads settles it. Holding the port for the
                // rest of the patience would prove nothing further.
                None => return Ok(StreamCheck::Reads),
                Some(error) => {
                    mismatch.get_or_insert_with(|| error.to_string());
                }
            }
        }

        if buffer.len() > MAX_BUFFER_BYTES {
            buffer.clear();
        }
    }

    Ok(verdict(heard_bytes, frame_bytes, mismatch))
}

/// Judge what a spell of listening added up to.
///
/// Split from the listening so the arithmetic can be tested without a port.
/// Choosing between "the rate is wrong" and "a channel is wrong" is the whole
/// value of the check, and it is the part that was wrong the first time: a
/// single accidental match was taken as proof the rate was right.
fn verdict(heard_bytes: usize, frame_bytes: usize, mismatch: Option<String>) -> StreamCheck {
    // Frames that would not read only say something about the channels if the
    // stream was mostly frames to begin with. Otherwise they are noise finding
    // the pattern by accident, and the answer is the same as if they had never
    // matched at all.
    let mostly_frames =
        heard_bytes > 0 && frame_bytes as f64 / heard_bytes as f64 >= ENOUGH_FRAMES;

    match (heard_bytes > 0, mismatch) {
        (true, Some(reason)) if mostly_frames => StreamCheck::Mismatched { reason },
        (true, _) => StreamCheck::Unreadable,
        (false, _) => StreamCheck::Silent,
    }
}

/// A serial port that is plugged in at the moment.
///
/// Enough to choose one by: `COM7` alone is no help when three things are
/// plugged in, and the name the device reports is how somebody knows which is
/// theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortOption {
    /// What to put in the config: `COM7`, `/dev/ttyUSB0`.
    pub name: String,
    /// What the device calls itself, where it says. Empty when it does not.
    pub product: String,
    /// USB ports are the ones an instrument is usually on. A built-in COM1 on
    /// a desktop is a port, but rarely the one being looked for.
    pub usb: bool,
}

impl PortOption {
    /// The port as a line somebody picks from a list.
    pub fn label(&self) -> String {
        match self.product.is_empty() {
            true => self.name.clone(),
            false => format!("{} — {}", self.name, self.product),
        }
    }
}

/// Every serial port currently attached.
///
/// USB ports first, since that is where an instrument usually is, and each
/// group in the order the operating system gave them. An empty list is a
/// perfectly ordinary answer: it means nothing is plugged in.
///
/// This asks the operating system, so it is a moment's work rather than free.
/// Call it when a list is about to be shown, not on every redraw.
pub fn available_ports() -> Vec<PortOption> {
    let mut ports: Vec<PortOption> = match serialport::available_ports() {
        Ok(ports) => ports
            .into_iter()
            .map(|port| {
                let (product, usb) = match &port.port_type {
                    serialport::SerialPortType::UsbPort(info) => {
                        // The product name if it gives one, else the maker's
                        // name, else nothing rather than a made-up label.
                        let product = info
                            .product
                            .clone()
                            .or_else(|| info.manufacturer.clone())
                            .unwrap_or_default();
                        (product, true)
                    }
                    _ => (String::new(), false),
                };

                PortOption { name: port.port_name, product, usb }
            })
            .collect(),
        // Nothing to offer is the same answer whether the list was empty or
        // could not be read. A rig is not misconfigured because a port scan
        // failed, and the port can still be typed in.
        Err(_) => Vec::new(),
    };

    ports.sort_by_key(|port| !port.usb);
    ports
}

/// The port to suggest for a device nobody has configured yet.
pub fn first_usb_port() -> Option<String> {
    available_ports().into_iter().find(|port| port.usb).map(|port| port.name)
}

impl Default for SerialStreamConfig {
    /// A device that still has to be told which port it is on.
    ///
    /// The port is left empty rather than guessed at: `COM1` is a real port on
    /// most Windows machines and almost never the right one, so a wrong guess
    /// would be tried and fail confusingly. Empty says plainly that nobody has
    /// said yet.
    fn default() -> SerialStreamConfig {
        SerialStreamConfig {
            port: String::new(),
            baudrate: 115200,
            frame_pattern: default_frame_pattern(),
            channels: vec![],
        }
    }
}

const FIELD_SEPARATOR: char = ',';

/// If this much arrives with no frame terminator in it, something is wrong with
/// the stream and we are just accumulating noise. Better to drop it than to
/// grow without bound for the rest of the run.
const MAX_BUFFER_BYTES: usize = 64 * 1024;

/// How much of what arrives must be frames before a frame that will not read
/// is taken as a channel problem rather than a coincidence.
///
/// At the right baud rate a stream is almost entirely frames. At the wrong one
/// it is noise, and noise throws up the occasional accidental match: `#` and
/// `$` each turn up about once in every 256 random bytes, so a pattern looking
/// for one either side of some text finds one soon enough. Treating that as
/// "the rate is right and a channel is wrong" is exactly how a wrong rate
/// escapes without a warning.
///
/// A quarter is far below what a real stream manages and far above what
/// coincidence produces, so nothing delicate rests on the figure.
const ENOUGH_FRAMES: f64 = 0.25;

/// How long bytes may keep arriving without completing a single frame before
/// the device says something is wrong.
///
/// Measured in time rather than in bytes. Waiting for a bufferful means the
/// complaint arrives when enough rubbish has accumulated, and how long that
/// takes depends on the baud rate, the frame size and how badly the rate is
/// wrong - a minute or more in practice, which is far too late to be the
/// answer to "why is nothing being recorded".
///
/// Only counted against reads that actually returned bytes, so a device that
/// is simply quiet between frames never trips it: the clock is "we are being
/// sent something and none of it is a frame", not "we have not heard anything
/// lately". Three seconds is long enough for any frame to finish arriving at
/// a sane rate and short enough to see before giving up on it.
const UNMATCHED_AFTER: Duration = Duration::from_secs(3);

/// A frame, and when it finished arriving.
struct StampedFrame {
    at: DateTime<Utc>,
    frame: String,
}

/// What the reader thread hands back.
///
/// Two things rather than one channel each, so that what a device said stays
/// in order against the readings it said it between.
enum FromReader {
    Frame(StampedFrame),
    /// A line the device sent that was not a reading: a boot banner, a debug
    /// message, a warning in its own words.
    ///
    /// Worth showing somebody, which is why it travels at all rather than
    /// being dropped where it is found, and never worth recording: it has no
    /// channel to belong to and no value to be.
    Said(String),
}

/// How much of one line a device says is worth keeping.
///
/// Long enough for anything meant to be read, short enough that a device
/// dumping a screenful cannot push everything else out of a log that keeps a
/// few hundred lines.
const MAX_SAID_CHARS: usize = 200;

/// The running device: its settings, and a thread reading the port.
///
/// The port is not held here. It is moved into a reader thread that blocks on
/// it, which is the only way to know when data arrived: nothing on Windows
/// timestamps serial bytes, so the closest available answer is the moment a
/// blocking read wakes up. Draining on a schedule instead would date every
/// frame to when we got round to looking, and the error would be however long
/// that was.
///
/// It also takes the accuracy out of the user's hands. With a drain schedule,
/// setting the interval too slow silently costs timestamp accuracy and lumps
/// frames together; here the timestamps are the same whatever the device
/// thread's interval is, and a slow drain only means more frames per batch.
pub struct SerialStream {
    config: SerialStreamConfig,
    /// The compiled form of `config.frame_pattern`. Compiling is not cheap, so
    /// it happens once here rather than on every read.
    frame_pattern: Regex,
    /// Frames the reader thread has stamped and handed over, and the lines
    /// it found in between them.
    frames: Option<Receiver<FromReader>>,
    /// Asks the reader thread to finish.
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    /// How many bufferfuls the reader has given up on because nothing in them
    /// looked like a frame. Shared with the reader thread, which is the only
    /// place that can see it happen.
    unmatched: Arc<AtomicU64>,
    /// What is wrong with this device, while it is still working.
    ///
    /// State rather than an event, so an interface can show it for as long as
    /// it is true. Refreshed by every read and cleared by the first clean one.
    concern: Option<String>,
    /// Channels already complained about, by name.
    ///
    /// A channel pointed at a field that never holds a number is worth saying
    /// once. Saying it again on every batch for the rest of the run is not: the
    /// complaint has not changed, and a channel that alternates between a
    /// number and `LO` would otherwise raise and clear the same one all day.
    ///
    /// Per connection, like `unmatched` below it - what the last connection
    /// could not read says nothing about this one.
    reported: HashSet<String>,
    /// Lines the device sent that were not readings, waiting to be collected.
    ///
    /// Held rather than sent straight on, because the reader thread has no way
    /// to reach a log and this is the only place both it and the caller can
    /// see. Emptied by `said`, so each line is reported once.
    said: Vec<String>,
}

impl SerialStream {
    pub fn new(port: String, baudrate: u32) -> Result<SerialStream> {
        SerialStream::from_config(SerialStreamConfig {
            port: port,
            baudrate: baudrate,
            frame_pattern: default_frame_pattern(),
            channels: vec![],
        })
    }

    /// Compiling the pattern here means a config with a bad expression is
    /// rejected when the setup is built, rather than on the first read.
    pub fn from_config(config: SerialStreamConfig) -> Result<SerialStream> {
        let frame_pattern = Regex::new(&config.frame_pattern).map_err(|error| {
            Error::InvalidFramePattern {
                pattern: config.frame_pattern.clone(),
                port: config.port.clone(),
                source: error,
            }
        })?;
        Ok(SerialStream {
            config: config,
            frame_pattern: frame_pattern,
            frames: None,
            stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            unmatched: Arc::new(AtomicU64::new(0)),
            concern: None,
            reported: HashSet::new(),
            said: Vec::new(),
        })
    }

    pub fn config(&self) -> SerialStreamConfig {
        self.config.clone()
    }

    pub fn add_channel(&mut self, channel: SerialStreamChannel) {
        self.config.channels.push(channel);
    }
}

impl SerialStream {
    /// Ask the reader thread to finish, and wait for it.
    ///
    /// Dropping the receiver as well, so a thread that has already exited on a
    /// dead port does not leave a channel behind that looks connected.
    fn stop_reader(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.frames = None;
        if let Some(reader) = self.reader.take() {
            // The thread checks the flag between reads, so this waits at most
            // one port timeout.
            let _ = reader.join();
        }
    }
}

impl DeviceInterface for SerialStream {
    fn connect(&mut self) -> Result<()> {
        // Any previous reader owns a port that is about to be replaced, and a
        // buffer holding bytes from before the outage. Stopping it discards
        // both. Carrying that buffer across a reconnection would join half a
        // frame from before to half from after, and the result would parse
        // perfectly: a plausible reading that never happened.
        self.stop_reader();

        let port = serialport::new(&self.config.port, self.config.baudrate)
            .timeout(Duration::from_millis(100))
            .open()?;

        // A fresh flag rather than clearing the old one, so a thread that has
        // not noticed it should stop cannot be revived by this.
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let pattern = self.frame_pattern.clone();
        let thread_stop = Arc::clone(&stop);
        // Counted per connection, like the flag above: what the last one made
        // of the stream says nothing about this one.
        let unmatched = Arc::new(AtomicU64::new(0));
        let thread_unmatched = Arc::clone(&unmatched);

        let reader = std::thread::spawn(move || {
            read_frames(port, pattern, sender, thread_stop, thread_unmatched);
        });

        self.unmatched = unmatched;
        self.concern = None;
        self.reported.clear();
        // What the last connection said has been said. Carrying it across
        // would report it a second time under a fresh connection.
        self.said.clear();
        self.stop = stop;
        self.frames = Some(receiver);
        self.reader = Some(reader);
        Ok(())
    }
}

/// Read the port until told to stop, handing over each frame as it completes.
///
/// The blocking read is the point. It wakes when bytes arrive, so the time
/// taken immediately after is as close to the arrival time as anything
/// available without driver support, rather than being however late the next
/// scheduled look happened to be.
///
/// A frame is stamped when the read that *completed* it returned, which is
/// when the device finished sending it.
fn read_frames(
    mut port: Box<dyn serialport::SerialPort + Send>,
    pattern: Regex,
    sender: Sender<FromReader>,
    stop: Arc<AtomicBool>,
    unmatched: Arc<AtomicU64>,
) {
    let mut buffer = String::new();
    let mut bytes = [0u8; 4096];
    // Nothing has arrived yet, so the clock starts now rather than at some
    // moment in the past that would complain before anything had a chance.
    let mut last_frame = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let count = match port.read(&mut bytes) {
            Ok(0) => continue,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
            // The port has gone. Ending the thread drops the sender, which is
            // how the device finds out: its next read sees the channel closed.
            Err(_) => return,
        };
        let at = Utc::now();

        // The frame format is ASCII, so a lossy conversion is safe here.
        buffer.push_str(&String::from_utf8_lossy(&bytes[..count]));

        // Every frame completed by this one read shares its arrival time, and
        // two readings at the same instant are not a series: nothing can order
        // them, plot them or interpolate between them. When that happens there
        // is also no way to know when the earlier one arrived, since both were
        // already sitting in the operating system's buffer. So keep the newest
        // and drop the rest, which is the value that was current at `at`.
        //
        // This is the old take-the-latest behaviour, but applied per read
        // rather than per drain. Per drain it discarded most of the data,
        // because a drain can be seconds long. A read spans the moment bytes
        // arrived, so in practice it discards nothing: measured against the
        // real device, 2.3% of reads completed more than one frame.
        let mut newest: Option<String> = None;
        while let Some(taken) = take_next_frame(&mut buffer, &pattern) {
            // Sent whether or not the frame behind it is the one kept. What a
            // device said does not stop being worth reading because a newer
            // reading arrived in the same breath.
            //
            // Only ever reached by way of a frame that matched, which is what
            // keeps a wrong baud rate quiet: nothing matches, so nothing is
            // skipped to reach it, and the counting below stays the whole of
            // the complaint. Chatter is what a *working* stream says in among
            // its readings.
            for line in said_lines(&taken.skipped) {
                if sender.send(FromReader::Said(line)).is_err() {
                    return; // nobody is listening any more
                }
            }
            newest = Some(taken.frame);
        }
        if let Some(frame) = newest {
            last_frame = Instant::now();
            let stamped = StampedFrame { at: at, frame: frame };
            if sender.send(FromReader::Frame(stamped)).is_err() {
                return; // nobody is listening any more
            }
        } else if last_frame.elapsed() > UNMATCHED_AFTER {
            // Bytes keep arriving and none of them are finishing a frame.
            // That is what a wrong baud rate looks like from in here, and it
            // is the one failure nobody could see from outside: the port
            // opens, the device looks connected, and nothing is ever recorded.
            unmatched.fetch_add(1, Ordering::Relaxed);
            // Restarted so this is one complaint per window rather than one
            // per read, which at a hundred reads a second is a different
            // thing entirely.
            last_frame = Instant::now();
        }

        // Whatever is in there is not becoming frames. Dropping it rather than
        // growing all run - the complaint above has already been made, so this
        // is only about memory.
        if buffer.len() > MAX_BUFFER_BYTES {
            buffer.clear();
        }
    }
}

impl Drop for SerialStream {
    fn drop(&mut self) {
        self.stop_reader();
    }
}


/// Pull the *first* complete frame out of the buffer, discarding any leading
/// noise and leaving everything after it in place.
///
/// This used to take the last frame and throw the rest away, which quietly lost
/// data whenever the device sent faster than we looked. Now that a thread reads
/// the port continuously there is no reason to drop any: taking them in turn
/// keeps every frame, in order, each with its own arrival time.
fn take_next_frame(buffer: &mut String, pattern: &Regex) -> Option<TakenFrame> {
    // Work out what to keep before touching the buffer, so the borrow the regex
    // holds on it has ended by the time we drain.
    let (consumed_to, skipped, frame) = {
        let captures = pattern.captures(buffer.as_str())?;
        let whole = captures.get(0)?;
        // Group 1 is the data if the pattern names one, otherwise the whole
        // match is, which lets simple patterns skip the parentheses.
        let frame = captures.get(1).unwrap_or(whole).as_str().to_string();
        (whole.end(), buffer[..whole.start()].to_string(), frame)
    };
    // Everything up to the end of that match is dealt with. What follows may be
    // further complete frames or the start of one still arriving; either way it
    // stays for the next call.
    buffer.drain(..consumed_to);
    Some(TakenFrame { skipped: skipped, frame: frame })
}

/// A frame taken out of the buffer, and whatever sat in front of it.
struct TakenFrame {
    /// What was discarded to reach the frame: the line endings between frames,
    /// and anything the device said in among them.
    ///
    /// Handed back rather than dropped here, because this is the only place
    /// that ever sees it. It used to go in the bin, which is why a device's
    /// own messages were invisible however carefully somebody watched.
    skipped: String,
    frame: String,
}

/// The lines worth reporting out of text that was skipped to reach a frame.
///
/// Usually none. What sits between two frames is a line ending and nothing
/// else, and reporting that would fill a log with blank lines. What survives
/// the trimming is what the device actually said.
fn said_lines(skipped: &str) -> Vec<String> {
    skipped
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| match line.chars().count() > MAX_SAID_CHARS {
            // Counted in characters rather than bytes: truncating a UTF-8
            // string by bytes can land in the middle of one, and `String`
            // will not hold the result.
            true => line.chars().take(MAX_SAID_CHARS).collect::<String>() + "...",
            false => line.to_string(),
        })
        .collect()
}

/// Read one frame, once per configured channel.
///
/// The returned Vec is in the same order as `channels`, because `Device::read`
/// pairs it against the device's channels positionally. Both come from this one
/// list, so they cannot fall out of step.
///
/// A result per channel rather than one for the whole frame. The channels are
/// independent, so a field that will not read says nothing about the field next
/// to it: `241.9, 0.085, 0, 0, 0, LO, 0, 4, 0` has eight numbers in it and one
/// status word. Failing the frame on the first unreadable field threw away all
/// eight, which is a hole across every channel rather than the one missing
/// value the stream actually justifies.
///
/// Values rather than datapoints, because one frame contributes one value to
/// each channel and the caller holds the timestamp that applies to all of them.
fn parse_frame_values(frame: &str, channels: &[SerialStreamChannel]) -> Vec<Result<f64>> {
    // Trimmed because a device is as entitled to write `241.9, 0.085` as it is
    // to write `241.9,0.085`, and they mean the same thing.
    let fields: Vec<&str> = frame.split(FIELD_SEPARATOR).map(|field| field.trim()).collect();

    channels
        .iter()
        .map(|channel| {
            let position = usize::try_from(channel.index).map_err(|_| {
                Error::NegativeChannelIndex {
                    channel: channel.info.name.clone(),
                    index: channel.index,
                }
            })?;
            let field = fields.get(position).ok_or_else(|| Error::FrameTooShort {
                channel: channel.info.name.clone(),
                index: channel.index,
                fields: fields.len(),
                frame: frame.to_string(),
            })?;
            field.parse().map_err(|_| Error::FieldNotNumeric {
                channel: channel.info.name.clone(),
                index: channel.index,
                field: field.to_string(),
                frame: frame.to_string(),
            })
        })
        .collect()
}

impl HardwareDataAcquisition for SerialStream {
    /// Take every frame the reader thread has handed over since last time.
    ///
    /// Nothing is discarded and nothing waits: an empty result means no frame
    /// finished arriving since the last call, which is normal when draining
    /// faster than the device sends.
    fn read(&mut self) -> Result<Vec<Vec<DataPoint>>> {
        let frames = match &self.frames {
            Some(frames) => frames,
            None => return Err(Error::NotConnected { port: self.config.port.clone() }),
        };

        let mut stamped: Vec<StampedFrame> = Vec::new();
        // Collected here rather than pushed straight onto `self.said`, which
        // the borrow above rules out: `frames` is borrowed from self for as
        // long as this loop runs.
        let mut said: Vec<String> = Vec::new();
        let mut reader_gone = false;
        loop {
            match frames.try_recv() {
                Ok(FromReader::Frame(frame)) => stamped.push(frame),
                Ok(FromReader::Said(line)) => said.push(line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    reader_gone = true;
                    break;
                }
            }
        }
        // Before the check below, so that a device whose last act was to say
        // why it was going is still heard saying it.
        self.said.append(&mut said);

        // The reader only ends on a dead port. Report that, but not before
        // handing over what it managed to read first: the next call will find
        // the channel closed and empty, and say so then.
        if reader_gone && stamped.is_empty() {
            return Err(Error::NotConnected { port: self.config.port.clone() });
        }

        let mut readings: Vec<Vec<DataPoint>> = vec![Vec::new(); self.config.channels.len()];
        // Which channels could not read their field, and the first reason each
        // gave. Keyed by channel so a batch of a thousand frames complains
        // about a status field once rather than a thousand times, and ordered
        // so the same batch always produces the same complaint.
        let mut unreadable: BTreeMap<usize, String> = BTreeMap::new();

        for frame in stamped.iter() {
            let outcomes = parse_frame_values(&frame.frame, &self.config.channels);
            for (index, outcome) in outcomes.into_iter().enumerate() {
                match outcome {
                    Ok(value) => {
                        readings[index].push(DataPoint { datetime: frame.at, value });
                    }
                    // Only this channel loses this sample; the rest of the
                    // frame is good and is kept. A device that reports `LO` in
                    // one field should cost that one channel one reading, not
                    // punch a hole across every channel it sends.
                    Err(error) => {
                        unreadable.entry(index).or_insert_with(|| error.to_string());
                    }
                }
            }
        }

        self.concern = self.what_is_wrong(&unreadable, stamped.len());
        Ok(readings)
    }

    /// What this device should be complaining about, if anything.
    ///
    /// Kept as state that stands until it is contradicted, so an interface can
    /// show it for as long as it is true rather than having to catch an event
    /// as it goes past.
    fn concern(&self) -> Option<String> {
        self.concern.clone()
    }

    /// Lines the device sent that were not readings.
    ///
    /// Taken rather than read, unlike `concern` above it: these are things
    /// that happened rather than a state that holds, so each is handed over
    /// once and then gone.
    fn said(&mut self) -> Vec<String> {
        std::mem::take(&mut self.said)
    }
}

impl SerialStream {
    /// Judge one read: what could not be read in it, and what the reader made
    /// of the bytes it could not use.
    ///
    /// Each channel is complained about once. The complaint stands while the
    /// channel keeps failing, so an interface still shows what is wrong, but it
    /// is only ever *raised* once: a status field that never holds a number is
    /// not news after the first batch, and one that holds a number half the
    /// time would otherwise raise and clear the same complaint all day.
    ///
    /// Takes `&mut self` for that reason - remembering what has already been
    /// said is the whole of it.
    fn what_is_wrong(
        &mut self,
        unreadable: &BTreeMap<usize, String>,
        frames: usize,
    ) -> Option<String> {
        // Taken rather than read, so each bufferful of noise is counted once.
        let unmatched = self.unmatched.swap(0, Ordering::Relaxed);

        // Note every channel that failed, and keep the reason from the first
        // one not mentioned before. All of them are noted whether or not they
        // are the one reported, so each channel costs one complaint rather than
        // one per batch until they have each had a turn.
        let mut fresh: Option<String> = None;
        for (index, reason) in unreadable.iter() {
            let name = match self.config.channels.get(*index) {
                Some(channel) => channel.info.name.clone(),
                None => continue,
            };
            // `insert` answers true the first time this channel is seen.
            if self.reported.insert(name) && fresh.is_none() {
                fresh = Some(reason.clone());
            }
        }

        match (fresh, unreadable.is_empty(), unmatched, frames) {
            // A channel that has not been mentioned before cannot read its
            // field. This is the one time it gets said.
            (Some(reason), _, _, _) => {
                Some(format!("dropping the samples it cannot read: {}", reason))
            }
            // Or bytes are arriving and none of them are frames at all, which
            // is what a wrong baud rate looks like from here. It is also what
            // a wrong frame pattern looks like, so say both.
            (None, _, 1.., _) => Some(format!(
                "reading {} but nothing matches the frame pattern - check the baud rate",
                self.config.port
            )),
            // Channels still failing, all of them already mentioned. What was
            // said stands - it is still true - but saying it again is the
            // noise this is here to avoid.
            (None, false, 0, _) => self.concern.clone(),
            // A batch that arrived and read cleanly settles it.
            (None, true, 0, 1..) => None,
            // An empty batch says nothing either way: it is what draining
            // faster than the device sends looks like. Leave the last answer
            // standing rather than treating silence as good news.
            (None, true, 0, 0) => self.concern.clone(),
        }
    }
}

pub fn create_device(name: String, port: String, baudrate: u32) -> Result<Device> {
    let hardware = SerialStream::new(port, baudrate)?;
    Ok(Device::new(name, Hardware::SerialStream(hardware)))
}

pub fn add_channel(device: &mut Device, name: String, index: i64, unit: String) -> Result<()> {
    match &mut device.hardware {
        Hardware::SerialStream(hardware) => {
            hardware.add_channel(SerialStreamChannel {
                info: ChannelInfo {
                    name: name,
                    unit: unit,
                scale: None,
                    enabled: true,
                },
                index: index,
            });
        },
        _ => {
            return Err(Error::WrongHardwareType { expected: "serial stream".to_string() })
        }
    }
    // The hardware config is the definition; the device mirrors it.
    device.rebuild_channels()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example frame from the device documentation above.
    const EXAMPLE: &str = "1,2.00,0,1,1,STBY,0,1,0";

    fn line_inputs(indices: &[i64]) -> Vec<SerialStreamChannel> {
        indices.iter().map(|index| SerialStreamChannel {
            info: ChannelInfo {
                name: format!("Channel {}", index),
                unit: "-".to_string(),
            scale: None,
                enabled: true,
            },
            index: *index,
        }).collect()
    }

    /// The values from a frame that is expected to read completely.
    fn read_values(frame: &str, indices: &[i64]) -> Vec<f64> {
        parse_frame_values(frame, &line_inputs(indices))
            .into_iter()
            .map(|outcome| outcome.expect("every field here is a number"))
            .collect()
    }

    #[test]
    fn reads_the_configured_indices_in_order() {
        assert_eq!(read_values(EXAMPLE, &[1, 3]), vec![2.00, 1.0]);
    }

    /// Plenty of devices space their fields out: `241.9, 0.085, 0` is the same
    /// frame as `241.9,0.085,0` and has to read as one.
    #[test]
    fn a_space_after_the_comma_is_not_part_of_the_value() {
        let spaced = "241.9, 0.085, 0, 0, 0, LO, 0, 4, 0";
        assert_eq!(read_values(spaced, &[0, 1, 7]), vec![241.9, 0.085, 4.0]);
    }

    /// The whole of it, as the device sends it: markers stripped by the
    /// pattern, spaces by the parse.
    #[test]
    fn a_spaced_frame_reads_the_same_as_a_tight_one() {
        let mut buffer = String::from("#241.9, 0.085, 0, 0, 0, LO, 0, 4, 0$");
        let frame = next_frame(&mut buffer, &default_pattern()).expect("one whole frame");
        assert_eq!(read_values(&frame, &[0, 1]), vec![241.9, 0.085]);
    }

    #[test]
    fn a_non_numeric_field_fails_its_own_channel_and_no_other() {
        // Index 5 is "STBY". The channels either side of it are numbers and
        // have nothing wrong with them, so they still read.
        let outcomes = parse_frame_values(EXAMPLE, &line_inputs(&[1, 5, 3]));

        assert_eq!(*outcomes[0].as_ref().expect("index 1 is a number"), 2.00);
        assert_eq!(*outcomes[2].as_ref().expect("index 3 is a number"), 1.0);

        let error = outcomes[1].as_ref().expect_err("index 5 is STBY");
        assert!(error.to_string().contains("STBY"), "{}", error);
    }

    #[test]
    fn an_index_past_the_end_of_the_frame_fails_only_that_channel() {
        let outcomes = parse_frame_values(EXAMPLE, &line_inputs(&[1, 99]));
        assert_eq!(*outcomes[0].as_ref().expect("index 1 is a number"), 2.00);
        assert!(outcomes[1].is_err());
    }

    fn default_pattern() -> Regex {
        Regex::new(&default_frame_pattern()).unwrap()
    }

    /// The frame alone, for the tests that are not about what preceded it.
    fn next_frame(buffer: &mut String, pattern: &Regex) -> Option<String> {
        take_next_frame(buffer, pattern).map(|taken| taken.frame)
    }

    /// The device's own messages, which used to be dropped on the way to the
    /// next frame and so could not be seen at all.
    #[test]
    fn what_a_device_says_before_a_frame_is_handed_back() {
        let mut buffer = String::from("ADC calibrated\r\n#1,2.00$");
        let taken = take_next_frame(&mut buffer, &default_pattern()).expect("a frame");

        assert_eq!(taken.frame, "1,2.00");
        assert_eq!(said_lines(&taken.skipped), vec!["ADC calibrated".to_string()]);
    }

    /// What sits between two frames is a line ending and nothing else. A log
    /// with a blank line in it for every reading would be no log at all.
    #[test]
    fn the_gap_between_two_frames_is_not_worth_reporting() {
        let mut buffer = String::from("#1,2.00$\r\n#1,3.00$");
        let first = take_next_frame(&mut buffer, &default_pattern()).expect("a frame");
        let second = take_next_frame(&mut buffer, &default_pattern()).expect("another");

        assert!(said_lines(&first.skipped).is_empty());
        assert!(said_lines(&second.skipped).is_empty(), "a line ending is not a message");
        assert_eq!(second.frame, "1,3.00");
    }

    #[test]
    fn several_lines_at_once_are_reported_one_by_one() {
        let skipped = "starting up\r\nrange set to 20 mA\r\n";
        assert_eq!(said_lines(skipped), vec![
            "starting up".to_string(),
            "range set to 20 mA".to_string(),
        ]);
    }

    /// A device dumping a screenful should not push everything else out of a
    /// log that keeps a few hundred lines.
    #[test]
    fn a_very_long_line_is_cut_short() {
        let long = "x".repeat(MAX_SAID_CHARS + 50);
        let said = said_lines(&long);

        assert_eq!(said.len(), 1);
        assert!(said[0].ends_with("..."), "it should say it was cut");
        assert_eq!(said[0].chars().count(), MAX_SAID_CHARS + 3);
    }

    /// The whole reason the wrong baud rate stays quiet. Nothing is skipped
    /// except to reach a frame, so a stream where nothing matches has nothing
    /// to say - it has a concern instead, which is the honest answer.
    #[test]
    fn a_stream_with_no_frames_in_it_says_nothing() {
        let noise = "\u{fffd}\u{fffd}garbage at the wrong rate\u{fffd}\u{fffd}";
        let mut buffer = String::from(noise);
        assert!(take_next_frame(&mut buffer, &default_pattern()).is_none());
        assert_eq!(buffer, noise, "and none of it is thrown away either");
    }

    #[test]
    fn what_a_device_says_is_collected_by_reading_and_handed_over_once() {
        let mut device = unattached(&[0, 1]);
        let (sender, receiver) = mpsc::channel();
        sender.send(FromReader::Said("ADC calibrated".to_string())).expect("receiver is here");
        let stamped = StampedFrame { at: Utc::now(), frame: "1,2".to_string() };
        sender.send(FromReader::Frame(stamped)).expect("receiver is here");
        device.frames = Some(receiver);
        let _sender = sender;

        let readings = device.read().expect("a successful read");
        assert_eq!(readings[0].len(), 1, "a line of chatter is not a reading");
        assert!(device.concern().is_none(), "a device that talks is not a device in trouble");

        assert_eq!(device.said(), vec!["ADC calibrated".to_string()]);
        assert!(device.said().is_empty(), "handed over once, and then gone");
    }

    /// The behaviour this replaced took the last frame and dropped the rest.
    /// Every frame now comes out, in order, and the partial one is kept.
    #[test]
    fn frames_come_out_in_order_and_none_are_dropped() {
        let mut buffer = String::from("#1,2.00$#1,3.00$#1,4.0");
        let pattern = default_pattern();
        assert_eq!(next_frame(&mut buffer, &pattern).unwrap(), "1,2.00");
        assert_eq!(next_frame(&mut buffer, &pattern).unwrap(), "1,3.00");
        assert!(next_frame(&mut buffer, &pattern).is_none());
        assert_eq!(buffer, "#1,4.0");
    }

    #[test]
    fn an_incomplete_frame_yields_nothing_and_is_kept() {
        let mut buffer = String::from("#1,2.0");
        assert!(next_frame(&mut buffer, &default_pattern()).is_none());
        assert_eq!(buffer, "#1,2.0");
    }

    #[test]
    fn a_frame_split_across_two_reads_is_rejoined() {
        let mut buffer = String::from("#1,2.");
        assert!(next_frame(&mut buffer, &default_pattern()).is_none());
        buffer.push_str("00,3$");
        assert_eq!(next_frame(&mut buffer, &default_pattern()).unwrap(), "1,2.00,3");
    }

    #[test]
    fn noise_before_a_frame_is_discarded() {
        let mut buffer = String::from("garbage#1,2.00$");
        assert_eq!(next_frame(&mut buffer, &default_pattern()).unwrap(), "1,2.00");
    }

    /// A device with no framing characters at all, just newline terminated
    /// lines. This is the common case the default pattern does not cover.
    #[test]
    fn a_newline_terminated_device_needs_only_a_different_pattern() {
        let pattern = Regex::new(r"([^\r\n]+)\r?\n").unwrap();
        let mut buffer = String::from("1,2.00\r\n1,3.00\r\n1,4.0");
        assert_eq!(next_frame(&mut buffer, &pattern).unwrap(), "1,2.00");
        assert_eq!(next_frame(&mut buffer, &pattern).unwrap(), "1,3.00");
        assert_eq!(buffer, "1,4.0");
    }

    /// Without a capture group the whole match is the data, so a pattern that
    /// needs no cleaning can skip the parentheses.
    #[test]
    fn a_pattern_without_a_capture_group_uses_the_whole_match() {
        let pattern = Regex::new(r"[0-9.,]+;").unwrap();
        let mut buffer = String::from("1,2.00;1,3.00;");
        assert_eq!(next_frame(&mut buffer, &pattern).unwrap(), "1,2.00;");
    }

    /// Devices that wrap data in something more than one character, here a
    /// checksum that should not reach the parser.
    #[test]
    fn a_pattern_can_strip_more_than_delimiters() {
        let pattern = Regex::new(r"\$DATA,([^*]*)\*[0-9A-F]{2}\r\n").unwrap();
        let mut buffer = String::from("$DATA,1,2.00,3*7F\r\n");
        assert_eq!(next_frame(&mut buffer, &pattern).unwrap(), "1,2.00,3");
    }

    /// A failed connection must leave nothing behind that looks connected: no
    /// reader thread, and a read that says so rather than waiting on a channel
    /// nobody will send to.
    #[test]
    fn a_failed_connection_leaves_no_reader() {
        let mut stream = SerialStream::new("no-such-port".to_string(), 115200).unwrap();
        assert!(stream.connect().is_err());
        assert!(stream.reader.is_none());
        assert!(stream.frames.is_none());
        assert!(matches!(stream.read(), Err(Error::NotConnected { .. })));
    }

    /// Bytes from before an outage live in the reader thread's buffer, and it
    /// is stopped and replaced on connect, so a partial frame from before can
    /// never be joined to bytes from after. This checks the stopping, which is
    /// what makes that true.
    #[test]
    fn connecting_stops_any_previous_reader() {
        let mut stream = SerialStream::new("no-such-port".to_string(), 115200).unwrap();
        let first_flag = Arc::clone(&stream.stop);
        assert!(stream.connect().is_err());
        // The old flag is set, so a thread still holding it would finish.
        assert!(first_flag.load(Ordering::Relaxed));
    }

    /// Configs written before frame_pattern existed must still load.
    #[test]
    fn a_config_without_a_pattern_falls_back_to_the_default() {
        let json = r#"{
            "description": "Older config",
            "port": "COM3",
            "baudrate": 115200,
            "channels": []
        }"#;
        let config: SerialStreamConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.frame_pattern, default_frame_pattern());
    }

    /// A channel reads as one flat object: what it is, and where it comes from.
    #[test]
    fn a_channel_carries_its_description_and_its_binding_together() {
        let json = r#"{
            "id": "1",
            "name": "Pressure",
            "unit": "Pa",
            "description": "Differential pressure sensor",
            "index": 1
        }"#;
        let channel: SerialStreamChannel = serde_json::from_str(json).unwrap();
        assert_eq!(channel.info.name, "Pressure");
        assert_eq!(channel.index, 1);
    }

    /// The failure the merge exists to prevent: reordering used to swap which
    /// channel each value landed in. Now the name travels with the index.
    #[test]
    fn reordering_channels_moves_their_bindings_with_them() {
        let forwards = read_values(EXAMPLE, &[1, 3]);
        let backwards = read_values(EXAMPLE, &[3, 1]);
        assert_eq!(forwards[0], backwards[1]);
        assert_eq!(forwards[1], backwards[0]);
    }

    /// A device with two channels and nothing connected to it, ready to be
    /// handed frames by hand.
    fn unattached(indices: &[i64]) -> SerialStream {
        SerialStream::from_config(SerialStreamConfig {
            port: "COM_TEST".to_string(),
            baudrate: 9600,
            frame_pattern: default_frame_pattern(),
            channels: line_inputs(indices),
        })
        .expect("the default pattern compiles")
    }

    /// Put frames in front of a device as though a reader thread had. The
    /// sender is returned so it stays alive: dropping it would look like the
    /// reader ending, which is a different thing entirely.
    fn hand_over(device: &mut SerialStream, frames: &[&str]) -> Sender<FromReader> {
        let (sender, receiver) = mpsc::channel();
        for frame in frames {
            let stamped = StampedFrame { at: Utc::now(), frame: frame.to_string() };
            sender.send(FromReader::Frame(stamped)).expect("the receiver is right here");
        }
        device.frames = Some(receiver);
        sender
    }

    #[test]
    fn a_frame_the_channels_cannot_read_is_skipped_and_the_rest_are_kept() {
        // The whole point of skipping: a status line in the middle of a
        // recording costs that one frame, not the batch around it.
        let mut device = unattached(&[0, 1]);
        let _sender = hand_over(&mut device, &["1,2", "STBY,none", "3,4"]);

        let readings = device.read().expect("a frame it cannot read is not a failed read");

        assert_eq!(readings[0].len(), 2, "both readable frames should survive");
        assert_eq!(readings[0][0].value, 1.0);
        assert_eq!(readings[0][1].value, 3.0);
        assert_eq!(readings[1][1].value, 4.0);
    }

    /// What the frame-at-a-time version got wrong: one field it could not read
    /// threw away the whole frame, so a device reporting `LO` in field two put
    /// a hole in field one as well. Only the field that cannot be read is lost.
    #[test]
    fn one_unreadable_field_does_not_cost_the_channels_beside_it() {
        let mut device = unattached(&[0, 1]);
        let _sender = hand_over(&mut device, &["1,LO", "3,4"]);

        let readings = device.read().expect("a field it cannot read is not a failed read");

        assert_eq!(readings[0].len(), 2, "the good field of a mixed frame survives");
        assert_eq!(readings[0][0].value, 1.0);
        assert_eq!(readings[0][1].value, 3.0);

        assert_eq!(readings[1].len(), 1, "only the channel on LO loses a sample");
        assert_eq!(readings[1][0].value, 4.0);
    }

    #[test]
    fn dropping_a_sample_is_something_the_device_says_rather_than_swallows() {
        // Dropping silently would turn a misconfigured index into no data and
        // no explanation, which is worse than the error it replaced.
        let mut device = unattached(&[0, 1]);
        let _sender = hand_over(&mut device, &["1,2", "STBY,none"]);

        device.read().expect("still a successful read");
        let concern = device.concern().expect("it should have something to say");
        assert!(concern.contains("dropping"), "{}", concern);
        assert!(concern.contains("STBY"), "{}", concern);
    }

    /// Said once. A channel pointed at a status field would otherwise complain
    /// on every batch for as long as the run lasts, and one that alternates
    /// between a number and `LO` would raise and clear the same complaint all
    /// day - which in an interface is a warning that will not sit still.
    #[test]
    fn a_channel_is_complained_about_once_and_then_left_alone() {
        let mut device = unattached(&[0, 1]);

        let _first = hand_over(&mut device, &["1,LO"]);
        device.read().expect("a successful read");
        let raised = device.concern().expect("the first one is worth saying");
        assert!(raised.contains("LO"), "{}", raised);

        // Still failing, and already mentioned: the complaint stands rather
        // than being renewed, so an interface has something to show.
        let _still = hand_over(&mut device, &["2,LO"]);
        device.read().expect("a successful read");
        assert_eq!(device.concern(), Some(raised));

        // A clean batch settles it, as it always did.
        let _clean = hand_over(&mut device, &["1,2"]);
        device.read().expect("a successful read");
        assert!(device.concern().is_none(), "a clean batch should clear it");

        // And the same channel failing again is not news.
        let _again = hand_over(&mut device, &["1,LO"]);
        device.read().expect("a successful read");
        assert!(device.concern().is_none(), "the same complaint is not raised twice");
    }

    /// Each channel gets its own turn, though. Two channels failing is two
    /// different things wrong, and the second is not covered by the first.
    #[test]
    fn a_second_channel_going_wrong_is_still_worth_saying() {
        let mut device = unattached(&[0, 1]);

        let _first = hand_over(&mut device, &["1,LO"]);
        device.read().expect("a successful read");
        let about_one = device.concern().expect("the first one is worth saying");

        let _second = hand_over(&mut device, &["HI,LO"]);
        device.read().expect("a successful read");
        let about_both = device.concern().expect("the second one is too");
        assert_ne!(about_both, about_one, "a different channel is a different complaint");
        assert!(about_both.contains("HI"), "{}", about_both);
    }

    #[test]
    fn a_clean_batch_settles_it_but_an_empty_one_says_nothing() {
        let mut device = unattached(&[0, 1]);

        let _bad = hand_over(&mut device, &["STBY,none"]);
        device.read().expect("a successful read");
        assert!(device.concern().is_some());

        // Nothing arrived. That is what draining faster than the device sends
        // looks like, so the last answer should stand.
        let _quiet = hand_over(&mut device, &[]);
        device.read().expect("a successful read");
        assert!(device.concern().is_some(), "silence is not good news");

        // A batch that read cleanly is.
        let _good = hand_over(&mut device, &["1,2"]);
        device.read().expect("a successful read");
        assert!(device.concern().is_none(), "a clean batch should clear it");
    }

    #[test]
    fn bytes_that_never_form_a_frame_are_blamed_on_the_baud_rate() {
        // What a wrong baud rate looks like from in here: the reader throwing
        // away bufferfuls, and not one frame to show for any of it.
        let mut device = unattached(&[0, 1]);
        let _sender = hand_over(&mut device, &[]);
        device.unmatched.store(1, Ordering::Relaxed);

        device.read().expect("reading nothing is not a failure");
        let concern = device.concern().expect("it should have something to say");
        assert!(concern.contains("baud rate"), "{}", concern);

        // Counted once. The complaint stands, but on the strength of the same
        // bufferful rather than being renewed by it.
        assert_eq!(device.unmatched.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn frames_that_will_not_read_blame_the_channels_only_when_the_stream_is_frames() {
        // The right rate: nearly everything that arrives is a frame, so a
        // frame the channels cannot read is about the channels.
        assert_eq!(
            verdict(1000, 900, Some("field 3 is not a number".to_string())),
            StreamCheck::Mismatched { reason: "field 3 is not a number".to_string() }
        );
    }

    #[test]
    fn one_lucky_match_in_a_sea_of_noise_still_blames_the_rate() {
        // The wrong rate, and the case that got through the first time: `#`
        // and `$` turn up in noise often enough that the pattern eventually
        // matches something, and that match proves nothing.
        assert_eq!(
            verdict(60_000, 40, Some("field 3 is not a number".to_string())),
            StreamCheck::Unreadable
        );
    }

    #[test]
    fn bytes_that_never_match_are_the_rate_and_no_bytes_at_all_are_neither() {
        assert_eq!(verdict(60_000, 0, None), StreamCheck::Unreadable);
        // Nothing arrived, which says nothing about the settings: the device
        // may simply have nothing to say yet.
        assert_eq!(verdict(0, 0, None), StreamCheck::Silent);
    }

    #[test]
    fn a_bad_pattern_is_rejected_when_the_device_is_built() {
        let config = SerialStreamConfig {
            port: "COM1".to_string(),
            baudrate: 9600,
            frame_pattern: r"#([$".to_string(),
            channels: vec![],
        };
        let error = SerialStream::from_config(config).err().unwrap();
        assert!(error.to_string().contains("not a valid regular expression"));
    }
}
