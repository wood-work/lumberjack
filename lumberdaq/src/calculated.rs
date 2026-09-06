//! Channels worked out from other channels rather than measured.
//!
//! A sensor reports volts; the thing you care about is pressure or flow. A
//! calculated channel applies an equation to one or more measured channels and
//! records the result alongside them, so what gets stored is the quantity you
//! actually wanted.
//!
//! These are not a device in the running sense. Devices own hardware and are
//! read on their own threads, each with exclusive access; a calculated channel
//! needs to see *other* devices' data, which that arrangement deliberately
//! prevents. So it is a device in the configuration and in the results, but the
//! work happens where every device's data already converges: the collecting
//! loop, between the devices and the sink.
//!
//! # Combining channels that never share a timestamp
//!
//! Two devices sampling at the same rate still sample at different moments:
//! separate threads, started at separate instants. Measured on two identical
//! mock devices at 100ms, not one timestamp of 53 matched, though the nearest
//! partner was a median of 0.5ms away.
//!
//! So values have to be paired rather than matched, and which input drives the
//! output decides how far out that pairing can be. A calculated channel is
//! driven by its **slowest** input, and every other input contributes its
//! nearest sample. That bounds the error by half the *fastest* period rather
//! than half the slowest. Measured on a 1Hz channel against a 10Hz one:
//!
//! ```text
//! trigger on the slow input, pair with the fast:   median   1.8 ms
//! trigger on the fast input, pair with the slow:   median 290.7 ms
//! ```
//!
//! The output therefore appears at the slowest input's rate. Producing it any
//! faster would mean repeating a stale value, which is resolution that was
//! never measured.

use crate::channel::ChannelInfo;
use crate::datapoint::DataPoint;
use crate::equation::Expression;
use crate::device::DeviceInfo;
use crate::session::DeviceEvent;
use crate::storage::Batch;
use crate::{ Error, Result };
use chrono::{ DateTime, Utc };
use serde::{ Deserialize, Serialize };
use std::collections::{ BTreeMap, VecDeque };
use std::time::Duration;


/// How many recent samples of an input to keep.
///
/// Enough to find the one nearest a trigger, with room for batches from
/// different devices arriving out of order.
const HISTORY: usize = 64;

/// How many gaps to measure before trusting a rate nothing declared.
const GAPS_TO_ESTIMATE: usize = 5;

/// Which measured channel an equation takes a value from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChannelRef {
    pub device: String,
    pub channel: String,
}

impl std::fmt::Display for ChannelRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.device, self.channel)
    }
}

/// One calculated channel: what it is, where its values come from, and the
/// equation that turns them into it.
///
/// Inputs are named rather than referred to inline, because channel names have
/// spaces in them and quoting those inside an expression is miserable. The
/// short name is what appears in the equation.
#[derive(Serialize, Deserialize, Clone)]
pub struct CalculatedChannel {
    #[serde(flatten)]
    pub info: ChannelInfo,
    pub inputs: BTreeMap<String, ChannelRef>,
    /// Fixed numbers the equation may use, alongside the inputs.
    ///
    /// The same reasoning as a scale's parameters: written into the arithmetic,
    /// `x / 120` gives no hint that 120 is a shunt resistor, and refitting a
    /// hundred ohm one means working the equation out again. Named, they stay
    /// editable and a saved project says what they were.
    ///
    /// Defaulted and left out when empty, so a channel without any is written
    /// exactly as it was before this existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, f64>,
    pub equation: String,
}

impl CalculatedChannel {
    /// Check this channel on its own, without building a whole calculator.
    ///
    /// Equations are strings read at run time, so a program embedding this can
    /// let someone write one while it is running: a UI can call this as they
    /// type and say what is wrong before anything is recorded. Nothing about an
    /// equation is fixed when lumberdaq is built.
    ///
    /// The same checks a run would make, so passing here means it will build.
    pub fn validate(&self) -> Result<()> {
        compile(self).map(|_| ())
    }
}

/// The device calculated channels belong to.
#[derive(Serialize, Deserialize, Clone)]
pub struct CalculatedDevice {
    pub info: DeviceInfo,
    pub channels: Vec<CalculatedChannel>,
}

/// A calculated channel with its equation compiled and its inputs resolved.
struct Compiled {
    info: ChannelInfo,
    expression: Expression,
    /// Every input, as (variable name, the channel it reads).
    inputs: Vec<(String, ChannelRef)>,
    /// The constants, ready to be handed to the equation with the inputs.
    ///
    /// Held as the pairs `evaluate` wants rather than as the map they were
    /// written in, since they are the same on every sample and rebuilding them
    /// per reading would be work for nothing.
    parameters: Vec<(String, f64)>,
    /// Which input drives the output: the slowest, once the rates are known.
    ///
    /// Not knowable when built if an input is on a device whose rate has to be
    /// measured, so it is settled on the first batch that makes it knowable.
    trigger: Option<usize>,
    /// Trigger samples waiting for their partners.
    ///
    /// A trigger cannot be resolved the instant it lands: the nearest sample of
    /// a faster input may be a millisecond *later* and not here yet. Held until
    /// every other input has something at or after the trigger's time, which is
    /// when the nearest one is certain.
    pending: VecDeque<DataPoint>,
}

