//! Equations written as text and checked before anything is recorded.
//!
//! Two things in a config are equations over channel values: a calculated
//! channel combining several inputs, and a scale converting one channel's
//! readings into the units someone actually wants. Both are strings read at run
//! time, so both need the same care — parse it, check the names it uses are
//! ones that will be supplied, and try it once before a run depends on it.
//!
//! This holds that shared part. What the variables mean, and what to do when
//! one sample fails, belong to the caller.

use evalexpr::{ ContextWithMutableFunctions, ContextWithMutableVariables };

type Tree = evalexpr::Node<evalexpr::DefaultNumericTypes>;
type Context = evalexpr::HashMapContext<evalexpr::DefaultNumericTypes>;

/// A parsed equation, ready to be given values.
///
/// Errors here are plain strings rather than the crate's error type: the same
/// fault reads differently depending on whether it came from a calculated
/// channel or a scale, so the caller supplies that context.
pub struct Expression {
    tree: Tree,
}

impl Expression {
    /// Parse an equation, without yet knowing what will be put into it.
    pub fn compile(equation: &str) -> Result<Expression, String> {
        let tree = evalexpr::build_operator_tree::<evalexpr::DefaultNumericTypes>(equation)
            .map_err(|error| error.to_string())?;
        Ok(Expression { tree: tree })
    }

    /// Every variable name the equation reads.
    ///
    /// The caller checks these against the names it intends to supply. A name
    /// that will never be set would otherwise look fine in a config and then
    /// fail on every sample of the run.
    pub fn variables(&self) -> Vec<String> {
        self.tree.iter_variable_identifiers().map(|name| name.to_string()).collect()
    }

    /// Try the equation once before a run depends on it.
    ///
    /// Building the tree only rejects structural faults such as an unmatched
    /// parenthesis. An operator missing an argument, a misspelled function and
    /// an empty equation all parse perfectly well and fail on evaluation, which
    /// without this would mean failing on every sample of a run that had
    /// already started.
    ///
    /// Pass whatever real values are known — a scale's constants are fixed at
    /// config time, so trying it with those is a truer rehearsal than a
    /// stand-in would be. Anything not yet known can be 1.
    ///
    /// A non-finite answer is not a fault in the equation: plenty of sound ones
    /// are undefined at the value tried. That is a property of the data, and is
    /// caught per sample instead.
    pub fn check(&self, values: &[(String, f64)]) -> Result<(), String> {
        match self.evaluate(values) {
            Ok(_) => Ok(()),
            Err(reason) if reason.starts_with("gave ") => Ok(()),
            Err(reason) => Err(reason),
        }
    }

    /// Evaluate one set of values, refusing anything that is not a real number.
    ///
    /// A division by zero gives infinity rather than an error, and it would be
    /// stored quite happily, so it is caught here: a reading of `inf` looks
    /// like data and is not.
    pub fn evaluate(&self, values: &[(String, f64)]) -> Result<f64, String> {
        let mut context = fresh_context();
        for (variable, value) in values.iter() {
            context
                .set_value(variable.as_str().into(), evalexpr::Value::from_float(*value))
                .map_err(|error| error.to_string())?;
        }
        let value = self.tree.eval_with_context(&context).map_err(|error| error.to_string())?;
        let number = value.as_number().map_err(|error| error.to_string())?;
        if !number.is_finite() {
            return Err(format!("gave {}", number));
        }
        Ok(number)
    }
}

