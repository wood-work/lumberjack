<div align="center">

<img src="assets/Lumberjack.svg" alt="" width="64">

# Lumberjack

</div>

Data acquisition software for physical data logging devices. Package contains a rust library `lumberdaq`, terminal user interface for logging and data visualisation only, and a graphical user interface that can be used for full configuration, data logging and visualisation as well as loading and inspection of data records.

# Features

Features include:
- Data logging simultaneously from multiple devices
- Live graphical data visualisation with configurable plot layout
- SQLite data format plus export to tabulated csv
- Calculations on channels using user defined formulas
- Calculated device allowing calculated channels from multiple devices
- Device and plot configurations saved and loaded via simple human readable json files 

# Hardware

Current hardware supported:
- Pico Technology ADC-20 & ADC-24
- NI USB-6001
- USB serial devices sending delimited frames, described by a regular expression

Mock devices are available for testing.

# Package Layout
A Cargo workspace over the Rust crates, so they share one `target/` directory
and one `Cargo.lock`.

* `lumberdaq/` — the data acquisition library and its CLI. The active work.
* `choptui/` — terminal monitor for a run. A separate crate on purpose: it can
  only reach lumberdaq's public API, so a missing export fails to compile here.
* `lumbergui/` — the iced interface. The direction the GUI is taking.
* `picolog/` — safe wrappers over the Pico Technology driver, used by lumberdaq.
* `nidaqmx/` — safe wrappers over the NI DAQmx driver, used by lumberdaq.
* `sandbox/` — throwaway experiments, kept out of the library's dependencies.

`sandbox/` stands outside the workspace and keeps its own dependencies.
`assets/` holds the marks and the font, kept beside the project rather than
inside the interface that draws them, so the same logo serves this page.

## Platform

Windows, in practice. Both drivers are DLLs, the interface is built with a
Windows icon and a statically linked C runtime, and device arrival and removal
are heard through a Windows message-only window. Everything else is portable
Rust and the acquisition loop has nothing platform specific in it, so a port is
a question of the driver loading and one notification path rather than a
rewrite. Nobody has tried it.

## Drivers

**No vendor software is needed to build this, or to run it against hardware you
do not own.** Bindings are generated once and committed rather than generated
during the build, and each driver is looked up at run time with `libloading`
rather than linked. So the workspace compiles on a machine with none of it
installed, and a driver that is not there fails when a device is opened, saying
what it looked for.

| Devices | To build | To read that hardware | To regenerate bindings |
|---|---|---|---|
| Mock hardware | Nothing | — | — |
| Pico ADC-20/24 | Nothing | PicoSDK runtime (`picohrdl.dll`) | PicoSDK headers, LLVM |
| NI USB-6001 | Nothing | NI-DAQmx runtime (`nicaiu.dll`) | NI-DAQmx Support for C, LLVM |

Only whoever regenerates bindings needs a header and a working bindgen, and that
is done once per driver, not once per build.

### Installing Pico
Download and run the [PicoSDK](https://www.picotech.com/downloads/sdk-release/pico-software-development-kit-64bit).


### Installing NI-DAQmx

Download and run the [DAQmx installer](https://www.ni.com/en/support/downloads/drivers/download.ni-daq-mx.html). The installer offers a number of options but only the following are required:

| Option | Why |
|---|---|
| **NI-DAQmx Runtime with Configuration Support** | the driver itself, plus MAX for seeing and naming devices |
| **NI-DAQmx Support for C** | `NIDAQmx.h`, and only for regenerating bindings |



If you are only *running* lumberdaq against a 6001, the runtime alone is enough:
Support for C is a build-time convenience for this repository, not a dependency
of the program.

DAQmx addresses channels by device name, as in `Dev1/ai0`, so a device needs a
name before it can be read. MAX assigns one when the hardware is plugged in, and
will also create a **simulated** device, which is enough to develop and test against with
nothing attached.


# Running

Each interface takes a project directory — a folder holding `config.json` — and
with no argument uses the current one. `mock_sine` and `scaled` need no hardware
attached, so either is a good first run.

The graphical interface, which is the one that can also *edit* a setup:

```ps
cargo run -p lumbergui -- lumberdaq/test_projects/scaled
```

The command line recorder, and the terminal monitor:

```ps
cargo run -p lumberdaq -- lumberdaq/test_projects/scaled
cargo run -p choptui   -- lumberdaq/test_projects/scaled
```

Other test projects are available; `lumberdaq/README.md` lists what each shows.

# Development

```ps
cargo test --workspace
cargo clippy --workspace --all-targets
```

A release build puts the interface at `target/release/lumbergui.exe`, which runs
on its own — the C runtime is linked statically, so there is no redistributable
to install alongside it. It still needs the vendor driver for whichever hardware
a project actually uses.

# Licence

MIT. See [LICENSE](LICENSE).
