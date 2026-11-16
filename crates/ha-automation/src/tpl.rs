//! Rendering an automation's templates as HA's scripts do
//! (`template.render_complex`, `ScriptVariables.async_render`,
//! `service.async_prepare_call_from_config`). These run on a render thread
//! ([`ha_core::template::run`]); what they log comes back in [`Out`].

use crate::cond::Out;
use crate::config::{Complex, Service, Variable};
use ha_core::template::{Home, TemplateError, Var, Vars};
use serde_json::{Map, Value};

/// `render_complex(value, variables)`.
pub(crate) fn complex(
    c: &Complex,
    home: &Home,
    vars: &Vars,
    out: &mut Out,
) -> Result<Var, TemplateError> {
    Ok(match c {
        Complex::Static(v) => Var::json(v),
        Complex::Template(t) => Var::rendered(&out.render(t, home, vars, true)?),
        Complex::List(items) => Var::list(
            items
                .iter()
                .map(|i| complex(i, home, vars, out))
                .collect::<Result<_, _>>()?,
        ),
        Complex::Map(pairs) => {
            let mut entries = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                let key = complex(k, home, vars, out)?.to_py_str();
                entries.push((key, complex(v, home, vars, out)?));
            }
            Var::dict(entries)
        }
    })
}

/// [`complex`] as JSON: a static value as it is.
fn complex_json(
    c: &Complex,
    home: &Home,
    vars: &Vars,
    out: &mut Out,
) -> Result<Value, TemplateError> {
    match c {
        Complex::Static(v) => Ok(v.clone()),
        other => Ok(complex(other, home, vars, out)?.to_json()),
    }
}

/// `ScriptVariables.async_render(run_variables)`: each variable in order,
/// rendered with the ones before it, unless the run already has it.
pub(crate) fn variables(
    defs: &[Variable],
    mut run: Vars,
    home: &Home,
    out: &mut Out,
) -> Result<Vars, TemplateError> {
    for v in defs {
        if run.contains_key(&v.name) {
            continue;
        }
        let value = complex(&v.value, home, &run, out)?;
        run.insert(v.name.clone(), value);
    }
    Ok(run)
}

/// The variables of [`variables`] that need no render, as values.
pub(crate) fn static_variables(defs: &[Variable], run: &mut Vars) {
    for v in defs {
        if let Complex::Static(value) = &v.value {
            run.entry(v.name.clone())
                .or_insert_with(|| Var::json(value));
        }
    }
}

/// A service call, ready to make.
pub(crate) struct Prepared {
    pub domain: String,
    pub service: String,
    pub data: Map<String, Value>,
    pub target: Option<Map<String, Value>>,
}

/// `async_prepare_call_from_config`: the service name, the target, then
/// the data, rendered; the error is HA's `HomeAssistantError` text.
pub(crate) fn prepare(
    service: &Service,
    data: &[Complex],
    target: Option<&Complex>,
    home: &Home,
    vars: &Vars,
    out: &mut Out,
) -> Result<Prepared, String> {
    let (domain, service) = match service {
        Service::Static { domain, service } => (domain.clone(), service.clone()),
        Service::Template(t) => {
            let rendered = out
                .render(t, home, vars, true)
                .map_err(|e| format!("Error rendering service name template: {e}"))?;
            let value = Var::rendered(&rendered);
            // `cv.service`: a string (not a list or dict), lower-cased, a
            // valid `domain.service`.
            let name = (!value.is_dict() && value.to_json().as_array().is_none())
                .then(|| value.to_py_str().to_lowercase())
                .filter(|s| ha_core::valid_entity_id(s));
            let Some(name) = name else {
                return Err(format!(
                    "Template rendered invalid service: {}",
                    value.to_py_str()
                ));
            };
            let (d, s) = name.split_once('.').unwrap_or((&name, ""));
            (d.to_owned(), s.to_owned())
        }
    };
    let target = match target {
        None => None,
        Some(Complex::Static(Value::Object(t))) => Some(t.clone()),
        Some(c) => {
            let rendered = complex(c, home, vars, out)
                .map_err(|e| format!("Error rendering service target template: {e}"))?;
            let mut t = match rendered.to_json() {
                Value::Object(t) => t,
                other => {
                    return Err(format!(
                        "Error rendering service target template: {} is not a dictionary",
                        Var::json(&other).to_py_str()
                    ));
                }
            };
            if let Some(ids) = t.get("entity_id") {
                let shown = Var::json(ids).to_py_str();
                let ids = comp_entity_ids(ids)
                    .ok_or_else(|| format!("Template rendered invalid entity IDs: {shown}"))?;
                t.insert("entity_id".into(), ids);
            }
            Some(t)
        }
    };
    let mut service_data = Map::new();
    for d in data {
        let rendered = complex_json(d, home, vars, out)
            .map_err(|e| format!("Error rendering data template: {e}"))?;
        let Value::Object(m) = rendered else {
            return Err("Error rendering data template: Result is not a Dictionary".into());
        };
        service_data.extend(m);
    }
    Ok(Prepared {
        domain,
        service,
        data: service_data,
        target,
    })
}

/// `cv.comp_entity_ids_or_uuids` on a rendered value: `all` or `none` as
/// they are; otherwise ids (a comma-separated string or a list), each
/// lower-cased and valid, as a list.
fn comp_entity_ids(v: &Value) -> Option<Value> {
    let one = |s: &str| {
        let s = s.trim().to_lowercase();
        ha_core::valid_entity_id(&s).then_some(Value::String(s))
    };
    match v {
        Value::String(s) => {
            let lower = s.to_lowercase();
            if lower == "all" || lower == "none" {
                return Some(Value::String(lower));
            }
            s.split(',')
                .map(one)
                .collect::<Option<Vec<_>>>()
                .map(Value::Array)
        }
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().and_then(one))
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        _ => None,
    }
}
