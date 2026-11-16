//! Rendering an automation's templates as HA's scripts do
//! (`template.render_complex`, `ScriptVariables.async_render`,
//! `service.async_prepare_call_from_config`). These run on a render thread
//! ([`ha_core::template::run`]); what they log comes back in [`Out`].

use crate::cond::Out;
use crate::config::{Complex, Service, Variable};
use ha_core::template::{Home, TemplateError, Var, Vars};
use ha_registry::Registries;
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
    /// When the target names an entity by its registry id: its `entity_id`
    /// as HA's error shows it, to resolve ([`resolve_ids`]) just before the
    /// call.
    pub shown: Option<String>,
}

impl Prepared {
    /// A call with nothing to render.
    pub(crate) fn fixed(
        domain: &str,
        service: &str,
        data: Map<String, Value>,
        target: Option<Map<String, Value>>,
    ) -> Prepared {
        let shown = target.as_ref().and_then(registry_ids_shown);
        Prepared {
            domain: domain.to_owned(),
            service: service.to_owned(),
            data,
            target,
            shown,
        }
    }
}

/// `async_prepare_call_from_config`: the service name, the target, then
/// the data, rendered; the error is HA's `HomeAssistantError` text. Entity
/// registry ids in the target are checked for shape here and resolved
/// against the registry as it is when the call is made ([`resolve_ids`]).
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
    let (target, shown) = match target {
        None => (None, None),
        Some(Complex::Static(Value::Object(t))) => (Some(t.clone()), registry_ids_shown(t)),
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
            let mut shown = None;
            if let Some(ids) = t.get("entity_id") {
                let raw = Var::json(ids).to_py_str();
                let ids = comp_entity_ids(ids)
                    .ok_or_else(|| format!("Template rendered invalid entity IDs: {raw}"))?;
                t.insert("entity_id".into(), ids);
                shown = has_registry_ids(&t).then_some(raw);
            }
            (Some(t), shown)
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
        shown,
    })
}

/// `cv.fake_uuid4_hex`: an entity registry id (32 lower-case hex digits).
fn is_registry_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a target names an entity by its registry id.
fn has_registry_ids(target: &Map<String, Value>) -> bool {
    matches!(target.get("entity_id"), Some(Value::Array(ids))
        if ids.iter().any(|i| i.as_str().is_some_and(is_registry_id)))
}

/// A static target's `entity_id` as HA's error shows it, if it names a
/// registry id (only those read the registry).
fn registry_ids_shown(target: &Map<String, Value>) -> Option<String> {
    has_registry_ids(target).then(|| Var::json(&target["entity_id"]).to_py_str())
}

/// `entity_registry.async_validate_entity_ids`, against the registry as
/// it is now: each registry id in the target becomes the entity id it has;
/// an unknown one is HA's error.
pub(crate) fn resolve_ids(p: &mut Prepared, registry: Option<&Registries>) -> Result<(), String> {
    let Some(shown) = &p.shown else {
        return Ok(());
    };
    let Some(Value::Array(ids)) = p.target.as_mut().and_then(|t| t.get_mut("entity_id")) else {
        return Ok(());
    };
    for id in ids.iter_mut() {
        if let Some(s) = id.as_str().filter(|s| is_registry_id(s)) {
            let entity = registry
                .and_then(|r| r.entities.iter().find(|e| e.id.as_deref() == Some(s)))
                .map(|e| e.entity_id.clone())
                .ok_or_else(|| format!("Template rendered invalid entity IDs: {shown}"))?;
            *id = Value::String(entity);
        }
    }
    Ok(())
}

/// `cv.comp_entity_ids_or_uuids` on a rendered value: `all` or `none` as
/// they are; otherwise ids (a comma-separated string or a list), each a
/// valid entity id (lower-cased) or a registry id (resolved later, by
/// [`resolve_ids`]), as a list.
fn comp_entity_ids(v: &Value) -> Option<Value> {
    let one = |s: &str| {
        let s = s.trim();
        let lower = s.to_lowercase();
        if ha_core::valid_entity_id(&lower) {
            return Some(Value::String(lower));
        }
        is_registry_id(s).then(|| Value::String(s.to_owned()))
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