/// What is known about how often an input produces samples.
struct Rate {
    /// From the setup, where it says.
    declared: Option<Duration>,
    /// Gaps seen so far, for an input whose rate nothing declares.
    gaps: Vec<Duration>,
    last_seen: Option<DateTime<Utc>>,
}

impl Rate {
    fn interval(&self) -> Option<Duration> {
        if let Some(declared) = self.declared {
            return Some(declared);
        }
        if self.gaps.len() < GAPS_TO_ESTIMATE {
            return None;
        }
        // The median, so one long gap while a device was reconnecting does not
        // stretch the estimate for the rest of the run.
        let mut sorted = self.gaps.clone();
        sorted.sort();
        Some(sorted[sorted.len() / 2])
    }

    fn observe(&mut self, at: DateTime<Utc>) {
        if self.declared.is_none() {
            if let Some(previous) = self.last_seen {
                if let Ok(gap) = (at - previous).to_std() {
                    if self.gaps.len() < GAPS_TO_ESTIMATE * 4 {
                        self.gaps.push(gap);
                    }
                }
            }
        }
        self.last_seen = Some(at);
    }
}

/// Turns measured batches into calculated ones.
pub struct Calculator {
    config: CalculatedDevice,
    compiled: Vec<Compiled>,
    /// Recent samples per input, for finding the one nearest a trigger.
    history: BTreeMap<ChannelRef, VecDeque<DataPoint>>,
    /// How often each input samples, declared or measured.
    rates: BTreeMap<ChannelRef, Rate>,
}

impl Calculator {
    /// Compile every equation, so a bad one is refused before a run starts
    /// rather than failing partway through.
    pub fn from_config(config: CalculatedDevice) -> Result<Calculator> {
        Calculator::with_rates(config, &BTreeMap::new())
    }

    /// Build with whatever the setup says about how often each input samples.
    ///
    /// Anything absent is measured from what arrives, which is how a serial
    /// device is handled: it streams at a rate of its own and nothing declares
    /// it. A channel reading one produces nothing until a few samples have been
    /// seen, which is a short warm up rather than a setting to get wrong.
    pub fn with_rates(
        config: CalculatedDevice,
        declared: &BTreeMap<ChannelRef, Duration>,
    ) -> Result<Calculator> {
        let mut compiled = Vec::with_capacity(config.channels.len());
        let mut rates: BTreeMap<ChannelRef, Rate> = BTreeMap::new();
        for channel in config.channels.iter() {
            let ready = compile(channel)?;
            for (_, source) in ready.inputs.iter() {
                rates.entry(source.clone()).or_insert_with(|| Rate {
                    declared: declared.get(source).copied(),
                    gaps: Vec::new(),
                    last_seen: None,
                });
            }
            compiled.push(ready);
        }
        Ok(Calculator {
            config: config,
            compiled: compiled,
            history: BTreeMap::new(),
            rates: rates,
        })
    }

    pub fn config(&self) -> CalculatedDevice {
        self.config.clone()
    }

    pub fn device_name(&self) -> &str {
        &self.config.info.name
    }

    /// Every channel these equations read from.
    ///
    /// Used to check a setup before running it: an equation naming a channel
    /// that does not exist is a typo, and better caught now than by silently
    /// producing nothing for the whole run.
    pub fn sources(&self) -> Vec<ChannelRef> {
        let mut sources: Vec<ChannelRef> = Vec::new();
        for channel in self.compiled.iter() {
            for (_, source) in channel.inputs.iter() {
                if !sources.contains(source) {
                    sources.push(source.clone());
                }
            }
        }
        sources
    }

