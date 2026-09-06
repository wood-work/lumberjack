//! Check an equation written after this was compiled.
//!
//!     cargo run --example equation -- test_projects/mock_sine "(v + 1) * 2.5"
//!
//! Equations are strings read at run time, so a program embedding lumberdaq can
//! let someone write one while it is running. This is what a user interface
//! would do behind a text box: list the channels available as inputs, then check
//! what was typed and say what is wrong before anything is recorded.
//!
//! Nothing here knows the equation until it is handed one.

use lumberdaq::calculated::{ CalculatedChannel, ChannelRef };
use lumberdaq::channel::ChannelInfo;
use lumberdaq::project::Project;
use lumberdaq::Result;
use std::collections::BTreeMap;

fn main() -> Result<()> {
    let directory = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "test_projects/mock_sine".to_string());
    let equation = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "(v + 1) * 2.5".to_string());

    let config = Project::new(&directory).read_config()?;

    // What a UI would put in a dropdown, rather than having someone type a
    // device and channel name correctly.
    let inputs = config.available_inputs();
    println!("Channels available as inputs in {}:", directory);
    for input in inputs.iter() {
        println!("    {}", input);
    }

    let source = match inputs.first() {
        Some(source) => source.clone(),
        None => {
            eprintln!("\nThat project has no channels to calculate from.");
            return Ok(());
        }
    };

    println!("\nChecking: {}", equation);
    println!("      v = {}", source);

    let candidate = proposed(&equation, "v", source);
    match candidate.validate() {
        Ok(()) => println!("\n  accepted"),
        Err(error) => {
            println!("\n  rejected: {}", error);
            if let Some(cause) = std::error::Error::source(&error) {
                println!("            {}", cause);
            }
        }
    }

    // A few more, to show the checking is real rather than a formality. None of
    // these strings existed when this was built either.
    println!("\nOther equations, same check:");
    for text in [
        "v * 2 + 273.15",
        "sqrt(v * v)",
        "v * (2",
        "v * * 2",
        "v * gain",
        "",
    ] {
        let candidate = proposed(text, "v", ChannelRef {
            device: "any".to_string(),
            channel: "any".to_string(),
        });
        match candidate.validate() {
            Ok(()) => println!("    {:<16?}  accepted", text),
            Err(error) => println!("    {:<16?}  rejected: {}", text, error),
        }
    }
    Ok(())
}

fn proposed(equation: &str, variable: &str, source: ChannelRef) -> CalculatedChannel {
    let mut inputs = BTreeMap::new();
    inputs.insert(variable.to_string(), source);
    CalculatedChannel {
        info: ChannelInfo {
            name: "Proposed".to_string(),
            unit: "-".to_string(),
        scale: None,
            enabled: true,
        },
        inputs: inputs,
        parameters: BTreeMap::new(),
        equation: equation.to_string(),
    }
}
