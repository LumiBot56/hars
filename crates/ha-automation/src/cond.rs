//! Conditions (`helpers/condition.py`), and HA's `ConditionError`s, whose
//! text the warnings show (`exceptions.py`'s `output`).
//!
//! A condition with a template is checked on a render thread, with the
//! run's variables; the others read the state machine wherever they run.

use crate::config::{Condition, Tpl};
use crate::{py_float, time};
use ha_core::template::{Home, Mode, Rendered, Var, Vars};
use ha_core::{EntityId, Hass, State};
use std::fmt;
use std::sync::Arc;

/// HA's `ConditionError` tree.
#[derive(Debug, Clone)]
pub(crate) enum CondError {
    /// `ConditionErrorMessage`.
    Message { ty: &'static str, message: String },
    /// `ConditionErrorIndex`.
    Index {
        ty: &'static str,
        index: usize,
        total: usize,
        error: Box<CondError>,
    },
    /// `ConditionErrorContainer`.
    Container {
        // HA's container also names its type, which its text never shows.
        errors: Vec<CondError>,
    },
}

impl CondError {
    pub(crate) fn message(ty: &'static str, message: impl Into<String>) -> CondError {
        CondError::Message {
            ty,
            message: message.into(),
        }
    }

    fn output(&self, indent: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(indent);
        match self {
            CondError::Message { ty, message } => {
                out.push(format!("{pad}In '{ty}' condition: {message}"));
            }
            CondError::Index {
                ty,
                index,
                total,
                error,
            } => {
                if *total > 1 {
                    out.push(format!("{pad}In '{ty}' (item {} of {total}):", index + 1));
                } else {
                    out.push(format!("{pad}In '{ty}':"));
                }
                error.output(indent + 1, out);
            }
            CondError::Container { errors } => {
                for e in errors {
                    e.output(indent, out);
                }
            }
        }
    }
}

impl fmt::Display for CondError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = Vec::new();
        self.output(0, &mut lines);
        f.write_str(&lines.join("\n"))
    }
}

/// What a check rendered and logged, for the async side to write.
#[derive(Default)]
pub(crate) struct Out {
    /// (warning?, line) for `homeassistant.helpers.template`.
    pub template_log: Vec<(bool, String)>,
}

impl Out {
    /// A render's `LoggingUndefined` messages.
    pub(crate) fn note(&mut self, t: &Tpl, log: ha_core::template::Log) {
        for (level, message) in log {
            let warning = level == ha_core::template::Level::Warning;
            let kind = if warning { "warning" } else { "error" };
            self.template_log.push((
                warning,
                format!(
                    "Template variable {kind}: {message} when rendering '{}'",
                    t.source()
                ),
            ));
        }
    }

    /// Write what was logged (on the async side, in order).
    pub(crate) fn emit(&mut self) {
        for (warning, line) in self.template_log.drain(..) {
            if warning {
                tracing::warn!(logger = "homeassistant.helpers.template", "{line}");
            } else {
                tracing::error!(logger = "homeassistant.helpers.template", "{line}");
            }
        }
    }

    /// `Template.async_render(variables)`.
    pub(crate) fn render(
        &mut self,
        t: &Tpl,
        home: &Home,
        vars: &Vars,
        parse: bool,
    ) -> Result<Rendered, ha_core::template::TemplateError> {
        let r = t.render_vars(Some(home), vars, Mode::Lenient, parse);
        self.note(t, r.log);
        r.result
    }
}

/// What a template condition may read: the instance and the run's
/// variables. `None` where nothing renders.
pub(crate) type With<'a> = Option<(&'a Home, &'a Vars, &'a mut Out)>;

/// `async_numeric_state` for one entity id: `Ok(false)` for `unknown` or
/// `unavailable`; an error for an absent entity or a non-number.
pub(crate) fn numeric(
    hass: &Hass,
    eid: &str,
    below: Option<f64>,
    above: Option<f64>,
    template: Option<(&Tpl, &Home, &Vars, &mut Out)>,
) -> Result<bool, CondError> {
    let state = EntityId::parse(eid)
        .ok()
        .and_then(|id| hass.states().get(&id))
        .ok_or_else(|| CondError::message("numeric_state", format!("unknown entity {eid}")))?;
    numeric_of(&state, below, above, template)
}

/// `async_numeric_state` for a state.
pub(crate) fn numeric_of(
    state: &Arc<State>,
    below: Option<f64>,
    above: Option<f64>,
    template: Option<(&Tpl, &Home, &Vars, &mut Out)>,
) -> Result<bool, CondError> {
    let cannot = |value: &str| {
        CondError::message(
            "numeric_state",
            format!(
                "entity {} state '{value}' cannot be processed as a number",
                state.entity_id
            ),
        )
    };
    let v = match template {
        None => {
            let s = state.state.as_str();
            if matches!(s, "unavailable" | "unknown") {
                return Ok(false);
            }
            py_float(s).ok_or_else(|| cannot(s))?
        }
        Some((t, home, vars, out)) => {
            // `variables["state"] = entity`.
            let mut vars = vars.clone();
            vars.insert("state".into(), Var::state(state.clone()));
            let rendered = out
                .render(t, home, &vars, true)
                .map_err(|e| CondError::message("numeric_state", format!("template error: {e}")))?;
            let value = Var::rendered(&rendered);
            match rendered.as_text() {
                Some("unavailable" | "unknown") => return Ok(false),
                Some(s) => py_float(s).ok_or_else(|| cannot(s))?,
                None if value.is_none() => return Ok(false),
                None => value.as_f64().ok_or_else(|| cannot(&value.to_py_str()))?,
            }
        }
    };
    Ok(!(below.is_some_and(|b| v >= b) || above.is_some_and(|a| v <= a)))
}

