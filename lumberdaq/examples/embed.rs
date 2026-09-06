//! Drive lumberdaq from your own program.
//!
//!     cargo run --example embed -- test_projects/simulated_devices
//!
//! This is the shape a TUI or GUI would use. The library never prints, never
//! decides how long to record, and never decides what a partly connected rig
//! means: those belong to whatever is embedding it.

use lumberdaq::session::DeviceEvent;
use lumberdaq::Result;
use std::sync::atomic::{ AtomicBool, Ordering };
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    let directory = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "test_projects/simulated_devices".to_string());

    // One call: reads config.json, builds the devices, attaches the sink the
    // config asks for. Nothing about the rig is compiled in here.
    let mut daq = lumberdaq::open(&directory)?;

    // Connecting is separate from running so a program can decide for itself
    // what to do about a device that did not come up. A GUI might show this in
    // a dialog and offer to start anyway; here we carry on regardless, which
    // the retry loop will keep working at while the others record.
    let report = daq.connect();
    println!("{} of {} devices connected", report.connected.len(), daq.devices.len());
    for (name, reason) in report.failed.iter() {
        println!("  will keep retrying {}: {}", name, reason);
    }

    // Stopping is the caller's business too. A UI would set this from a button
    // or a menu; this stands in for that with a timer.
    let stop = AtomicBool::new(false);

    let mut samples = 0usize;
    let mut problems = 0usize;

    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_secs(3));
            stop.store(true, Ordering::Relaxed);
        });

        // Events arrive here as they happen. While a run is in progress the
        // devices belong to their threads, so this is the only view of them.
        daq.run(&stop, &mut |event| match event {
            DeviceEvent::Problem { device, error } => {
                problems += 1;
                println!("  problem on {}: {}", device, error);
            }
            DeviceEvent::Connected { device } => println!("  {} came back", device),
            DeviceEvent::Concern { device, concern } => match concern {
                Some(concern) => println!("  {} is unhappy: {}", device, concern),
                None => println!("  {} is reading cleanly again", device),
            },
            DeviceEvent::Disconnected { device, cause } => {
                println!("  lost {}: {}", device, cause.unwrap_or_default())
            }
        })
    })?;

    // The run is over, so the devices are readable again and hold what they
    // collected.
    for device in daq.devices.iter() {
        for channel in device.channels.iter() {
            samples += channel.datapoints.len();
        }
        println!("Latest reading from device: {}", device.info.name);
        for channel in device.channels.iter() {
            println!("    {}", channel.latest_as_string());
        }
    }
    println!("\n{} problems reported, {} samples still buffered", problems, samples);
    Ok(())
}