    /// Work out whatever this batch makes possible.
    ///
    /// Data is filed first, then any trigger whose partners have all arrived is
    /// resolved. Failures are reported and skipped rather than ending the run:
    /// an equation that divides by zero on one sample should not stop the
    /// recording of everything else.
    pub fn apply(&mut self, batch: &Batch, on_event: &mut dyn FnMut(DeviceEvent)) -> Vec<Batch> {
        let arrived = ChannelRef { device: batch.device.clone(), channel: batch.channel.clone() };
        if !self.rates.contains_key(&arrived) {
            return Vec::new(); // nothing calculates from this channel
        }

        for datapoint in batch.datapoints.iter() {
            if let Some(rate) = self.rates.get_mut(&arrived) {
                rate.observe(datapoint.datetime);
            }
            let history = self.history.entry(arrived.clone()).or_default();
            history.push_back(*datapoint);
            while history.len() > HISTORY {
                history.pop_front();
            }
        }

        // Queue these against any channel they drive.
        for index in 0..self.compiled.len() {
            self.settle_trigger(index);
            let trigger = match self.compiled[index].trigger {
                Some(trigger) => trigger,
                None => continue,
            };
            if self.compiled[index].inputs[trigger].1 != arrived {
                continue;
            }
            for datapoint in batch.datapoints.iter() {
                self.compiled[index].pending.push_back(*datapoint);
            }
            while self.compiled[index].pending.len() > HISTORY {
                self.compiled[index].pending.pop_front();
            }
        }

        let mut produced: Vec<Batch> = Vec::new();
        for index in 0..self.compiled.len() {
            if let Some(batch) = self.resolve(index, on_event) {
                produced.push(batch);
            }
        }
        produced
    }

    /// Decide which input drives a channel, once every rate is known.
    ///
    /// The slowest, because that is what bounds the pairing error.
    fn settle_trigger(&mut self, index: usize) {
        if self.compiled[index].trigger.is_some() {
            return;
        }
        let mut slowest: Option<(usize, Duration)> = None;
        for (position, (_, source)) in self.compiled[index].inputs.iter().enumerate() {
            let interval = match self.rates.get(source).and_then(|rate| rate.interval()) {
                Some(interval) => interval,
                None => return, // still learning a rate; decide later
            };
            if slowest.map_or(true, |(_, best)| interval > best) {
                slowest = Some((position, interval));
            }
        }
        self.compiled[index].trigger = slowest.map(|(position, _)| position);
    }

    /// Emit whatever pending triggers now have all their partners.
    fn resolve(&mut self, index: usize, on_event: &mut dyn FnMut(DeviceEvent)) -> Option<Batch> {
        let trigger = self.compiled[index].trigger?;
        let mut datapoints: Vec<DataPoint> = Vec::new();
        let mut skipped = 0usize;
        let mut first_failure: Option<String> = None;

        while let Some(sample) = self.compiled[index].pending.front().copied() {
            // The constants first, being the same every time. The inputs that
            // follow cannot collide with them: `compile` refused that.
            let mut values: Vec<(String, f64)> = self.compiled[index].parameters.clone();
            let mut ready = true;
            let mut unavailable = false;

            for (position, (variable, source)) in self.compiled[index].inputs.iter().enumerate() {
                if position == trigger {
                    values.push((variable.clone(), sample.value));
                    continue;
                }
                // Half a period: a running input has a sample within that of
                // any instant, so anything further means it has stopped rather
                // than merely being out of phase.
                let window = match self.rates.get(source).and_then(|rate| rate.interval()) {
                    Some(interval) => interval / 2,
                    None => {
                        ready = false;
                        break;
                    }
                };
                let history = match self.history.get(source) {
                    Some(history) if !history.is_empty() => history,
                    _ => {
                        ready = false;
                        break;
                    }
                };
                // Nothing at or after the trigger yet, so a nearer sample may
                // still be on its way. Wait rather than pair with an older one.
                if history.back().map_or(true, |last| last.datetime < sample.datetime) {
                    ready = false;
                    break;
                }
                match nearest(history, sample.datetime, window) {
                    Some(value) => values.push((variable.clone(), value)),
                    None => {
                        unavailable = true;
                        break;
                    }
                }
            }

            if !ready {
                break; // leave it pending; more data may complete it
            }
            self.compiled[index].pending.pop_front();
            if unavailable {
                skipped += 1;
                if first_failure.is_none() {
                    first_failure = Some("had no sample from every input near it".to_string());
                }
                continue;
            }

            match self.compiled[index].expression.evaluate(&values) {
                Ok(value) => datapoints.push(DataPoint {
                    // The trigger's own timestamp. A calculated value happened
                    // when the measurement driving it happened.
                    datetime: sample.datetime,
                    value: value,
                }),
                Err(reason) => {
                    skipped += 1;
                    if first_failure.is_none() {
                        first_failure = Some(reason);
                    }
                }
            }
        }

        if let Some(reason) = first_failure {
            on_event(DeviceEvent::Problem {
                device: self.config.info.name.clone(),
                error: Error::EquationFailed {
                    channel: self.compiled[index].info.name.clone(),
                    equation: channel_equation(&self.config, &self.compiled[index].info.name),
                    skipped: skipped,
                    reason: reason,
                },
            });
        }

        if datapoints.is_empty() {
            return None;
        }
        Some(Batch {
            device: self.config.info.name.clone(),
            channel: self.compiled[index].info.name.clone(),
            datapoints: datapoints,
        })
    }
}