/// `condition.async_template`: the rendered text is `true`, in any case.
fn template(t: &Tpl, with: With) -> Result<bool, CondError> {
    let Some((home, vars, out)) = with else {
        return Ok(false);
    };
    let rendered = out
        .render(t, home, vars, false)
        .map_err(|e| CondError::message("template", e.to_string()))?;
    Ok(rendered.as_text().unwrap_or_default().to_lowercase() == "true")
}

/// One condition. `trigger_id` is the run's `trigger.id` (for `trigger`
/// conditions).
pub(crate) fn check(
    hass: &Hass,
    c: &Condition,
    trigger_id: &str,
    with: With,
) -> Result<bool, CondError> {
    match c {
        Condition::NumericState {
            entity_ids,
            below,
            above,
            value_template,
        } => {
            let mut with = with;
            let mut errors = Vec::new();
            for (index, e) in entity_ids.iter().enumerate() {
                let template = match (value_template, with.as_mut()) {
                    (Some(t), Some((home, vars, out))) => Some((t, *home, *vars, &mut **out)),
                    _ => None,
                };
                match numeric(hass, e, *below, *above, template) {
                    Ok(true) => {}
                    Ok(false) => return Ok(false),
                    Err(error) => errors.push(CondError::Index {
                        ty: "numeric_state",
                        index,
                        total: entity_ids.len(),
                        error: Box::new(error),
                    }),
                }
            }
            if errors.is_empty() {
                Ok(true)
            } else {
                Err(CondError::Container { errors })
            }
        }
        Condition::State { entity_ids, states } => {
            let mut errors = Vec::new();
            for (index, e) in entity_ids.iter().enumerate() {
                match EntityId::parse(e)
                    .ok()
                    .and_then(|id| hass.states().get(&id))
                {
                    Some(s) if states.iter().any(|x| x == s.state.as_str()) => {}
                    Some(_) => return Ok(false),
                    None => errors.push(CondError::Index {
                        ty: "state",
                        index,
                        total: entity_ids.len(),
                        error: Box::new(CondError::message("state", format!("unknown entity {e}"))),
                    }),
                }
            }
            if errors.is_empty() {
                Ok(true)
            } else {
                Err(CondError::Container { errors })
            }
        }
        Condition::Time { after, before } => {
            let now = time::local_now(hass).time();
            let after = after.unwrap_or(jiff::civil::Time::midnight());
            let before = before.unwrap_or(jiff::civil::Time::MAX);
            Ok(if after < before {
                after <= now && now < before
            } else {
                !(before <= now && now < after)
            })
        }
        Condition::Trigger { ids } => Ok(ids.iter().any(|i| i == trigger_id)),
        Condition::Template(t) => template(t, with),
    }
}

/// `ConditionsChecker.async_check`: AND, stopping at the first false; an
/// error counts as false, with HA's warning (the text to log) when nothing
/// else was false.
pub(crate) fn all(
    hass: &Hass,
    conds: &[Condition],
    trigger_id: &str,
    mut with: With,
) -> (bool, Option<String>) {
    let mut errors = Vec::new();
    for (index, c) in conds.iter().enumerate() {
        let w = with.as_mut().map(|(h, v, o)| (*h, *v, &mut **o));
        match check(hass, c, trigger_id, w) {
            Ok(true) => {}
            Ok(false) => return (false, None),
            Err(error) => errors.push(CondError::Index {
                ty: "condition",
                index,
                total: conds.len(),
                error: Box::new(error),
            }),
        }
    }
    if errors.is_empty() {
        return (true, None);
    }
    let container = CondError::Container { errors };
    (false, Some(container.to_string()))
}

/// `_ScriptRun._test_conditions` (a `choose` option's): AND, stopping at
/// the first false or the first error, which is logged (`name[idx]`).
pub(crate) fn test(
    hass: &Hass,
    conds: &[Condition],
    trigger_id: &str,
    name: &str,
    mut with: With,
) -> (bool, Option<String>) {
    for (index, c) in conds.iter().enumerate() {
        let w = with.as_mut().map(|(h, v, o)| (*h, *v, &mut **o));
        match check(hass, c, trigger_id, w) {
            Ok(true) => {}
            Ok(false) => return (false, None),
            Err(e) => {
                return (
                    false,
                    Some(format!("Error in '{name}[{index}]' evaluation: {e}")),
                );
            }
        }
    }
    (true, None)
}