/// A context with the usual maths available under its usual name.
///
/// evalexpr provides these as `math::sqrt` and so on. Someone typing an
/// equation into a box expects `sqrt(v)`, not `math::sqrt(v)`, so the plain
/// names are bound here as well. The prefixed forms keep working.
///
/// Both arities, and for a reason worth writing down. When only the one
/// argument functions were aliased, `sqrt(x)` worked and `pow(a, b)` did not,
/// so a help page had to explain that some functions want a prefix and others
/// refuse one — a distinction with nothing behind it but which table they
/// happened to be in. evalexpr's own `min`, `max`, `floor` and `ceil` are
/// already plain and are left alone.
fn fresh_context() -> Context {
    let mut context = Context::new();
    let unary: [(&str, fn(f64) -> f64); 12] = [
        ("sqrt", f64::sqrt),
        ("abs", f64::abs),
        ("ln", f64::ln),
        ("log10", f64::log10),
        ("exp", f64::exp),
        ("sin", f64::sin),
        ("cos", f64::cos),
        ("tan", f64::tan),
        ("asin", f64::asin),
        ("acos", f64::acos),
        ("atan", f64::atan),
        ("round", f64::round),
    ];
    for (name, function) in unary {
        // set_function only fails on a name that is not a valid identifier, and
        // these are all literals, so there is nothing to handle.
        let _ = context.set_function(
            name.to_string(),
            evalexpr::Function::new(move |argument| {
                let value = argument.as_number()?;
                Ok(evalexpr::Value::from_float(function(value)))
            }),
        );
    }

    // The order is the one the name implies: `log(x, base)`, `atan2(y, x)`.
    let binary: [(&str, fn(f64, f64) -> f64); 4] = [
        ("pow", f64::powf),
        ("log", f64::log),
        ("hypot", f64::hypot),
        ("atan2", f64::atan2),
    ];
    for (name, function) in binary {
        let _ = context.set_function(
            name.to_string(),
            evalexpr::Function::new(move |argument| {
                let pair = argument.as_fixed_len_tuple(2)?;
                let first = pair[0].as_number()?;
                let second = pair[1].as_number()?;
                Ok(evalexpr::Value::from_float(function(first, second)))
            }),
        );
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// Variable names, all set to 1, for checking an equation's shape.
    fn stand_ins(names: &[&str]) -> Vec<(String, f64)> {
        names.iter().map(|name| (name.to_string(), 1.0)).collect()
    }

    /// Every function the interface's help offers, tried under the plain name
    /// it offers them under. The help is written from this list, so a name
    /// that stops working here is a help page that starts lying.
    #[test]
    fn every_function_works_without_a_prefix() {
        let cases: [(&str, f64); 20] = [
            ("sqrt(9)", 3.0),
            ("abs(-3)", 3.0),
            ("round(1.5)", 2.0),
            ("floor(1.7)", 1.0),
            ("ceil(1.2)", 2.0),
            ("ln(1)", 0.0),
            ("log10(100)", 2.0),
            ("exp(0)", 1.0),
            ("sin(0)", 0.0),
            ("cos(0)", 1.0),
            ("tan(0)", 0.0),
            ("asin(0)", 0.0),
            ("acos(1)", 0.0),
            ("atan(0)", 0.0),
            ("min(2, 5)", 2.0),
            ("max(2, 5)", 5.0),
            // The two argument ones, which for a long time answered only to a
            // `math::` prefix while their neighbours refused one.
            ("pow(2, 3)", 8.0),
            ("log(8, 2)", 3.0),
            ("hypot(3, 4)", 5.0),
            ("atan2(0, 1)", 0.0),
        ];

        for (equation, expected) in cases {
            let value = Expression::compile(equation)
                .unwrap_or_else(|why| panic!("{} did not compile: {}", equation, why))
                .evaluate(&[])
                .unwrap_or_else(|why| panic!("{} did not evaluate: {}", equation, why));
            assert!(
                (value - expected).abs() < 1e-9,
                "{} gave {} rather than {}",
                equation,
                value,
                expected
            );
        }
    }

    /// The prefixed spelling was what worked before the plain names existed, so
    /// an equation already written with one has to keep working.
    #[test]
    fn the_prefixed_spelling_still_works() {
        for equation in ["math::sqrt(9)", "math::pow(2, 3)"] {
            assert!(Expression::compile(equation).unwrap().evaluate(&[]).is_ok(), "{}", equation);
        }
    }

    #[test]
    fn evaluates_with_supplied_values() {
        let expression = Expression::compile("(x + 1) * 2").unwrap();
        let value = expression.evaluate(&[("x".to_string(), 3.0)]).unwrap();
        assert_eq!(value, 8.0);
    }

    #[test]
    fn reports_the_variables_it_reads() {
        let expression = Expression::compile("a * b + 2").unwrap();
        let mut used = expression.variables();
        used.sort();
        assert_eq!(used, variables(&["a", "b"]));
    }

    #[test]
    fn plain_maths_names_work_as_typed() {
        let expression = Expression::compile("sqrt(x)").unwrap();
        assert_eq!(expression.evaluate(&[("x".to_string(), 9.0)]).unwrap(), 3.0);
    }

    #[test]
    fn refuses_a_non_finite_result() {
        let expression = Expression::compile("1 / x").unwrap();
        let error = expression.evaluate(&[("x".to_string(), 0.0)]).unwrap_err();
        assert!(error.starts_with("gave "), "unexpected error: {}", error);
    }

    #[test]
    fn check_catches_what_parsing_misses() {
        // All of these parse, and all of them fail on evaluation.
        for equation in ["x * * 2", "x +", "", "wibble(x)"] {
            let expression = match Expression::compile(equation) {
                Ok(expression) => expression,
                Err(_) => continue, // rejected earlier, which is also fine
            };
            assert!(
                expression.check(&stand_ins(&["x"])).is_err(),
                "{:?} passed the check",
                equation
            );
        }
    }

    #[test]
    fn check_uses_the_values_it_is_given() {
        // A misspelled function is only reachable once the constants it sits
        // among are supplied, and those are fixed at config time.
        let expression = Expression::compile("wibble(x * gain)").unwrap();
        let result = expression.check(&[("x".to_string(), 1.0), ("gain".to_string(), 2.0)]);
        assert!(result.is_err());
    }

    #[test]
    fn check_allows_an_equation_undefined_at_the_stand_in_value() {
        // Sound, but 1/(1-1) is infinite. That is the data's problem, not the
        // equation's, so it must not be rejected here.
        let expression = Expression::compile("1 / (x - 1)").unwrap();
        assert!(expression.check(&stand_ins(&["x"])).is_ok());
    }
}