/// The value of the sample closest to `at`, if one falls within `window`.
///
/// None means the input was not producing around then, which is a reason to
/// emit nothing rather than to reach for a stale value.
fn nearest(history: &VecDeque<DataPoint>, at: DateTime<Utc>, window: Duration) -> Option<f64> {
    let mut best: Option<(Duration, f64)> = None;
    for sample in history.iter() {
        let distance = if sample.datetime > at {
            sample.datetime - at
        } else {
            at - sample.datetime
        };
        let distance = match distance.to_std() {
            Ok(distance) => distance,
            Err(_) => continue,
        };
        if distance <= window && best.map_or(true, |(closest, _)| distance < closest) {
            best = Some((distance, sample.value));
        }
    }
    best.map(|(_, value)| value)
}

fn channel_equation(config: &CalculatedDevice, name: &str) -> String {
    config
        .channels
        .iter()
        .find(|channel| channel.info.name == name)
        .map(|channel| channel.equation.clone())
        .unwrap_or_default()
}

/// Compile one channel's equation and check it against its declared inputs.
fn compile(channel: &CalculatedChannel) -> Result<Compiled> {
    // Caught before evalexpr sees it. An empty equation is not a syntax error
    // to it: it compiles to an empty tree, evaluates to nothing, and the
    // complaint that comes back is about Rust types the person typing the
    // equation has never heard of and cannot act on.
    if channel.equation.trim().is_empty() {
        return Err(Error::EquationIsEmpty { channel: channel.info.name.clone() });
    }

    let expression = Expression::compile(&channel.equation).map_err(|reason| {
        Error::InvalidEquation {
            channel: channel.info.name.clone(),
            equation: channel.equation.clone(),
            reason: reason,
        }
    })?;

    // One name cannot mean two things. Both are bound before the equation is
    // evaluated, so whichever were bound last would silently win, and which
    // that is is an accident of the order they happen to be pushed in.
    if let Some(name) = channel.parameters.keys().find(|name| channel.inputs.contains_key(*name)) {
        return Err(Error::EquationNameUsedTwice {
            channel: channel.info.name.clone(),
            name: name.clone(),
        });
    }

    // An equation using a name that was never declared would otherwise fail on
    // every sample at run time, having looked fine in the config.
    let known: Vec<String> =
        channel.inputs.keys().chain(channel.parameters.keys()).cloned().collect();

    for used in expression.variables() {
        if !known.contains(&used) {
            return Err(Error::UnknownEquationInput {
                channel: channel.info.name.clone(),
                variable: used,
                declared: known.join(", "),
            });
        }
    }

    // Constants alone are not enough. Nothing about them changes, so there
    // would be no reading to work out and no moment to work it out at: an
    // input is what a calculated channel is driven by.
    if channel.inputs.is_empty() {
        return Err(Error::EquationHasNoInput { channel: channel.info.name.clone() });
    }

    // Nothing is known about the inputs until data arrives, so 1 stands in.
    // The constants are known now, though, and trying the equation with the
    // real ones is a truer rehearsal than a stand-in would be.
    let declared: Vec<(String, f64)> = channel
        .inputs
        .keys()
        .map(|variable| (variable.clone(), 1.0))
        .chain(channel.parameters.iter().map(|(name, value)| (name.clone(), *value)))
        .collect();
    expression.check(&declared).map_err(|reason| Error::InvalidEquation {
        channel: channel.info.name.clone(),
        equation: channel.equation.clone(),
        reason: reason,
    })?;

    Ok(Compiled {
        info: channel.info.clone(),
        expression: expression,
        inputs: channel
            .inputs
            .iter()
            .map(|(variable, source)| (variable.clone(), source.clone()))
            .collect(),
        parameters: channel
            .parameters
            .iter()
            .map(|(name, value)| (name.clone(), *value))
            .collect(),
        trigger: None,
        pending: VecDeque::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(name: &str, equation: &str, inputs: &[(&str, &str, &str)]) -> CalculatedChannel {
        CalculatedChannel {
            info: ChannelInfo {
                name: name.to_string(),
                unit: "-".to_string(),
            scale: None,
            },
            inputs: inputs
                .iter()
                .map(|(variable, device, source)| {
                    (
                        variable.to_string(),
                        ChannelRef { device: device.to_string(), channel: source.to_string() },
                    )
                })
                .collect(),
            parameters: BTreeMap::new(),
            equation: equation.to_string(),
        }
    }

    fn source(device: &str, channel: &str) -> ChannelRef {
        ChannelRef { device: device.to_string(), channel: channel.to_string() }
    }

    fn calculator(channels: Vec<CalculatedChannel>) -> Result<Calculator> {
        Calculator::from_config(CalculatedDevice {
            info: DeviceInfo { name: "Derived".to_string() },
            channels: channels,
        })
    }

    fn calculator_with(channels: Vec<CalculatedChannel>, rates: &[(&str, &str, u64)]) -> Calculator {
        let declared: BTreeMap<ChannelRef, Duration> = rates
            .iter()
            .map(|(device, channel, ms)| (source(device, channel), Duration::from_millis(*ms)))
            .collect();
        Calculator::with_rates(
            CalculatedDevice {
                info: DeviceInfo { name: "Derived".to_string() },
                channels: channels,
            },
            &declared,
        )
        .unwrap()
    }

    /// A batch of one channel, at offsets in milliseconds from `origin`.
    fn at(origin: DateTime<Utc>, device: &str, name: &str, samples: &[(i64, f64)]) -> Batch {
        Batch {
            device: device.to_string(),
            channel: name.to_string(),
            datapoints: samples
                .iter()
                .map(|(ms, value)| DataPoint {
                    datetime: origin + chrono::Duration::milliseconds(*ms),
                    value: *value,
                })
                .collect(),
        }
    }

    fn batch(device: &str, name: &str, values: &[f64]) -> Batch {
        let start = Utc::now();
        Batch {
            device: device.to_string(),
            channel: name.to_string(),
            datapoints: values
                .iter()
                .enumerate()
                .map(|(index, value)| DataPoint {
                    datetime: start + chrono::Duration::milliseconds(index as i64 * 10),
                    value: *value,
                })
                .collect(),
        }
    }

    fn ignore(_: DeviceEvent) {}

    fn values(batch: &Batch) -> Vec<f64> {
        batch.datapoints.iter().map(|point| point.value).collect()
    }

    // -- one input -----------------------------------------------------------

    /// The same, with named constants alongside the inputs.
    fn with_constants(
        name: &str,
        equation: &str,
        inputs: &[(&str, &str, &str)],
        constants: &[(&str, f64)],
    ) -> CalculatedChannel {
        let mut built = channel(name, equation, inputs);
        built.parameters =
            constants.iter().map(|(name, value)| (name.to_string(), *value)).collect();
        built
    }

    #[test]
    fn an_equation_may_read_a_constant_beside_its_inputs() {
        let channel = with_constants(
            "Flow",
            "(v - offset) * gain",
            &[("v", "Rig", "Raw")],
            &[("offset", 4.0), ("gain", 2.0)],
        );

        assert!(channel.validate().is_ok(), "{:?}", channel.validate().err().map(|e| e.to_string()));
    }

    #[test]
    fn a_constant_is_checked_at_its_real_value_rather_than_a_stand_in() {
        // An input stands in as 1 because nothing is known about it yet. A
        // constant is known now, so the rehearsal uses it — and this one makes
        // the equation undefined, which is a property of the number rather
        // than of the equation and so is not a fault.
        let channel = with_constants("Flow", "v / gap", &[("v", "Rig", "Raw")], &[("gap", 0.0)]);

        assert!(channel.validate().is_ok(), "dividing by a constant zero is the data's problem");
    }

    #[test]
    fn one_name_cannot_be_both_an_input_and_a_constant() {
        // Both are bound before the equation runs, so whichever were bound
        // last would quietly win.
        let channel =
            with_constants("Flow", "v * 2", &[("v", "Rig", "Raw")], &[("v", 3.0)]);

        let error = channel.validate().expect_err("that should be refused");
        assert!(matches!(error, Error::EquationNameUsedTwice { .. }), "{}", error);
        assert!(error.to_string().contains("Rename one of them"), "{}", error);
    }

    #[test]
    fn constants_alone_are_not_enough_to_drive_a_channel() {
        // Nothing about a constant changes, so there is no reading to work out
        // and no moment to work it out at.
        let channel = with_constants("Flow", "gain * 2", &[], &[("gain", 2.0)]);

        assert!(matches!(
            channel.validate().expect_err("that should be refused"),
            Error::EquationHasNoInput { .. }
        ));
    }

    #[test]
    fn a_name_that_is_neither_an_input_nor_a_constant_is_named_with_what_is() {
        let channel =
            with_constants("Flow", "v * gian", &[("v", "Rig", "Raw")], &[("gain", 2.0)]);

        let error = channel.validate().expect_err("a typo should be refused");
        let said = error.to_string();
        assert!(said.contains("gian"), "{}", said);
        // Both lists, since either would have been a fine place to declare it.
        assert!(said.contains("v") && said.contains("gain"), "{}", said);
    }

    /// The one that matters: a constant that validates but never reaches the
    /// arithmetic would pass every check above and quietly produce the wrong
    /// number for the whole of a run.
    #[test]
    fn a_constant_reaches_the_arithmetic_and_not_only_the_checks() {
        let mut calc = calculator_with(
            vec![with_constants(
                "Pressure",
                "(v - offset) * gain",
                &[("v", "ADC-20", "Input 1")],
                &[("offset", 0.5), ("gain", 12.5)],
            )],
            &[("ADC-20", "Input 1", 10)],
        );

        let produced = calc.apply(&batch("ADC-20", "Input 1", &[0.5, 1.0, 2.0]), &mut ignore);

        // The same numbers as the equation written out longhand, which is the
        // point: naming the constants changes what a person can edit, and
        // nothing about what is recorded.
        assert_eq!(values(&produced[0]), vec![0.0, 6.25, 18.75]);
    }

    #[test]
    fn an_equation_is_applied_to_every_sample() {
        let mut calc = calculator_with(
            vec![channel("Pressure", "(v - 0.5) * 12.5", &[("v", "ADC-20", "Input 1")])],
            &[("ADC-20", "Input 1", 10)],
        );
        let produced = calc.apply(&batch("ADC-20", "Input 1", &[0.5, 1.0, 2.0]), &mut ignore);
        assert_eq!(produced.len(), 1);
        assert_eq!(produced[0].device, "Derived");
        assert_eq!(values(&produced[0]), vec![0.0, 6.25, 18.75]);
    }

    #[test]
    fn output_keeps_the_input_timestamps() {
        let mut calc =
            calculator_with(vec![channel("Doubled", "v * 2", &[("v", "D", "C")])], &[("D", "C", 10)]);
        let batch = batch("D", "C", &[1.0, 2.0, 3.0]);
        let produced = calc.apply(&batch, &mut ignore);
        let times: Vec<_> = produced[0].datapoints.iter().map(|p| p.datetime).collect();
        let expected: Vec<_> = batch.datapoints.iter().map(|p| p.datetime).collect();
        assert_eq!(times, expected);
    }

    #[test]
    fn a_batch_from_another_channel_produces_nothing() {
        let mut calc =
            calculator_with(vec![channel("Doubled", "v * 2", &[("v", "D", "C")])], &[("D", "C", 10)]);
        assert!(calc.apply(&batch("D", "Other", &[1.0]), &mut ignore).is_empty());
        assert!(calc.apply(&batch("Elsewhere", "C", &[1.0]), &mut ignore).is_empty());
    }

    // -- several inputs ------------------------------------------------------

    /// Two channels of one device share timestamps exactly, so the pairing is
    /// exact and the window never comes into it.
    #[test]
    fn channels_of_one_device_pair_exactly() {
        let origin = Utc::now();
        let mut calc = calculator_with(
            vec![channel(
                "Differential",
                "high - low",
                &[("high", "Rig", "P1"), ("low", "Rig", "P2")],
            )],
            &[("Rig", "P1", 100), ("Rig", "P2", 100)],
        );
        // As a device sends them: one batch per channel, identical timestamps.
        assert!(
            calc.apply(&at(origin, "Rig", "P1", &[(0, 5.0), (100, 6.0)]), &mut ignore).is_empty(),
            "cannot resolve before the partner arrives"
        );
        let produced = calc.apply(&at(origin, "Rig", "P2", &[(0, 2.0), (100, 2.5)]), &mut ignore);
        assert_eq!(values(&produced[0]), vec![3.0, 3.5]);
    }

    /// The off-by-one this design exists to prevent. Batches arrive one channel
    /// at a time, so pairing each trigger with the other input's *latest* value
    /// would use the previous sample: 9.0 - 2.0 rather than 9.0 - 4.0.
    #[test]
    fn a_trigger_waits_rather_than_pairing_with_an_older_sample() {
        let origin = Utc::now();
        let mut calc = calculator_with(
            vec![channel("D", "a - b", &[("a", "Rig", "A"), ("b", "Rig", "B")])],
            &[("Rig", "A", 100), ("Rig", "B", 100)],
        );
        calc.apply(&at(origin, "Rig", "B", &[(0, 2.0)]), &mut ignore);
        let first = calc.apply(&at(origin, "Rig", "A", &[(0, 5.0)]), &mut ignore);
        assert_eq!(values(&first[0]), vec![3.0], "5.0 - 2.0, both at time zero");

        // A's second sample arrives before B's. Pairing it with B's latest
        // would give 9.0 - 2.0 = 7.0, using a value from a hundred
        // milliseconds earlier, so it must wait instead.
        assert!(calc.apply(&at(origin, "Rig", "A", &[(100, 9.0)]), &mut ignore).is_empty());

        let second = calc.apply(&at(origin, "Rig", "B", &[(100, 4.0)]), &mut ignore);
        assert_eq!(values(&second[0]), vec![5.0], "9.0 - 4.0, both at a hundred");
    }

    /// Two devices at the same rate never share a timestamp, but their nearest
    /// samples are close. Measured on real ones: a median of 0.5ms at 100ms.
    #[test]
    fn twin_devices_pair_on_their_nearest_samples() {
        let origin = Utc::now();
        let mut calc = calculator_with(
            vec![channel("D", "a - b", &[("a", "A", "P"), ("b", "B", "P")])],
            &[("A", "P", 100), ("B", "P", 100)],
        );
        calc.apply(&at(origin, "A", "P", &[(0, 10.0), (100, 12.0)]), &mut ignore);
        // Three milliseconds out of phase, well inside the fifty millisecond
        // window that half a period gives.
        let produced = calc.apply(&at(origin, "B", "P", &[(3, 4.0), (103, 5.0)]), &mut ignore);
        assert_eq!(values(&produced[0]), vec![6.0, 7.0]);
    }

    /// The slowest input drives the output, so the result appears at its rate
    /// and each sample pairs with a fresh value of the fast one.
    #[test]
    fn the_slowest_input_drives_the_output() {
        let origin = Utc::now();
        let mut calc = calculator_with(
            vec![channel("D", "slow + fast", &[("slow", "S", "V"), ("fast", "F", "V")])],
            &[("S", "V", 1000), ("F", "V", 100)],
        );
        let fast: Vec<(i64, f64)> = (0..=12).map(|k| (k * 100, k as f64)).collect();
        calc.apply(&at(origin, "F", "V", &fast), &mut ignore);
        let produced = calc.apply(&at(origin, "S", "V", &[(0, 100.0), (1000, 200.0)]), &mut ignore);
        // One output per slow sample, each with the fast value at that moment.
        assert_eq!(values(&produced[0]), vec![100.0, 210.0]);
    }

    /// A stopped input has nothing near the trigger, and reaching back to its
    /// last reading would be fiction.
    #[test]
    fn a_trigger_with_no_partner_nearby_produces_nothing() {
        let origin = Utc::now();
        let mut calc = calculator_with(
            vec![channel("D", "a + b", &[("a", "A", "V"), ("b", "B", "V")])],
            &[("A", "V", 100), ("B", "V", 100)],
        );
        calc.apply(&at(origin, "B", "V", &[(0, 1.0)]), &mut ignore);
        calc.apply(&at(origin, "A", "V", &[(0, 10.0)]), &mut ignore);
        let mut problems = 0;
        // A trigger long after anything B produced, then a later B sample so
        // the trigger is resolvable rather than merely waiting.
        calc.apply(&at(origin, "A", "V", &[(5000, 20.0)]), &mut ignore);
        let produced = calc.apply(&at(origin, "B", "V", &[(6000, 2.0)]), &mut |event| {
            if matches!(event, DeviceEvent::Problem { .. }) {
                problems += 1;
            }
        });
        assert_eq!(problems, 1, "should report that it could not pair");
        assert!(produced.is_empty());
    }

    /// A serial device streams at a rate nothing declares, so it is measured
    /// and nothing is produced until enough gaps have been seen.
    #[test]
    fn an_undeclared_rate_is_measured_before_anything_is_produced() {
        let origin = Utc::now();
        let mut calc = calculator_with(
            vec![channel("D", "a * b", &[("a", "Serial", "V"), ("b", "Known", "V")])],
            &[("Known", "V", 100)],
        );
        // Too few samples to know the serial rate yet.
        let early: Vec<(i64, f64)> = (0..3).map(|k| (k * 100, 2.0)).collect();
        calc.apply(&at(origin, "Serial", "V", &early), &mut ignore);
        calc.apply(&at(origin, "Known", "V", &[(0, 3.0)]), &mut ignore);
        assert!(calc.apply(&at(origin, "Known", "V", &[(100, 3.0)]), &mut ignore).is_empty());

        // Enough gaps now, so the rate is known and pairing can start.
        let later: Vec<(i64, f64)> = (3..12).map(|k| (k * 100, 2.0)).collect();
        calc.apply(&at(origin, "Serial", "V", &later), &mut ignore);
        let produced = calc.apply(&at(origin, "Known", "V", &[(500, 3.0)]), &mut ignore);
        assert!(!produced.is_empty(), "should produce once the rate is known");
        assert_eq!(values(&produced[0]), vec![6.0]);
    }

    // -- refusals ------------------------------------------------------------

    #[test]
    fn an_unbalanced_equation_is_refused_at_build() {
        let error = calculator(vec![channel("Bad", "v * (2", &[("v", "D", "C")])]).err().unwrap();
        assert!(matches!(error, Error::InvalidEquation { .. }));
    }

    /// These all parse. They only fail when evaluated, which without the trial
    /// evaluation would mean failing on every sample of a run already under way
    /// rather than being refused before it started.
    #[test]
    fn equations_that_only_fail_when_run_are_still_refused_at_build() {
        // An empty one is refused too, but earlier and by name: see
        // `an_empty_equation_says_so_in_plain_words`.
        for equation in ["v * * 2", "v +", "v $$ 2", "v 2"] {
            let result = calculator(vec![channel("Bad", equation, &[("v", "D", "C")])]);
            assert!(
                matches!(result, Err(Error::InvalidEquation { .. })),
                "{:?} should have been refused",
                equation
            );
        }
    }

    /// An equation may be undefined at the stand-in value and perfectly sound
    /// for real data, so the trial must not require a finite answer.
    #[test]
    fn an_equation_undefined_at_the_trial_value_is_still_accepted() {
        assert!(calculator(vec![channel("Ratio", "1 / (v - 1)", &[("v", "D", "C")])]).is_ok());
    }

    #[test]
    fn an_undeclared_variable_is_refused_at_build() {
        let error =
            calculator(vec![channel("Bad", "v * offset", &[("v", "D", "C")])]).err().unwrap();
        assert!(matches!(error, Error::UnknownEquationInput { .. }));
        assert!(error.to_string().contains("offset"));
    }

    #[test]
    fn an_empty_equation_says_so_in_plain_words() {
        let problem = channel("B", "", &[("v", "Rig", "Flow")])
            .validate()
            .expect_err("an empty equation cannot be worked out");
        let said = problem.to_string();

        assert!(said.contains("empty"), "{}", said);
        // evalexpr's own complaint names its Rust types, which means nothing
        // to somebody who typed an equation into a box.
        assert!(!said.contains("Value::"), "{}", said);
    }

    #[test]
    fn an_equation_with_no_input_is_refused() {
        let error = calculator(vec![channel("Constant", "42", &[])]).err().unwrap();
        assert!(matches!(error, Error::EquationHasNoInput { .. }));
    }

    /// Infinity is not a reading. It would store perfectly happily and look
    /// like data.
    #[test]
    fn a_non_finite_result_is_skipped_and_reported() {
        let mut calc =
            calculator_with(vec![channel("Ratio", "1 / v", &[("v", "D", "C")])], &[("D", "C", 10)]);
        let mut problems = 0;
        let produced = calc.apply(&batch("D", "C", &[1.0, 0.0, 4.0]), &mut |event| {
            if matches!(event, DeviceEvent::Problem { .. }) {
                problems += 1;
            }
        });
        assert_eq!(problems, 1);
        assert_eq!(values(&produced[0]), vec![1.0, 0.25]);
    }

    // -- the rest ------------------------------------------------------------

    #[test]
    fn one_measured_channel_can_feed_several_calculated_ones() {
        let mut calc = calculator_with(
            vec![
                channel("Doubled", "v * 2", &[("v", "D", "C")]),
                channel("Halved", "v / 2", &[("v", "D", "C")]),
            ],
            &[("D", "C", 10)],
        );
        let produced = calc.apply(&batch("D", "C", &[4.0]), &mut ignore);
        assert_eq!(produced.len(), 2);
        assert_eq!(produced[0].datapoints[0].value, 8.0);
        assert_eq!(produced[1].datapoints[0].value, 2.0);
    }

    /// evalexpr namespaces these as math::sqrt. Someone typing into a box in a
    /// user interface will write sqrt, so both have to work.
    #[test]
    fn the_usual_maths_works_under_its_usual_name() {
        let mut calc = calculator_with(
            vec![
                channel("Root", "sqrt(v)", &[("v", "D", "C")]),
                channel("Namespaced", "math::sqrt(v)", &[("v", "D", "C")]),
                channel("Magnitude", "abs(0 - v)", &[("v", "D", "C")]),
            ],
            &[("D", "C", 10)],
        );
        let produced = calc.apply(&batch("D", "C", &[9.0]), &mut ignore);
        let got: Vec<f64> = produced.iter().map(|b| b.datapoints[0].value).collect();
        assert_eq!(got, vec![3.0, 3.0, 9.0]);
    }

    /// The check a user interface would run behind a text box, on an equation
    /// that did not exist when this was built.
    #[test]
    fn an_equation_can_be_checked_on_its_own() {
        assert!(channel("X", "sqrt(v) * 2", &[("v", "D", "C")]).validate().is_ok());
        assert!(channel("X", "v * (2", &[("v", "D", "C")]).validate().is_err());
        assert!(channel("X", "a - b", &[("a", "D", "C"), ("b", "D", "E")]).validate().is_ok());
    }

    #[test]
    fn sources_lists_every_channel_the_equations_read() {
        let calc =
            calculator(vec![channel("D", "a - b", &[("a", "Rig", "Volts"), ("b", "Other", "Amps")])])
                .unwrap();
        let sources = calc.sources();
        assert!(sources.contains(&source("Rig", "Volts")));
        assert!(sources.contains(&source("Other", "Amps")));
    }
}
