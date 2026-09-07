//! Record a project from the command line.
//!
//!     lumberdaq [project directory]
//!     lumberdaq check [project directory]
//!     lumberdaq export [project directory]
//!
//! The directory holds config.json describing the devices, and is where the
//! results are written. Defaults to the current directory, so running inside a
//! project needs no argument at all.

use lumberdaq::project::Project;
use lumberdaq::session::DeviceEvent;
use lumberdaq::Result;
use std::sync::atomic::{ AtomicBool, Ordering };
use std::sync::Arc;

const HELP: &str = "\
Record a data acquisition project.

USAGE:
    lumberdaq [PROJECT]           Record until interrupted with Ctrl-C.
    lumberdaq check [PROJECT]     Check the setup and stop.
    lumberdaq export [PROJECT]    Write recorded runs out as CSV.

ARGS:
    PROJECT    Directory holding config.json. Defaults to the current directory.

`check` builds everything a recording would build and reports what is wrong,
without connecting to any hardware or writing a results file. Useful away from
the rig, and before a run that matters.

`export` writes one CSV per recorded run into PROJECT/export, named for when
the run started. A run whose file is already there is left alone, so exporting
again costs nothing and does nothing.
";

fn main() {
    // Not `fn main() -> Result<()>`. That prints the error with Debug, which
    // shows the enum's shape rather than the message written for it:
    //
    //     Error: NoProjectHere { directory: "the current directory" }
    //
    // rather than
    //
    //     Error: no config.json in the current directory, so there is ...
    //
    // and it drops the source chain entirely.
    if let Err(error) = run() {
        eprintln!("\nError: {}", error);

        // Whatever caused it, in turn. This is where a regex error or serde's
        // line and column come out, having been attached with #[source].
        let mut cause = std::error::Error::source(&error);
        while let Some(next) = cause {
            eprintln!("  caused by: {}", next);
            cause = next.source();
        }
        // The library says a project is not there; only this knows what to
        // type instead.
        if matches!(error, lumberdaq::Error::NoProjectHere { .. }) {
            eprintln!("\nUSAGE: lumberdaq [PROJECT]");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("-h") | Some("--help") => {
            print!("{}", HELP);
            Ok(())
        }
        Some("check") => check(&directory_or_here(arguments.next())),
        Some("export") => export(&directory_or_here(arguments.next())),
        argument => record(&directory_or_here(argument.map(String::from))),
    }
}

/// Running inside a project should need no argument at all.
fn directory_or_here(argument: Option<String>) -> String {
    argument.unwrap_or_else(|| ".".to_string())
}

/// Write recorded runs out as CSV, skipping any already written.
fn export(directory: &str) -> Result<()> {
    let project = Project::new(directory);
    let database = project.database_path();
    if !database.exists() {
        return Err(format!(
            "no {} in {}, so nothing has been recorded there yet",
            database.file_name().unwrap_or_default().to_string_lossy(),
            directory
        )
        .into());
    }

    let report = lumberdaq::export::export(&database, &project.export_path())?;
    println!("Exporting {}\n", directory);
    for (path, rows) in report.written.iter() {
        println!("    wrote    {}  ({} rows)", name_of(path), rows);
    }
    for path in report.skipped.iter() {
        println!("    skipped  {}  (already exported)", name_of(path));
    }

    match (report.written.len(), report.skipped.len()) {
        (0, 0) => println!("    nothing recorded yet"),
        (0, _) => println!("\nEverything was exported already. Delete a file to have it written again."),
        (written, _) => println!("\n{} run(s) written to {}.", written, project.export_path().display()),
    }
    Ok(())
}

fn name_of(path: &std::path::Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().to_string()
}

/// Report on a setup without connecting to it or writing anything.
fn check(directory: &str) -> Result<()> {
    let report = Project::new(directory).check()?;
    println!("Checking {}
", directory);

    for part in report.passed.iter() {
        println!("    ok    {}", part);
    }
    for problem in report.problems.iter() {
        println!("    FAIL  {}", problem.part);
        println!("          {}", problem.error);
        // The regex, or serde's line and column, attached with #[source].
        let mut cause = std::error::Error::source(&problem.error);
        while let Some(next) = cause {
            println!("          caused by: {}", next);
            cause = next.source();
        }
    }

    match report.is_ok() {
        true => {
            println!("
Nothing wrong with the setup. Whether the hardware answers is another question.");
            Ok(())
        }
        // A non-zero exit, so this is worth something in a script.
        false => Err(format!(
            "{} of {} parts of the setup would stop a run.",
            report.problems.len(),
            report.problems.len() + report.passed.len()
        )
        .into()),
    }
}

fn record(directory: &str) -> Result<()> {
    // Everything about the setup, including which format to record in, comes
    // from the directory. Nothing here needs recompiling to change a channel.
    let mut daq = lumberdaq::open(directory)?;
    println!("Project: {}", directory);

    // Try every device, so one bad port does not hide the state of the rest.
    let report = daq.connect();
    for name in report.connected.iter() {
        println!("    connected  {}", name);
    }
    if !report.all_connected() {
        eprintln!();
        eprintln!("Could not connect to {} of {} devices:", report.failed.len(), daq.devices.len());
        for (name, reason) in report.failed.iter() {
            eprintln!("    {}: {}", name, reason);
        }
        // Recording a run that is quietly missing a device wastes the run.
        return Err("Not all devices connected. Fix the above, or remove those devices from the setup.".into());
    }

    // Ctrl-C sets the flag rather than killing the process, so the run stops
    // tidily and the sink gets its final flush. Killing it outright would lose
    // whatever had not reached disk.
    let stop = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&stop);
    ctrlc::set_handler(move || handler_flag.store(true, Ordering::Relaxed))
        .map_err(|error| format!("Could not listen for Ctrl-C: {}", error))?;

    println!("\nRecording. Press Ctrl-C to stop.\n");
    daq.run(&stop, &mut |event| match event {
        DeviceEvent::Problem { device, error } => {
            eprintln!("    ! {}: {}", device, error);
        }
        DeviceEvent::Connected { device } => {
            println!("    reconnected  {}", device);
        }
        DeviceEvent::Concern { device, concern } => match concern {
            Some(concern) => eprintln!("    ? {}: {}", device, concern),
            None => println!("    {} is reading cleanly again", device),
        },
        DeviceEvent::Disconnected { device, cause } => {
            eprintln!(
                "    ! lost {}: {}",
                device,
                cause.unwrap_or_else(|| "unknown".to_string())
            );
        }
        // Not a fault, so stdout rather than stderr: the device is working and
        // this is it talking.
        DeviceEvent::Said { device, line } => {
            println!("    {} said: {}", device, line);
        }
    })?;

    println!("\nStopped.");
    // Printed here rather than by the library. Where the words go is the
    // caller's to decide, and a terminal interface or a window cannot undo a
    // `println!` that has already happened.
    for device in daq.devices.iter() {
        println!("Latest reading from device: {}", device.info.name);
        for channel in device.channels.iter() {
            println!("    {}", channel.latest_as_string());
        }
    }
    Ok(())
}
