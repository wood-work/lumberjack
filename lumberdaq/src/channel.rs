use crate::equation::Expression;
use crate::{ Error, Result };
use crate::datapoint::DataPoint;
use crate::storage::Batch;
use chrono;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The name a scale reads its channel's own measurement under.
const VARIABLE: &str = "x";

/// What an analog input measures, before anything is done to it.
const VOLTS: &str = "V";

/// Spellings of it that somebody might reasonably write.
const VOLT_SPELLINGS: [&str; 3] = ["v", "volt", "volts"];

/// Where the measurement sits among a scale's values. The constants follow it.
const VALUE: usize = 0;

/// How a channel's readings are turned into the unit it reports.
///
/// Both forms come to the same thing at run time. The difference is whether the
/// numbers stay visible: writing `x / 120` dissolves the shunt resistor into
/// the arithmetic where nothing can find it again, so changing the resistor
/// means working the equation out afresh and no saved project can say what
/// sensor it was for. Naming the constants keeps them editable.
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Scale {
    /// An equation over the measurement alone.
    ///
    /// ```json
    /// "scale": "x * 5 + 5"
    /// ```
    Equation(String),

    /// An equation over the measurement and some named constants.
    ///
    /// ```json
    /// "scale": {
    ///   "from": "4-20 mA transmitter",
    ///   "equation": "(((x / shunt_ohms) * 1000 - 4) / 16) * (high - low) + low",
    ///   "parameters": { "shunt_ohms": 120, "low": 0, "high": 29 }
    /// }
    /// ```
    ///
    /// The equation is copied in rather than referred to, so a project still
    /// runs when whatever library `from` names is not to hand. `from` is a
    /// label and nothing more: it says which sensor definition these numbers
    /// came from, which is what an interface needs to offer the right form for
    /// editing them.
    Parameterised {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        equation: String,
        #[serde(default)]
        parameters: BTreeMap<String, f64>,
    },
}

impl Scale {
    /// The equation, whichever form this is.
    pub fn equation(&self) -> &str {
        match self {
            Scale::Equation(equation) => equation,
            Scale::Parameterised { equation, .. } => equation,
        }
    }

    /// Which sensor definition the constants came from, if it was recorded.
    pub fn from(&self) -> Option<&str> {
        match self {
            Scale::Equation(_) => None,
            Scale::Parameterised { from, .. } => from.as_deref(),
        }
    }

