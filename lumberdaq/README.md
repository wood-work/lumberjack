# Lumberdaq
Rust library for data acquisition. 

## Install

```cmd
cargo build
```

Nothing else, whatever hardware you own. Vendor drivers are looked up at run
time rather than linked, so this builds on a machine with none of them
installed and the mock device runs with nothing attached at all.

A driver is needed only to read the hardware that needs it: PicoSDK for an
ADC-20 or ADC-24, the NI-DAQmx runtime for a USB-6001. Which components, and
what to leave out of the NI installer, is in the [repository README](../README.md#drivers).

A device that cannot find its driver says so when it connects, naming what it
looked for, and the rest of the setup carries on recording without it.


## Running

A *project* is a directory holding `config.json`, which describes the devices and
channels, and the results recorded from them. Nothing about a rig is compiled in,
so changing a channel means editing that file, not rebuilding.

Run one by pointing at its directory:

```cmd
cargo run -- test_projects/simulated_and_serial_devices
```

Everything after `--` goes to the program rather than to cargo. With no argument
it uses the current directory, so from inside a project:

```cmd
cd test_projects/simulated_and_serial_devices
cargo run
```

Recording continues until **Ctrl-C**, which stops the run tidily and flushes
whatever is still buffered. Killing the process instead loses that.

```cmd
cargo run -- --help
```

### Checking a setup without running it

```cmd
cargo run -- check test_projects/scaled
```

Builds everything a recording would build and reports what is wrong, without
connecting to any hardware or writing a results file. Useful away from the rig,
and before a run that matters.

Problems are collected a part at a time rather than stopping at the first, so
three misconfigured devices take one run of this to find rather than three:

```
    ok    device 'Good rig', 1 channel(s)
    FAIL  device 'Bad scale'
          the scale for 'Flow' uses 'v', which it has no value for. Available: x (the measurement). Scale: v * 2
    FAIL  device 'Bad pico'
          channel 2 cannot be differential: a differential input pairs a channel with the one above it,
          so the first of the pair must be odd. Use channel 1 to measure between 1 and 2
    FAIL  calculated channel 'Missing source'
          'Missing source' reads Ghost/Nothing, which no device provides
```

It exits non-zero when anything failed, so it is worth something in a script.
`Project::check` is the same thing for a program embedding the library, and
`check_config` takes a configuration that has not been saved yet.

What it cannot tell you is whether the hardware answers. That needs the rig.

### Getting the results out

```cmd
cargo run -- export test_projects/scaled
```

Writes one CSV per recorded run into `PROJECT/export`, named for when the run
started:

```
    wrote    2026-08-27_09-01-49Z.csv  (12 rows)
    skipped  2026-08-27_09-01-50Z.csv  (already exported)
```

A run whose file is already there is left alone, so exporting again costs
nothing and does nothing. The file is the only record of what has been
exported, so deleting one is how to have it written again.

```
timestamp_utc,seconds,Rig/Flow (L/min),Rig/Pressure (bar)
2026-08-26 20:54:12.000000,0.000000,14.5,7.25
2026-08-26 20:54:12.050000,0.050000,14.5,7.31
```

`seconds` counts from the first reading of that run, which is what a test is
usually plotted against. Values are written to whatever precision they are held
at: a reading is what the instrument said, and rounding it on the way out would
be this deciding how much of it matters.

Channels converted together share a timestamp exactly, so their columns line up:
a mock device, a serial frame and an NI task each produce one scan holding every
channel. Channels on **different** devices never do — separate threads, landing
about half a millisecond apart — and a **polled Pico** does not either, because
it really does convert one channel at a time and stamps each with when it was
measured. Either way a row leaves the other columns blank. A blank means no reading was taken then; the last one is
never carried forward, since that would invent data indistinguishable from the
real thing.

The database is opened read only. Exporting must not be able to damage the
results it is reading.

Two runs starting in the same second, which stopping and restarting a recording
quickly will do, would want the same file. The second gets the run number added
rather than being taken for the first and never written.

**Export a run that is still recording and you get it as far as it had got, and
it is skipped from then on.** Stop the recording first, or delete the file.

### Reading the results back

`Archive` opens `results.db` read only and walks it: which runs it holds, which
devices were in each, which channels on each device, and the readings
themselves.

```rust
let archive = Archive::open(&project.database_path())?;
for run in archive.runs()? {
    for device in archive.devices(run.id)? {
        for channel in archive.channels(device.id)? {
            let points = archive.readings(channel.id)?;
        }
    }
}
```

`reading_count` answers how many there are without fetching them, which is what
a tree of runs wants before somebody has asked to see one.

Read only, deliberately: whatever is looking through old results must not be
able to damage them, and a run may be recording into the same file while this
reads it.

Results files carry a schema version. This one writes version 6 and reads back
to version 2, so a file from an older lumberdaq opens for inspection and export
while a *new run* wants a file of the current version. Recording into an older
file is refused rather than migrated.

## Creating a project

### Taking devices from another project

`DaqConfig::merge` copies the devices and calculated channels out of another
configuration and leaves the rest of it behind, which is what makes a file of
devices usable as a library to add to whatever is being set up.

```rust
let library = read_configuration_file(&path)?;
let report = config.merge(library);        // report.devices, .calculated, .skipped
```

The configuration being merged into wins every disagreement. A device whose
name is already there is left alone entirely rather than merged into, because a
channel's binding — which field of a serial frame, which input of a Pico — only
means something beside the hardware settings it was written against. Nothing is
renamed and nothing is overwritten, so merging the same file twice does nothing
the second time.

A calculated channel whose inputs are not present is left behind too, and said
so. It would otherwise load, run, and silently produce nothing, the trigger it
waits on never arriving.

### Devices that would fight over one port

`DaqConfig::address_clashes` finds devices pointed at hardware another device
has already claimed — the same serial port, or the same NI MAX device name.
Checked over the whole configuration rather than as one is chosen, since a file
loaded from disk or merged from a library can arrive with a port named twice.

Worth asking before a run. Otherwise the first device opens the port, the
operating system refuses the second, and it is reported as access being denied,
which sends somebody to look at a cable when the fault is in the setup.

`build_config` defines a rig in Rust and writes it out as a config. That is a
convenience for producing something to test against, not how a setup is normally
made.

```cmd
cargo run --example build_config -- test_projects/my_project
```

It overwrites `config.json` in that directory, so edit the example rather than
the config if you are iterating on a rig.

## Using it from another program

```rust
let mut daq = lumberdaq::open("my_project")?;   // reads config, attaches the sink

let report = daq.connect();                      // which devices came up
daq.run(&stop, &mut on_event)?;                  // records until `stop` is set
```

Events reaching the callback are `Connected`, `Disconnected`, `Problem` and
`Concern` — the last being something wrong that is not bad enough to fail a
read, such as serial frames being skipped or a stream that does not look like
the configuration says. It is sent when it starts and when it clears, not every
cycle, and `Hardware::concern` answers the current state at any moment.

`connect` and `run` are separate so a program can decide for itself what a partly
connected rig means, and can show what happens while a run is going. `run` blocks
for the length of the run and reports through the callback, because each device
is being read by its own thread and nothing else can see it until they finish.

A worked example, in the shape a TUI or GUI would use:

```cmd
cargo run --example embed -- test_projects/simulated_and_serial_devices
```

### Doing more than one thing with the data

A run has one sink, so doing two things with what it reads means a sink that is
itself two. `Fanout` is that, and it is a `DataSink` like any other, so nothing
in `Daq` knows the difference.

The pairing it exists for is one sink writing to disk and one doing something
else. Implementing the something else is three methods, none of which has to
write anything anywhere:

```rust
impl DataSink for Alarm {
    fn init(&mut self, _config: &DaqConfig) -> Result<()> { Ok(()) }

    fn write_batch(&mut self, batch: &Batch) -> Result<()> {
        if batch.channel != self.channel { return Ok(()); }
        for point in batch.datapoints.iter() {
            // ... notice what matters
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> { Ok(()) }
}

daq.set_sink(Box::new(
    Fanout::new()
        .and("database", project.sink(storage)?)
        .and("alarm", Box::new(alarm)),
))?;
```

Every sink is offered every batch, so one that cares about a single channel says
so itself; there is no subscribing. And because it sees the batches the recorder
sees, it cannot disagree with what was written — an alarm fed by a second read
of the hardware could say a limit was never crossed while the results file said
it was.

This is how a live display attaches. choptui's is a sink alongside the file
being written, not a second path through the library.

Every sink is offered each batch even if an earlier one fails, so a sink that
has gone wrong cannot starve the others; the failure is reported afterwards and
names all of them, since a full disk fails every sink at once.

```cmd
cargo run --example alarm_while_recording -- test_projects/scaled Pressure 8
```

```
  07:50:20.830  Pressure over 8 at 8.536  (crossing 1)
  07:50:21.380  Pressure back under at 7.939
  07:50:22.830  Pressure over 8 at 8.536  (crossing 2)
```

### Starting and stopping recording

A run holds its sink for the whole run, so nothing can hand one over partway
through when somebody presses record. A `Recorder` is attached from the start
instead and writes only while a flag is set:

```rust
let recording = Arc::new(AtomicBool::new(false));
daq.set_sink(Box::new(Recorder::new(
    Arc::clone(&recording),
    Box::new(move || project.sink()),
)))?;
```

The sink underneath is built when recording starts rather than up front, so a
session that is only being watched leaves no results file behind at all. It is
built afresh each time, so stopping and starting gives a second recording rather
than more of the first. Both go into the one database and its runs table tells
them apart, so starting again needs nothing said about where it should go.

Devices keep reading throughout. What is not recorded is still acquired, which
is what lets a display show live readings before anyone has pressed anything.

```cmd
cargo run --example record_in_bursts -- test_projects/scaled
```

## Project directory

```
my_project/
    config.json      the setup: devices, channels, sample rates
    results.db       every run ever recorded here
    plot_config.json how a display lays those channels out, if one has saved it
    export/          one csv per run, written by `lumberdaq export`
```

Results and exports are gitignored; `config.json` is not.

`results.db` holds the setup and every run ever recorded there. It is readable
while recording, so a plot can watch a run in progress, and recording again adds
a run rather than overwriting. `lumberdaq export` writes each run out as a CSV
when something else needs to read it.

There is nothing to choose: recording straight to CSV was dropped once `export`
could do the same job from the database, and the `"storage"` setting went with
it. An older config still naming one loads unchanged — the field is ignored.

Each device is read on its own thread, so a slow device does not hold up a fast
one.

### Rates

Two different things get called a rate, and only one of them is about the data.

**`read_interval_ms`**, on the device, is how often lumberdaq collects from it.
It defaults to 100 ms and usually needs no thought. It is *not* how often the
data is saved: that happens on its own one second timer regardless.

**Sample rate** is how fast the hardware measures. It only appears as a setting
where we can actually command the instrument:

| device | who sets the sample rate | setting |
|---|---|---|
| Pico, polled | us, implicitly: a read *is* a sample | `read_interval_ms` |
| Pico, streaming | us, explicitly: the unit is told to scan at this rate | `acquisition.sample_interval_ms` |
| Mock, streaming | us, for a simulated device | `acquisition.sample_interval_ms` |
| Serial | **the device. There is no setting, because we cannot change it** | none |

So for a polled Pico, `read_interval_ms` really is your sample rate. For anything
that samples on its own schedule it is only a collection rate: it changes how
promptly data reaches disk, and nothing about the data. Set it slower on a serial
device and you get exactly the same samples with exactly the same timestamps, in
larger batches, later.

That is why timestamps are trustworthy whatever it is set to. They come from the
instrument for a streaming Pico, from the schedule for a streaming mock, and from
when the bytes arrived for serial - never from when we happened to collect.

### What happens when

```
device  --read_interval_ms-->  batches  --channel-->  sink  --1 s-->  disk
        per device, default 100ms       immediately          fixed
```

## Test projects

Setups under `test_projects/`, each showing one thing:

| | needs hardware | shows |
|---|---|---|
| `mock_sine` | no | streaming with nothing plugged in. Two sine channels sampled at 100 Hz, collected far more slowly. The values can be checked against the wave they claim to be. |
| `scaled` | no | scaled channels, recording the sensor's units rather than volts, with and without named constants |
| `calculated` | no | a calculated channel over one device: a scaled value and a squared one |
| `twin` | no | two devices at the same rate, which still sample at different moments |
| `mixed_rate` | no | 1 Hz beside 10 Hz, which is what pairing by nearest sample is for |
| `differential` | no | calculated channels across devices at 50 ms and 500 ms, driven by the slower |
| `simulated_and_serial_devices` | a serial device on COM3 | a mock and a real device in one run, at different rates, on their own threads |
| `pico_adc20` | ADC-20 or ADC-24 | polled acquisition, where `read_interval_ms` is the sample rate |
| `pico_adc20_stream` | ADC-20 or ADC-24 | the same unit told to scan itself, so samples carry the unit's own timestamps |
| `pico_differential` | ADC-20 or ADC-24 | inputs paired against the one above, and what that costs |
| `ni_usb6001` | NI USB-6001 | a DAQmx device addressed by its MAX name |

`mock_sine` is the one to run first, since it needs nothing attached:

```cmd
cargo run -- test_projects/mock_sine
```

### Differential inputs on a Pico logger

A channel measures against ground unless it says otherwise:

```json
{ "name": "Delta P", "unit": "V", "description": "",
  "channel": 1, "range": "milli_volts2500", "single_ended": false }
```

A differential input measures between a channel and the one above it, so the
first of the pair is always odd and the even channel beside it is consumed. An
ADC-20 has eight inputs and therefore four differential pairs, 1-2, 3-4, 5-6 and
7-8; an ADC-24 has sixteen and eight pairs.

Configuring that wrongly is refused before a run rather than at connect, naming
the pair you probably meant. Whether a channel exists at all depends on the
model, so that is checked when the unit is opened and can say which it is.

## Scaling a channel

A sensor reports volts when the quantity you want is litres per minute. A scale
converts each reading as it arrives, so the channel records the quantity and not
the voltage it was measured as. `x` is the raw measurement:

```json
{ "name": "Pressure", "unit": "bar", "description": "0-10 bar transducer",
  "scale": "x * 5 + 5" }
```

Any channel on any device can take a `scale`; leave it out and readings are
stored as they came.

On hardware that only ever measures volts — a Pico or an NI analog input — a
scale is also the only way the unit is allowed to change. Without one the unit is
`V` and saying otherwise is refused; with one it is whatever the scale produces.

### Keeping the numbers editable

Written that way, the constants dissolve into the arithmetic. `x / 120` gives no
hint that 120 is a shunt resistor, so refitting a 100 ohm one means working the
equation out again, and no saved project can say what sensor it was for.

Naming them instead keeps them editable:

```json
{ "name": "Flow", "unit": "L/min", "description": "0-29 L/min flow meter",
  "scale": {
    "from": "4-20 mA transmitter",
    "equation": "(((x / shunt_ohms) * 1000 - 4) / 16) * (high - low) + low",
    "parameters": { "shunt_ohms": 120, "low": 0, "high": 29 }
  } }
```

Both forms are the same equation at run time — the constants are simply bound
alongside `x`. `from` is a label and nothing more, naming the sensor definition
the numbers came from so an interface can offer the right form for editing them.
The equation is copied in rather than referred to, so a project still runs when
that definition is not to hand.

`Scale::parameters` and `Scale::from` are what a form reads to fill itself in.

### What is checked, and when

A scale that will not parse, or that reads a name it has no value for, is
refused when the project is loaded rather than on the first reading of a run.
The message lists what was available, since the cause is almost always a typo:

```
the scale for 'Flow' uses 'shunt', which it has no value for.
Available: x (the measurement), high, low, shunt_ohms. Scale: x / shunt
```

A reading that cannot be scaled at all is left out and reported; the others from
the same read are still kept.

**The raw reading is not kept.** That is the point of it, since nobody wants
volts from a flow meter, but it does mean the scale is the only way back to the
measurement. It is written into the results with the rest of the config, so a
wrongly scaled run can be recovered as long as the equation can be undone —
which for a multiplication or an offset it always can.

`test_projects/scaled` shows both forms with an unscaled channel beside them for
comparison. It needs no hardware.

## Serial devices

A device sending readings over a serial port, in frames this end has to find in
the stream. Both what a frame *is* and where each channel sits inside it are
configuration, because neither can be guessed.

```json
{
  "type": "SerialStream",
  "port": "COM3",
  "baudrate": 115200,
  "frame_pattern": "#([^#$]*)\\$",
  "channels": [
    { "name": "Flow", "unit": "L/min", "index": 1 },
    { "name": "Pressure", "unit": "bar", "index": 2 }
  ]
}
```

`frame_pattern` is a regular expression matching one complete frame. It both
finds the boundaries and strips whatever wraps the data: a capture group is the
data if there is one, otherwise the whole match. The default above reads
`#1,14.5,7.25$`; a device that simply sends lines wants `([^\r\n]+)\r?\n`
instead.

The expression **must require whatever ends a frame**. That is what says a
frame has fully arrived rather than being half way through the wire, and a
pattern that can match without its terminator, such as `(.*)`, will happily
match a partial frame and hand back truncated data.

`index` is which comma separated field of the frame a channel reads, counting
from zero.

Anything between frames is discarded, so a board that also prints status lines
costs nothing — provided the pattern cannot match one. That is worth thinking
about with a line based pattern, which by definition matches every line
including `ERROR: sensor timeout`; a pattern describing the shape of the data,
such as `^(\d+(?:,-?[\d.]+)+)\r?\n`, will not.

A frame that matches but that the channels cannot read is skipped and reported
rather than failing the batch around it, so one status line does not punch a
hole in a recording.

### Finding out whether the settings read the device

A baud rate cannot be worked out by asking, only by listening.
`serial_stream::check_stream` opens the port for a moment and says which of four
things happened: it reads, bytes arrive but nothing matches the pattern, frames
match but the channels cannot read them, or nothing arrives at all.

```rust
match check_stream(&serial_config, Duration::from_secs(5))? {
    StreamCheck::Reads => {}
    StreamCheck::Unreadable => {}   // very probably the baud rate
    StreamCheck::Mismatched { reason } => {}
    StreamCheck::Silent => {}
}
```

Frames are weighed against everything else that arrived before the channels get
the blame. At the right rate a stream is almost entirely frames; at the wrong
one, noise throws up the occasional accidental match — `#` and `$` each turn up
about once in every 256 random bytes — and treating one of those as proof the
rate was right is how a wrong rate escapes without a warning.

Give it several seconds. Opening a port asserts DTR, which resets an Arduino,
and its bootloader is silent for a second or two; a shorter wait hears only the
silence it caused and blames the baud rate for it.

## National Instruments

Kept for USB-6001 hardware we already own; new work goes to Pico.

```json
{
  "type": "NiDaqmx",
  "description": "USB-6001",
  "device": "Dev1",
  "channels": [
    { "name": "Inlet", "unit": "V", "description": "ai0 against ground",
      "channel": 0, "single_ended": true },
    { "name": "Bridge", "unit": "V", "description": "ai1 measured against ai5",
      "channel": 1, "single_ended": false, "range": [-10.0, 10.0] }
  ]
}
```

`device` is the name NI MAX gave the hardware. DAQmx addresses channels by it,
as in `Dev1/ai0`, and it is assigned outside this program, so it has to be
written down rather than found. `cargo run -p nidaqmx --example probe_devices`
lists what the driver knows about.

Polled only. Every read converts, so the device's `read_interval_ms` is the
sample rate. All the channels of one device are converted in a single scan and
share a timestamp exactly.

Nothing here needs NI software installed unless a project actually contains one
of these devices — see [drivers](../README.md#drivers).

An analog input measures volts and nothing else, so a channel need not say so:
leave `unit` out and it becomes `V`. A channel that claims some other unit
without a scale to make it true is refused, since the recorded number would be
the voltage while every plot and exported column said otherwise. A **scaled**
channel may say whatever the scale produces — that is what a scale is for.

### Differential inputs on an NI device

NI pairs an input with the one **four** above it, so on an eight input device
only ai0 to ai3 can start a pair, each measured against ai4 to ai7. This is not
the same as a Pico, which pairs with the input immediately above.

The partner is consumed by the pair, so reading it separately is refused before
a run rather than at connect. Whether the device has that many inputs at all
needs the driver, and is checked when it connects.

## Calculated channels

A scale reads one channel. When a value needs several — a differential pressure
from two transducers — a calculated channel applies an equation across measured
channels and records the result beside them, under a device of its own so what
was measured stays distinct from what was worked out.

```json
"calculated": {
  "info": { "name": "Derived", "description": "" },
  "channels": [
    { "name": "Delta P", "unit": "bar", "description": "",
      "inputs": { "high": { "device": "Transducer", "channel": "High" },
                  "low":  { "device": "Transducer", "channel": "Low" } },
      "equation": "high - low" }
  ]
}
```

Inputs are given short names because channel names have spaces and quoting those
inside an expression is miserable. The usual arithmetic works — `+ - * / %` and
`^` for a power — along with `sqrt`, `abs`, `round`, `floor`, `ceil`, `ln`,
`log10`, `exp`, `log(x, base)`, the trigonometric functions, `atan2(y, x)`,
`min(a, b)`, `max(a, b)`, `pow(a, b)` and `hypot(a, b)`. Angles are in radians.

All of them answer to their plain names. evalexpr spells some of these
`math::sqrt`, and those forms still work, but nothing needs the prefix — having
half the functions want one and half refuse it was a distinction with nothing
behind it.

Equations are read at run time, so a program embedding lumberdaq can let someone
write one while it is running. `CalculatedChannel::validate` runs exactly the
checks a run would, and `DaqConfig::available_inputs` lists what can be referred
to, which is what a text box and a dropdown need.

### Combining channels that sample at different times

Two devices sampling at the same rate still sample at different moments, so
values have to be paired rather than matched. A calculated channel is driven by
its **slowest** input, and every other input contributes its nearest sample
within half of that input's own period. Nothing to configure: the periods come
from the setup, or are measured for a device that streams at a rate of its own.

Being driven by the slowest input is what keeps it accurate. Measured on a 1 Hz
channel against a 10 Hz one:

```text
trigger on the slow input, pair with the fast:   median   1.8 ms
trigger on the fast input, pair with the slow:   median 290.7 ms
```

Two consequences worth knowing. The output appears at the **slowest** input's
rate; producing it faster would mean repeating a value that was never measured.
And if an input has no sample near a trigger, because its device stopped, that
sample is skipped and reported rather than paired with something stale.

Channels of a single device share timestamps exactly, so a differential between
two channels of one transducer is exact rather than paired.

## Development

```cmd
cargo test
cargo check --all-targets
```

`cargo check` on its own does not build examples, so run it with
`--all-targets` or `cargo test` to catch breakage there.

This crate is a member of the workspace at the repository root, so build output
goes to `../target` and `cargo test --workspace` from there covers picolog and
choptui as well.

# Todo
- Add pico technology TC-08.
- Post processing.
- Create installer including any required .dll files.