    /// The named constants, for an interface to show and edit.
    ///
    /// `None` rather than an empty map: an equation written out by hand has no
    /// parameters to edit, which is not the same as having none filled in.
    pub fn parameters(&self) -> Option<&BTreeMap<String, f64>> {
        match self {
            Scale::Equation(_) => None,
            Scale::Parameterised { parameters, .. } => Some(parameters),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
/// What a channel is called and what its numbers mean.
///
/// Note there is no id here. Which input a channel reads is recorded by the
/// hardware config, in the one place that binding is defined; a second
/// identifier alongside the name only invited confusion with the row ids in a
/// results database.
pub struct ChannelInfo {
    pub name: String,
    /// What the recorded value is in.
    ///
    /// Optional, because hardware that only ever measures one thing can say so
    /// itself: an analog input reads volts and there is no sense in making
    /// somebody type that for every channel. Backends that cannot know leave it
    /// as it was given, since guessing a unit is worse than having none.
    #[serde(default)]
    pub unit: String,
    /// How to turn the raw measurement into the unit named above.
    ///
    /// The measurement is written `x`. Either an equation on its own, or one
    /// with named constants — see [`Scale`].
    ///
    /// The scaled value is what gets recorded and the raw reading is not kept.
    /// That is the point of it, since nobody wants volts from a flow meter, but
    /// it does mean this is the only way back to the measurement. It is written
    /// into the results with the rest of the config for exactly that reason.
    ///
    /// Use a calculated channel instead when a value needs more than one input;
    /// those have to reconcile timestamps, which this does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
    /// Whether this channel is read at all.
    ///
    /// A channel that is switched off stays in the setup, keeping its name,
    /// its binding and its scale, and simply produces nothing: nothing
    /// recorded, nothing plotted, nothing said about it. Somewhere between
    /// deleting it and living with it.
    ///
    /// Defaulted to on and left out of the file when it is, so a config that
    /// has never heard of this reads exactly as before and one written now
    /// only mentions the channels somebody has actually switched off.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub enabled: bool,
}

/// The default for `enabled`, which serde needs as a function.
pub(crate) fn on() -> bool {
    true
}

/// Whether to leave `enabled` out of the file, which serde also needs as one.
pub(crate) fn is_on(enabled: &bool) -> bool {
    *enabled
}

impl Default for ChannelInfo {
    /// Written out rather than derived, because a derived `bool` is `false`
    /// and a channel that defaulted to switched off would be a trap: every
    /// `..Default::default()` in the codebase would quietly stop recording.
    fn default() -> ChannelInfo {
        ChannelInfo {
            name: String::new(),
            unit: String::new(),
            scale: None,
            enabled: true,
        }
    }
}

pub struct Channel {
    pub info: ChannelInfo,
    /// `info.scale`, parsed once when the channel is built rather than on
    /// every reading.
    scale: Option<CompiledScale>,
    pub datapoints: Vec<DataPoint>,
    pub datapoint_last: Option<DataPoint>,
}

impl ChannelInfo {
    /// Settle the unit of a channel whose hardware only ever measures volts.
    ///
    /// Left out, it becomes volts, so a Pico or NI channel need not say what
    /// every one of them measures.
    ///
    /// Given as something else, it depends on whether the channel is scaled. A
    /// scale is what turns volts into litres per minute, so the unit follows it
    /// and any unit is right. Without one, the recorded number *is* the
    /// voltage, and calling it bar would make every plot and every exported
    /// column say something untrue about data nothing had converted.
    pub fn settle_voltage_unit(&mut self, channel: &str) -> Result<()> {
        if self.unit.trim().is_empty() {
            self.unit = VOLTS.to_string();
            return Ok(());
        }
        if self.scale.is_some() {
            return Ok(());
        }
        let given = self.unit.trim().to_lowercase();
        match VOLT_SPELLINGS.contains(&given.as_str()) {
            true => Ok(()),
            false => Err(Error::UnitIsNotVolts {
                channel: channel.to_string(),
                unit: self.unit.clone(),
            }),
        }
    }
}

impl Channel {
    // pub fn read(&mut self) -> Result<()> {
    //     let mut datapoints = self.config.read()?;
    //     self.data.add_datapoints(&mut datapoints)?;
    //     Ok(())
    // }
    pub fn new(name: String, unit: String) -> Channel {
        Channel {
            info: ChannelInfo {
                name: name,
                unit: unit,
                scale: None,
                enabled: true,
            },
            scale: None,
            datapoints: vec![],
            datapoint_last: None,
        }
    }

    /// Start an empty channel from its description in a config.
    ///
    /// Fails if the channel declares a scale that could not be used, which is
    /// the point at which to find out: better here, while a run is being set
    /// up, than on the first reading of one already under way.
    pub fn from_info(info: ChannelInfo) -> Result<Channel> {
        let scale = match &info.scale {
            Some(equation) => Some(compile_scale(&info.name, equation)?),
            None => None,
        };
        Ok(Channel {
            info: info,
            scale: scale,
            datapoints: vec![],
            datapoint_last: None,
        })
    }

    /// Take readings from the hardware, scaled if this channel has a scale.
    ///
    /// A reading that will not scale is left out rather than stored as it
    /// came, since the channel's unit says it has been converted. The rest are
    /// kept and the omission is reported afterwards: one unusable reading is no
    /// reason to discard the good ones that arrived with it.
    /// Rebuild a channel from stored results.
    ///
    /// No scale is attached. Values were scaled on the way in, so whatever was
    /// read back is already in the channel's stated unit; scaling it again
    /// would convert twice.
    pub fn from_stored(info: ChannelInfo, datapoints: Vec<DataPoint>) -> Channel {
        Channel {
            info: info,
            scale: None,
            datapoint_last: datapoints.last().copied(),
            datapoints: datapoints,
        }
    }

    pub fn add_datapoints(&mut self, datapoints: &mut Vec<DataPoint>) -> Result<()> {
        let mut skipped = 0;
        let mut reason = String::new();
        if let Some(scale) = &self.scale {
            // The constants never change, so the list is built once per read
            // and only the measurement in slot 0 is rewritten per reading.
            let mut values = scale.values.clone();
            datapoints.retain_mut(|point| {
                values[VALUE] = (VARIABLE.to_string(), point.value);
                match scale.expression.evaluate(&values) {
                    Ok(value) => {
                        point.value = value;
                        true
                    }
                    Err(cause) => {
                        skipped += 1;
                        if reason.is_empty() {
                            reason = cause;
                        }
                        false
                    }
                }
            });
        }
        self.datapoints.append(datapoints);
        self.datapoint_last = self.datapoints.last().copied();
        if skipped > 0 {
            return Err(Error::ScaleFailed {
                channel: self.info.name.clone(),
                scale: self.info.scale.as_ref().map(Scale::equation).unwrap_or_default().to_string(),
                skipped: skipped,
                reason: reason,
            });
        }
        Ok(())
    }

    pub fn latest_as_string(&self) -> String {
        match self.datapoint_last {
            Some(data) => format!("{}: {}, {} {}", self.info.name, data.datetime, data.value, self.info.unit),
            None => format!("{}: No data", self.info.name)
        }
    }

    /// Hand off everything acquired so far, leaving the channel's buffer empty.
    ///
    /// `mem::take` swaps in an empty Vec and returns the old one, so the
    /// datapoints move into the batch rather than being copied.
    pub fn drain_batch(&mut self, device_name: &str) -> Batch {
        Batch {
            device: device_name.to_string(),
            channel: self.info.name.clone(),
            datapoints: std::mem::take(&mut self.datapoints),
        }
    }

    pub fn datapoints_as_vectors(&self) -> Result<(Vec<chrono::DateTime<chrono::Utc>>, Vec<f64>)> {
        let mut datetimes: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
        let mut values: Vec<f64> = Vec::new();
        for datapoint in self.datapoints.iter() {
            datetimes.push(datapoint.datetime);
            values.push(datapoint.value);
        }
        return Ok((datetimes, values));
    }
}
/// A channel's scale, parsed and with its constants laid out ready to use.
struct CompiledScale {
    expression: Expression,
    /// The measurement at [`VALUE`], then every constant. Copied per read so
    /// the measurement can be written into it without re-collecting the rest.
    values: Vec<(String, f64)>,
}

/// Parse a channel's scale, checking it the way a run would use it.
fn compile_scale(channel: &str, scale: &Scale) -> Result<CompiledScale> {
    let equation = scale.equation();
    let invalid = |reason: String| Error::InvalidScale {
        channel: channel.to_string(),
        scale: equation.to_string(),
        reason: reason,
    };

    let expression = Expression::compile(equation).map_err(invalid)?;

    // The measurement first, so a reading can be written into a known slot.
    let mut values = vec![(VARIABLE.to_string(), 1.0)];
    if let Some(parameters) = scale.parameters() {
        values.extend(parameters.iter().map(|(name, value)| (name.clone(), *value)));
    }

    // A name that will never be set would otherwise look fine in a config and
    // then fail on every reading. The likeliest cause by far is a typo, so the
    // message lists what there was to choose from.
    for used in expression.variables() {
        if !values.iter().any(|(name, _)| name == &used) {
            return Err(Error::UnknownScaleVariable {
                channel: channel.to_string(),
                variable: used,
                available: values
                    .iter()
                    .enumerate()
                    .map(|(position, (name, _))| match position {
                        VALUE => format!("{} (the measurement)", name),
                        _ => name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                scale: equation.to_string(),
            });
        }
    }

    // Rehearsed with the real constants, since those are settled now. Only the
    // measurement is a stand-in.
    expression.check(&values).map_err(invalid)?;

    Ok(CompiledScale { expression: expression, values: values })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4-20 mA transmitter reading 0-29 L/min across a 120 ohm shunt, with
    /// every number that describes it still named.
    fn flow_meter() -> Scale {
        Scale::Parameterised {
            from: Some("4-20 mA transmitter".to_string()),
            equation: "(((x / shunt_ohms) * 1000 - 4) / 16) * (high - low) + low".to_string(),
            parameters: [
                ("shunt_ohms".to_string(), 120.0),
                ("low".to_string(), 0.0),
                ("high".to_string(), 29.0),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn info(scale: Option<Scale>) -> ChannelInfo {
        ChannelInfo {
            name: "Flow".to_string(),
            unit: "L/min".to_string(),
            scale: scale,
            enabled: true,
        }
    }

    fn equation(text: &str) -> Option<Scale> {
        Some(Scale::Equation(text.to_string()))
    }

    fn readings(values: &[f64]) -> Vec<DataPoint> {
        values
            .iter()
            .map(|value| DataPoint { datetime: chrono::Utc::now(), value: *value })
            .collect()
    }

    fn values_of(channel: &Channel) -> Vec<f64> {
        channel.datapoints.iter().map(|point| point.value).collect()
    }

    fn refusal(scale: Scale) -> Error {
        match Channel::from_info(info(Some(scale))) {
            Ok(_) => panic!("a scale that should have been refused was accepted"),
            Err(error) => error,
        }
    }

    #[test]
    fn an_analog_input_need_not_say_it_measures_volts() {
        let mut given = info(None);
        given.unit = String::new();
        given.settle_voltage_unit("Dev1/ai0").unwrap();
        assert_eq!(given.unit, "V");
    }

    #[test]
    fn however_it_is_spelled() {
        for spelling in ["V", "v", "Volts", "volt", " V "] {
            let mut given = info(None);
            given.unit = spelling.to_string();
            assert!(given.settle_voltage_unit("Dev1/ai0").is_ok(), "{:?} refused", spelling);
        }
    }

    #[test]
    fn an_unscaled_input_may_not_claim_to_measure_something_else() {
        // The recorded number is the voltage. Calling it bar would make every
        // plot and every exported column say something untrue.
        let mut given = info(None);
        given.unit = "bar".to_string();
        let error = given.settle_voltage_unit("Dev1/ai0").unwrap_err();
        assert!(matches!(error, Error::UnitIsNotVolts { .. }), "{}", error);
        // And the message has to say how to do the thing they meant.
        assert!(error.to_string().contains("scale"), "{}", error);
    }

    #[test]
    fn a_scaled_input_may_say_whatever_the_scale_produces() {
        // Which is the whole point of a scale.
        let mut given = info(equation("x * 5"));
        given.unit = "bar".to_string();
        assert!(given.settle_voltage_unit("Dev1/ai0").is_ok());
        assert_eq!(given.unit, "bar");
    }

    #[test]
    fn without_a_scale_the_reading_is_stored_as_it_came() {
        let mut channel = Channel::from_info(info(None)).unwrap();
        channel.add_datapoints(&mut readings(&[1.5, 2.5])).unwrap();
        assert_eq!(values_of(&channel), vec![1.5, 2.5]);
    }

    #[test]
    fn a_plain_equation_replaces_the_raw_value() {
        let mut channel = Channel::from_info(info(equation("x * 5 + 5"))).unwrap();
        channel.add_datapoints(&mut readings(&[-1.0, 1.0])).unwrap();
        assert_eq!(values_of(&channel), vec![0.0, 10.0]);
    }

    #[test]
    fn named_constants_are_bound_alongside_the_measurement() {
        // 0.48 V is 4 mA, the bottom of the range; 2.4 V is 20 mA, the top.
        let mut channel = Channel::from_info(info(Some(flow_meter()))).unwrap();
        channel.add_datapoints(&mut readings(&[0.48, 2.4])).unwrap();
        let values = values_of(&channel);
        assert!((values[0] - 0.0).abs() < 1e-9, "bottom of range: {}", values[0]);
        assert!((values[1] - 29.0).abs() < 1e-9, "top of range: {}", values[1]);
        // And nothing holds the volts it was worked out from.
        assert!(!values.contains(&0.48));
    }

    #[test]
    fn a_constant_can_be_changed_without_touching_the_equation() {
        // The whole point of naming them: refitting a 100 ohm shunt is one
        // number, not a re-derivation.
        let mut scale = flow_meter();
        if let Scale::Parameterised { parameters, .. } = &mut scale {
            parameters.insert("shunt_ohms".to_string(), 100.0);
        }
        let mut channel = Channel::from_info(info(Some(scale))).unwrap();
        // 20 mA across 100 ohms is 2.0 V, and still the top of the range.
        channel.add_datapoints(&mut readings(&[2.0])).unwrap();
        assert!((values_of(&channel)[0] - 29.0).abs() < 1e-9);
    }

    #[test]
    fn the_constants_are_there_for_an_interface_to_edit() {
        let scale = flow_meter();
        assert_eq!(scale.from(), Some("4-20 mA transmitter"));
        assert_eq!(scale.parameters().unwrap().get("shunt_ohms"), Some(&120.0));
        // An equation written by hand has nothing to put in a form, which is
        // not the same as a sensor whose form is empty.
        assert!(Scale::Equation("x * 2".to_string()).parameters().is_none());
        assert!(Scale::Equation("x * 2".to_string()).from().is_none());
    }

    #[test]
    fn the_latest_value_is_the_scaled_one() {
        let mut channel = Channel::from_info(info(equation("x * 2"))).unwrap();
        channel.add_datapoints(&mut readings(&[3.0])).unwrap();
        assert_eq!(channel.datapoint_last.unwrap().value, 6.0);
    }

    #[test]
    fn a_reading_that_will_not_scale_is_left_out_but_the_rest_are_kept() {
        // 1/x is fine except at zero, which is exactly the case worth handling:
        // failing the whole read would throw away the two sound readings too.
        let mut channel = Channel::from_info(info(equation("1 / x"))).unwrap();
        let error = channel.add_datapoints(&mut readings(&[2.0, 0.0, 4.0])).unwrap_err();
        assert!(matches!(error, Error::ScaleFailed { skipped: 1, .. }), "{}", error);
        assert_eq!(values_of(&channel), vec![0.5, 0.25]);
    }

    #[test]
    fn a_scale_using_a_name_it_has_no_value_for_is_refused_when_the_channel_is_built() {
        // The likeliest typo of the lot, and it must not wait for a run.
        let error = refusal(Scale::Equation("v * 2".to_string()));
        assert!(matches!(error, Error::UnknownScaleVariable { .. }), "{}", error);
        // A misspelled constant is the same fault, and the message has to name
        // what was on offer or there is no way to spot the difference.
        let error = refusal(Scale::Parameterised {
            from: None,
            equation: "x / shunt".to_string(),
            parameters: [("shunt_ohms".to_string(), 120.0)].into_iter().collect(),
        });
        let text = error.to_string();
        assert!(text.contains("shunt_ohms"), "{}", text);
        assert!(text.contains("x (the measurement)"), "{}", text);
    }

    #[test]
    fn a_scale_that_cannot_be_parsed_or_evaluated_is_refused() {
        for text in ["x * (2", "x * * 2", "", "wibble(x)"] {
            let result = Channel::from_info(info(equation(text)));
            assert!(result.is_err(), "{:?} was accepted", text);
        }
    }

    #[test]
    fn stored_results_are_not_scaled_a_second_time() {
        // Values read back from a file went in already converted.
        let channel = Channel::from_stored(info(equation("x * 2")), readings(&[10.0]));
        assert_eq!(channel.datapoint_last.unwrap().value, 10.0);
    }

    #[test]
    fn both_forms_survive_a_round_trip_through_the_config() {
        // A bare string stays a bare string, so the simple case stays simple
        // and configs written before parameters existed still load.
        let text = serde_json::to_string(&info(equation("x * 2 + 1"))).unwrap();
        assert!(text.contains("\"scale\":\"x * 2 + 1\""), "{}", text);
        let back: ChannelInfo = serde_json::from_str(&text).unwrap();
        assert!(matches!(back.scale, Some(Scale::Equation(ref e)) if e == "x * 2 + 1"));

        let text = serde_json::to_string(&info(Some(flow_meter()))).unwrap();
        let back: ChannelInfo = serde_json::from_str(&text).unwrap();
        let scale = back.scale.unwrap();
        assert_eq!(scale.from(), Some("4-20 mA transmitter"));
        assert_eq!(scale.parameters().unwrap().get("high"), Some(&29.0));

        // An unscaled channel writes no scale at all, so existing configs and
        // the files they produce are unchanged.
        let plain = serde_json::to_string(&info(None)).unwrap();
        assert!(!plain.contains("scale"), "{}", plain);
    }
}
