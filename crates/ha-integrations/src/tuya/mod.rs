//! The `tuya` integration (`components/tuya`, `tuya-device-sharing-sdk`),
//! for an imported Smart Life / Tuya entry: the cloud's device list, then
//! state pushed over Tuya's MQTT channel, and commands sent to the cloud.
//!
//! Covered: the dehumidifier (`cs`) and pet feeder (`cwwsq`) categories,
//! with HA's entity descriptions for them: humidifier, fan, switch, select,
//! number, sensor and binary sensor entities. Other categories are logged
//! and skipped.
//!
//! Where `hars` differs from HA: there is no config flow (the QR-code
//! login), so an entry only arrives by import; the fan has on/off only;
//! the SDK's version report and `unload` on removal are not sent.

pub mod api;
pub mod mqtt;

use crate::components::Components;
use crate::config_entries::{ConfigEntries, EntryState, Status};
use crate::light::BoxFuture;
use api::{Api, ApiError, Token};
use ha_core::{Attributes, EntityId, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use ha_registry::{DeviceInfo, EntityPatch, Registry};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const DOMAIN: &str = "tuya";

/// What running an entry loads, for `get_config`'s `components`.
pub const COMPONENTS: &[&str] = &[
    "tuya",
    "binary_sensor",
    "fan",
    "humidifier",
    "number",
    "select",
    "sensor",
    "switch",
];

/// A data point's type and range (`DeviceFunction` / `DeviceStatusRange`).
#[derive(Debug, Clone, Default)]
struct Spec {
    kind: String,
    values: Value,
}

impl Spec {
    fn int(&self, k: &str) -> Option<f64> {
        self.values.get(k).and_then(Value::as_f64)
    }
    fn scale(&self) -> i32 {
        self.int("scale").unwrap_or(0.0) as i32
    }
    fn unit(&self) -> Option<String> {
        self.values
            .get("unit")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
    fn range(&self) -> Vec<String> {
        self.values
            .get("range")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn labels(&self) -> Vec<String> {
        self.values
            .get("label")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
    /// A raw integer as the device means it.
    fn scaled(&self, raw: f64) -> f64 {
        raw / 10f64.powi(self.scale())
    }
    /// A value to send, back in the device's integer.
    fn unscaled(&self, v: f64) -> i64 {
        (v * 10f64.powi(self.scale())).round() as i64
    }
}

/// `CustomerDevice`, the parts used.
#[derive(Debug, Clone, Default)]
struct Dev {
    id: String,
    name: String,
    category: String,
    product_name: String,
    product_id: String,
    online: bool,
    status: Map<String, Value>,
    /// Writable data points first, then reported ones.
    function: HashMap<String, Spec>,
    status_range: HashMap<String, Spec>,
    support_local: bool,
    /// dpId → status code, for local-format pushes.
    local: HashMap<i64, String>,
}

impl Dev {
    fn has(&self, code: &str) -> bool {
        self.function.contains_key(code) || self.status_range.contains_key(code)
    }
    fn spec(&self, code: &str) -> Option<&Spec> {
        self.function
            .get(code)
            .or_else(|| self.status_range.get(code))
    }
    fn first<'a>(&self, codes: &[&'a str]) -> Option<&'a str> {
        codes.iter().copied().find(|c| self.has(c))
    }
}

/// What an entity reads and writes.
#[derive(Debug, Clone)]
enum Kind2 {
    Humidifier {
        switch: &'static str,
        current: Option<&'static str>,
        target: Option<&'static str>,
        mode: Option<&'static str>,
    },
    Fan {
        switch: &'static str,
    },
    Switch {
        code: &'static str,
    },
    Select {
        code: &'static str,
    },
    Number {
        code: &'static str,
    },
    Sensor {
        code: &'static str,
    },
    /// On when the code's value is `on` (a bool true when `None`).
    Binary {
        code: &'static str,
        on: Option<&'static str>,
    },
    /// On when bit `label` of a bitmap is set.
    Bitmap {
        code: &'static str,
        label: &'static str,
    },
}

/// An entity HA's description makes for a device.
struct Desc {
    domain: &'static str,
    /// The unique id's suffix after `tuya.<device id>`.
    key: &'static str,
    name: Option<&'static str>,
    translation_key: Option<&'static str>,
    device_class: Option<&'static str>,
    entity_category: Option<&'static str>,
    icon: Option<&'static str>,
    measurement: bool,
    /// A fixed unit, else the data point's.
    unit: Option<&'static str>,
    kind: Kind2,
}

fn desc(domain: &'static str, key: &'static str, kind: Kind2) -> Desc {
    Desc {
        domain,
        key,
        name: None,
        translation_key: None,
        device_class: None,
        entity_category: None,
        icon: None,
        measurement: false,
        unit: None,
        kind,
    }
}

impl Desc {
    fn named(mut self, tk: &'static str, name: &'static str) -> Self {
        self.translation_key = Some(tk);
        self.name = Some(name);
        self
    }
    fn class(mut self, c: &'static str) -> Self {
        self.device_class = Some(c);
        self
    }
    fn cat(mut self, c: &'static str) -> Self {
        self.entity_category = Some(c);
        self
    }
    fn icon(mut self, i: &'static str) -> Self {
        self.icon = Some(i);
        self
    }
    fn measured(mut self) -> Self {
        self.measurement = true;
        self
    }
    fn unit(mut self, u: &'static str) -> Self {
        self.unit = Some(u);
        self
    }
}

/// HA's descriptions for a device's category, kept only where the device
/// has the data points they need.
fn descriptions(d: &Dev) -> Vec<Desc> {
    let mut out = Vec::new();
    let mut add = |x: Desc, needs: &str| {
        if d.has(needs) {
            out.push(x);
        }
    };
    match d.category.as_str() {
        "cs" => {
            if let Some(switch) = d.first(&["switch", "switch_spray"]) {
                let hum = desc(
                    "humidifier",
                    "switch",
                    Kind2::Humidifier {
                        switch,
                        current: d.first(&["humidity_indoor"]),
                        target: d.first(&["dehumidify_set_value"]),
                        mode: d.first(&["mode"]),
                    },
                )
                .class("dehumidifier");
                out.push(hum);
            }
            if let Some(switch) = d.first(&["switch_fan", "fan_switch", "switch"]) {
                out.push(desc("fan", "", Kind2::Fan { switch }));
            }
            let mut add = |x: Desc, needs: &str| {
                if d.has(needs) {
                    out.push(x);
                }
            };
            add(
                desc(
                    "select",
                    "countdown_set",
                    Kind2::Select {
                        code: "countdown_set",
                    },
                )
                .named("countdown", "Countdown")
                .cat("config"),
                "countdown_set",
            );
            add(
                desc(
                    "select",
                    "dehumidify_set_enum",
                    Kind2::Select {
                        code: "dehumidify_set_enum",
                    },
                )
                .named("target_humidity", "Target humidity")
                .cat("config"),
                "dehumidify_set_enum",
            );
            for (code, tk, name, icon) in [
                ("anion", "ionizer", "Ionizer", "mdi:atom"),
                ("child_lock", "child_lock", "Child lock", "mdi:account-lock"),
                ("filter_reset", "filter_reset", "Filter reset", "mdi:filter"),
            ] {
                add(
                    desc("switch", code, Kind2::Switch { code })
                        .named(tk, name)
                        .icon(icon)
                        .cat("config"),
                    code,
                );
            }
            add(
                desc(
                    "sensor",
                    "temp_indoor",
                    Kind2::Sensor {
                        code: "temp_indoor",
                    },
                )
                .named("temperature", "Temperature")
                .class("temperature")
                .measured(),
                "temp_indoor",
            );
            add(
                desc(
                    "sensor",
                    "humidity_indoor",
                    Kind2::Sensor {
                        code: "humidity_indoor",
                    },
                )
                .named("humidity", "Humidity")
                .class("humidity")
                .measured(),
                "humidity_indoor",
            );
            let labels = d.spec("fault").map(Spec::labels).unwrap_or_default();
            for (key, label, tk, name) in [
                ("fault_water_full", "water_full", "tankfull", "Tank full"),
                ("tankfull", "tankfull", "tankfull", "Tank full"),
                ("fault_FULL", "FULL", "tankfull", "Tank full"),
                ("defrost", "defrost", "defrost", "Defrost"),
                ("fault_COIL", "COIL", "coil_freeze", "Coil freeze/defrost"),
                ("wet", "wet", "wet", "Wet"),
                (
                    "fault_Cleaning",
                    "Cleaning",
                    "filter_cleaning",
                    "Filter cleaning",
                ),
                ("fault_E1", "E1", "temp_error", "Temperature error"),
                ("fault_CL", "CL", "low_temp", "Low temperature"),
                ("fault_CH", "CH", "high_temp", "High temperature"),
                ("fault_LO", "LO", "low_humidity", "Low humidity"),
                ("fault_MOTOR", "MOTOR", "motor_fault", "Motor fault"),
            ] {
                if labels.iter().any(|l| l == label) {
                    out.push(
                        desc(
                            "binary_sensor",
                            key,
                            Kind2::Bitmap {
                                code: "fault",
                                label,
                            },
                        )
                        .named(tk, name)
                        .class("problem")
                        .cat("diagnostic"),
                    );
                }
            }
        }
        "cwwsq" => {
            add(
                desc(
                    "number",
                    "manual_feed",
                    Kind2::Number {
                        code: "manual_feed",
                    },
                )
                .named("feed", "Feed"),
                "manual_feed",
            );
            add(
                desc(
                    "number",
                    "voice_times",
                    Kind2::Number {
                        code: "voice_times",
                    },
                )
                .named("voice_times", "Voice times"),
                "voice_times",
            );
            add(
                desc("switch", "slow_feed", Kind2::Switch { code: "slow_feed" })
                    .named("slow_feed", "Slow feed")
                    .cat("config"),
                "slow_feed",
            );
            for code in [
                "battery_percentage",
                "battery",
                "battery_value",
                "va_battery",
            ] {
                let mut s = desc("sensor", code, Kind2::Sensor { code })
                    .named("battery", "Battery")
                    .class("battery")
                    .cat("diagnostic")
                    .measured();
                if matches!(code, "battery_percentage" | "battery") {
                    s = s.unit("%");
                }
                add(s, code);
            }
            add(
                desc(
                    "sensor",
                    "battery_state",
                    Kind2::Sensor {
                        code: "battery_state",
                    },
                )
                .named("battery_state", "Battery state")
                .cat("diagnostic"),
                "battery_state",
            );
            add(
                desc(
                    "sensor",
                    "feed_report",
                    Kind2::Sensor {
                        code: "feed_report",
                    },
                )
                .named("last_amount", "Last amount")
                .measured(),
                "feed_report",
            );
            add(
                desc(
                    "binary_sensor",
                    "feed_state",
                    Kind2::Binary {
                        code: "feed_state",
                        on: Some("feeding"),
                    },
                )
                .named("feeding", "Feeding"),
                "feed_state",
            );
            let mut charging = desc(
                "binary_sensor",
                "charge_state",
                Kind2::Binary {
                    code: "charge_state",
                    on: None,
                },
            )
            .class("battery_charging")
            .cat("diagnostic");
            charging.name = Some("Charging");
            add(charging, "charge_state");
        }
        other => tracing::info!(
            "Tuya device {} (category {other}) isn't supported by hars yet",
            d.name
        ),
    }
    out
}

/// What an entry's entities share.
pub struct Shared {
    hass: Hass,
    api: Arc<Api>,
    devices: Mutex<HashMap<String, Dev>>,
    alive: Mutex<bool>,
    /// The SDK's `Filter`: the same command to a device within 10 s is
    /// sent once.
    last_command: Mutex<HashMap<String, (Value, tokio::time::Instant)>>,
}

impl Shared {
    async fn send(&self, device: &str, commands: Value) -> Result<(), ServiceError> {
        {
            let mut last = self.last_command.lock();
            let now = tokio::time::Instant::now();
            if let Some((v, at)) = last.get(device)
                && *v == commands
                && now.duration_since(*at) < Duration::from_secs(10)
            {
                return Ok(());
            }
            last.insert(device.to_owned(), (commands.clone(), now));
        }
        self.api
            .post(
                &format!("/v1.1/m/thing/{device}/commands"),
                json!({"commands": commands}),
            )
            .await
            .map(|_| ())
            .map_err(|e| ServiceError::Failed(format!("Tuya command failed: {e}")))
    }
}

pub struct Ent {
    shared: Arc<Shared>,
    device: String,
    domain: &'static str,
    entity_id: EntityId,
    friendly: String,
    d: Desc,
}

fn num_state(v: f64) -> String {
    crate::num::py_format(v, 15)
}

impl Ent {
    fn spec(&self, dev: &Dev, code: &str) -> Spec {
        dev.spec(code).cloned().unwrap_or_default()
    }

    /// The state, the attributes kept while unavailable (capabilities),
    /// and those only while available.
    fn compute(&self, dev: &Dev) -> (String, Attributes, Attributes) {
        let mut cap = Attributes::new();
        let mut st = Attributes::new();
        let raw = |code: &str| dev.status.get(code);
        let int = |code: &str| -> Option<f64> {
            let spec = self.spec(dev, code);
            raw(code).and_then(Value::as_f64).map(|v| spec.scaled(v))
        };
        let bool_state = |b: Option<bool>| match b {
            Some(true) => "on".to_owned(),
            Some(false) => "off".to_owned(),
            None => "unknown".to_owned(),
        };
        let state = match &self.d.kind {
            Kind2::Humidifier {
                switch,
                current,
                target,
                mode,
            } => {
                if let Some(t) = target {
                    let s = self.spec(dev, t);
                    cap.insert(
                        "min_humidity".into(),
                        json!(s.scaled(s.int("min").unwrap_or(0.0)).round() as i64),
                    );
                    cap.insert(
                        "max_humidity".into(),
                        json!(s.scaled(s.int("max").unwrap_or(100.0)).round() as i64),
                    );
                }
                if let Some(m) = mode {
                    cap.insert("available_modes".into(), json!(self.spec(dev, m).range()));
                }
                if let Some(c) = current.and_then(&int) {
                    st.insert("current_humidity".into(), json!(c.round() as i64));
                }
                if let Some(t) = target.and_then(&int) {
                    st.insert("humidity".into(), json!(t.round() as i64));
                }
                if let Some(m) = mode {
                    st.insert("mode".into(), raw(m).cloned().unwrap_or(Value::Null));
                }
                bool_state(raw(switch).and_then(Value::as_bool))
            }
            Kind2::Fan { switch } => bool_state(raw(switch).and_then(Value::as_bool)),
            Kind2::Switch { code } => bool_state(raw(code).and_then(Value::as_bool)),
            Kind2::Binary { code, on } => {
                let v = raw(code);
                let b = match on {
                    Some(on) => v.and_then(Value::as_str).map(|s| s == *on),
                    None => v.and_then(Value::as_bool),
                };
                bool_state(b)
            }
            Kind2::Bitmap { code, label } => {
                let labels = self.spec(dev, code).labels();
                let bit = labels.iter().position(|l| l == label);
                let b = match (bit, raw(code).and_then(Value::as_i64)) {
                    (Some(bit), Some(v)) => Some((v >> bit) & 1 == 1),
                    _ => None,
                };
                bool_state(b)
            }
            Kind2::Select { code } => {
                cap.insert("options".into(), json!(self.spec(dev, code).range()));
                raw(code)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| "unknown".into())
            }
            Kind2::Number { code } => {
                let s = self.spec(dev, code);
                cap.insert("min".into(), json!(s.scaled(s.int("min").unwrap_or(0.0))));
                cap.insert("max".into(), json!(s.scaled(s.int("max").unwrap_or(100.0))));
                cap.insert("step".into(), json!(s.scaled(s.int("step").unwrap_or(1.0))));
                cap.insert("mode".into(), json!("auto"));
                int(code).map(num_state).unwrap_or_else(|| "unknown".into())
            }
            Kind2::Sensor { code } => {
                let s = self.spec(dev, code);
                if self.d.measurement {
                    cap.insert("state_class".into(), json!("measurement"));
                }
                match (s.kind.as_str(), raw(code)) {
                    ("Integer", Some(v)) => v.as_f64().map(|v| num_state(s.scaled(v))),
                    (_, Some(Value::String(v))) => Some(v.clone()),
                    (_, Some(Value::Number(n))) => n.as_f64().map(num_state),
                    _ => None,
                }
                .unwrap_or_else(|| "unknown".into())
            }
        };
        (state, cap, st)
    }

    fn write(&self) {
        let devices = self.shared.devices.lock();
        let Some(dev) = devices.get(&self.device) else {
            return;
        };
        let (state, mut attrs, st) = self.compute(dev);
        let available = dev.online;
        if available {
            attrs.extend(st);
        }
        // `_async_write_ha_state`'s order.
        if let Kind2::Number { code } | Kind2::Sensor { code } = &self.d.kind {
            let unit = self
                .d
                .unit
                .map(str::to_owned)
                .or_else(|| dev.spec(code).and_then(Spec::unit));
            if let Some(u) = unit {
                attrs.insert("unit_of_measurement".into(), json!(u));
            }
        }
        if let Some(c) = self.d.device_class {
            attrs.insert("device_class".into(), json!(c));
        }
        if let Some(i) = self.d.icon {
            attrs.insert("icon".into(), json!(i));
        }
        attrs.insert("friendly_name".into(), json!(self.friendly));
        match &self.d.kind {
            Kind2::Humidifier { mode, .. } => {
                attrs.insert(
                    "supported_features".into(),
                    json!(if mode.is_some() { 1 } else { 0 }),
                );
            }
            Kind2::Fan { .. } => {
                attrs.insert("supported_features".into(), json!(48));
            }
            _ => {}
        }
        drop(devices);
        if *self.shared.alive.lock() {
            let state = if available {
                state
            } else {
                "unavailable".into()
            };
            tracing::debug!(
                "tuya: {} = {state} {}",
                self.entity_id,
                serde_json::Value::Object(attrs.clone())
            );
            self.shared
                .hass
                .set_state(self.entity_id.clone(), state, attrs);
        }
    }

    fn available(&self) -> bool {
        self.shared
            .devices
            .lock()
            .get(&self.device)
            .is_some_and(|d| d.online)
    }

    fn is_on(&self) -> bool {
        let code = match &self.d.kind {
            Kind2::Humidifier { switch, .. } | Kind2::Fan { switch } => *switch,
            Kind2::Switch { code } => code,
            _ => return false,
        };
        self.shared
            .devices
            .lock()
            .get(&self.device)
            .and_then(|d| d.status.get(code))
            .and_then(Value::as_bool)
            == Some(true)
    }

    async fn turn(&self, on: bool) -> Result<(), ServiceError> {
        let code = match &self.d.kind {
            Kind2::Humidifier { switch, .. } | Kind2::Fan { switch } => *switch,
            Kind2::Switch { code } => code,
            _ => return Err(ServiceError::Failed("not switchable".into())),
        };
        self.shared
            .send(&self.device, json!([{"code": code, "value": on}]))
            .await
    }

    async fn set_int(&self, code: &str, v: f64) -> Result<(), ServiceError> {
        let spec = self
            .shared
            .devices
            .lock()
            .get(&self.device)
            .and_then(|d| d.spec(code).cloned())
            .unwrap_or_default();
        let (min, max) = (
            spec.scaled(spec.int("min").unwrap_or(f64::MIN)),
            spec.scaled(spec.int("max").unwrap_or(f64::MAX)),
        );
        if v < min || v > max {
            return Err(ServiceError::Failed(format!(
                "Value {v} for {} is outside {min}..{max}",
                self.entity_id
            )));
        }
        self.shared
            .send(
                &self.device,
                json!([{"code": code, "value": spec.unscaled(v)}]),
            )
            .await
    }

    async fn set_enum(&self, code: &str, v: &str) -> Result<(), ServiceError> {
        let options = self
            .shared
            .devices
            .lock()
            .get(&self.device)
            .and_then(|d| d.spec(code).map(Spec::range))
            .unwrap_or_default();
        if !options.iter().any(|o| o == v) {
            return Err(ServiceError::Failed(format!(
                "Option {v} is not valid for {}",
                self.entity_id
            )));
        }
        self.shared
            .send(&self.device, json!([{"code": code, "value": v}]))
            .await
    }
}

/// The entities services reach, by domain, from every entry.
#[derive(Default)]
struct Registered {
    items: RwLock<Vec<Arc<Ent>>>,
}

static ENTITIES: OnceLock<Arc<Registered>> = OnceLock::new();

/// A switch for the shared switch component.
struct TuyaSwitch(Arc<Ent>);

impl crate::switch::Switch for TuyaSwitch {
    fn entity_id(&self) -> &EntityId {
        &self.0.entity_id
    }
    fn available(&self) -> bool {
        self.0.available()
    }
    fn is_on(&self) -> bool {
        self.0.is_on()
    }
    fn turn(&self, on: bool) -> BoxFuture<'_> {
        Box::pin(self.0.turn(on))
    }
}

fn register_services(hass: &Hass, registries: ha_registry::Lookup) -> Arc<Registered> {
    ENTITIES
        .get_or_init(|| {
            let reg = Arc::new(Registered::default());
            let entity = Schema::entity_service;
            let services: Vec<(&'static str, &'static str, Schema)> = vec![
                ("humidifier", "turn_on", entity()),
                ("humidifier", "turn_off", entity()),
                ("humidifier", "toggle", entity()),
                (
                    "humidifier",
                    "set_humidity",
                    entity().required("humidity", Kind::Int(Range::new(0.0, 100.0))),
                ),
                (
                    "humidifier",
                    "set_mode",
                    entity().required("mode", Kind::String),
                ),
                (
                    "fan",
                    "turn_on",
                    entity()
                        .optional("percentage", Kind::Int(Range::new(0.0, 100.0)))
                        .optional("preset_mode", Kind::String),
                ),
                ("fan", "turn_off", entity()),
                ("fan", "toggle", entity()),
                (
                    "number",
                    "set_value",
                    entity().required("value", Kind::Float(Range::ANY)),
                ),
                (
                    "select",
                    "select_option",
                    entity().required("option", Kind::String),
                ),
                ("select", "select_first", entity()),
                ("select", "select_last", entity()),
                (
                    "select",
                    "select_next",
                    entity().optional("cycle", Kind::Boolean),
                ),
                (
                    "select",
                    "select_previous",
                    entity().optional("cycle", Kind::Boolean),
                ),
            ];
            for (domain, name, schema) in services {
                let reg = reg.clone();
                let registries = registries.clone();
                hass.services().register_with_schema(
                    domain,
                    name,
                    Some(schema),
                    SupportsResponse::None,
                    move |call| {
                        let reg = reg.clone();
                        let registries = registries.clone();
                        async move {
                            let target = call.entity_ids()?;
                            let indirect = registries.get().target_entities(&call.data);
                            let items: Vec<Arc<Ent>> = reg
                                .items
                                .read()
                                .iter()
                                .filter(|e| e.domain == domain)
                                .cloned()
                                .collect();
                            let chosen = crate::switch::select(
                                &items,
                                |e| &e.entity_id,
                                |e| e.available(),
                                &target,
                                &indirect,
                            );
                            let data = call.data.clone();
                            let calls = chosen.iter().map(|e| {
                                let data = data.clone();
                                async move { service(e, name, &data).await }
                            });
                            crate::light::join_all(calls)
                                .await
                                .into_iter()
                                .collect::<Result<Vec<()>, _>>()?;
                            Ok(None)
                        }
                    },
                );
            }
            reg
        })
        .clone()
}

async fn service(e: &Ent, name: &str, data: &Map<String, Value>) -> Result<(), ServiceError> {
    match (&e.d.kind, name) {
        (_, "turn_on") => e.turn(true).await,
        (_, "turn_off") => e.turn(false).await,
        (_, "toggle") => e.turn(!e.is_on()).await,
        (
            Kind2::Humidifier {
                target: Some(t), ..
            },
            "set_humidity",
        ) => {
            let h = data.get("humidity").and_then(Value::as_f64).unwrap_or(0.0);
            e.set_int(t, h).await
        }
        (Kind2::Humidifier { mode: Some(m), .. }, "set_mode") => {
            e.set_enum(m, data.get("mode").and_then(Value::as_str).unwrap_or(""))
                .await
        }
        (Kind2::Number { code }, "set_value") => {
            e.set_int(
                code,
                data.get("value").and_then(Value::as_f64).unwrap_or(0.0),
            )
            .await
        }
        (Kind2::Select { code }, _) => {
            let (options, current) = {
                let devs = e.shared.devices.lock();
                let d = devs.get(&e.device);
                (
                    d.and_then(|d| d.spec(code).map(Spec::range))
                        .unwrap_or_default(),
                    d.and_then(|d| d.status.get(*code))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )
            };
            let cycle = data.get("cycle").and_then(Value::as_bool).unwrap_or(true);
            let at = current.and_then(|c| options.iter().position(|o| *o == c));
            let n = options.len();
            let pick = match name {
                "select_option" => data
                    .get("option")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                "select_first" => options.first().cloned(),
                "select_last" => options.last().cloned(),
                "select_next" => match at {
                    Some(i) if i + 1 < n => options.get(i + 1).cloned(),
                    Some(_) if cycle => options.first().cloned(),
                    Some(i) => options.get(i).cloned(),
                    None => options.first().cloned(),
                },
                "select_previous" => match at {
                    Some(i) if i > 0 => options.get(i - 1).cloned(),
                    Some(_) if cycle => options.last().cloned(),
                    Some(i) => options.get(i).cloned(),
                    None => options.last().cloned(),
                },
                _ => None,
            };
            match pick {
                Some(o) => e.set_enum(code, &o).await,
                None => Ok(()),
            }
        }
        _ => Err(ServiceError::Unknown(format!(
            "{}.{name} is not supported by hars yet",
            e.domain
        ))),
    }
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn specs(list: Option<&Value>) -> HashMap<String, Spec> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let code = f.get("code")?.as_str()?.to_owned();
            let values = match f.get("values") {
                Some(Value::String(v)) => serde_json::from_str(v).unwrap_or(Value::Null),
                Some(v) => v.clone(),
                None => Value::Null,
            };
            Some((
                code,
                Spec {
                    kind: s(f, "type"),
                    values,
                },
            ))
        })
        .collect()
}

/// `Manager.update_device_cache`: every home's devices with their
/// specifications and push format. Also the home ids, for the owner topic.
async fn fetch(api: &Api) -> Result<(Vec<String>, Vec<Dev>), ApiError> {
    let homes = api.get("/v1.0/m/life/users/homes", None).await?;
    let home_ids: Vec<String> = homes["result"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| s(h, "ownerId"))
        .filter(|h| !h.is_empty())
        .collect();
    let mut devices = Vec::new();
    for home in &home_ids {
        let r = api
            .get(
                "/v1.0/m/life/ha/home/devices",
                Some(json!({"homeId": home})),
            )
            .await?;
        for item in r["result"].as_array().into_iter().flatten() {
            let mut d = Dev {
                id: s(item, "id"),
                name: s(item, "name"),
                category: s(item, "category"),
                product_name: s(item, "product_name"),
                product_id: s(item, "product_id"),
                online: item.get("online").and_then(Value::as_bool).unwrap_or(false),
                ..Default::default()
            };
            for st in item
                .get("status")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(c), Some(v)) =
                    (st.get("code").and_then(Value::as_str), st.get("value"))
                {
                    d.status.insert(c.to_owned(), v.clone());
                }
            }
            let spec = api
                .get(&format!("/v1.1/m/life/{}/specifications", d.id), None)
                .await?;
            d.function = specs(spec["result"].get("functions"));
            d.status_range = specs(spec["result"].get("status"));
            if let Ok(rel) = api
                .get(&format!("/v1.0/m/life/devices/{}/status", d.id), None)
                .await
            {
                let mut local = true;
                for x in rel["result"]["dpStatusRelationDTOS"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if x.get("supportLocal").and_then(Value::as_bool) != Some(true) {
                        local = false;
                        break;
                    }
                    if s(x, "valueConvert") != "default" {
                        tracing::debug!(
                            "tuya: {} dp {} uses conversion {}",
                            d.id,
                            s(x, "dpId"),
                            s(x, "valueConvert")
                        );
                    }
                    if let Some(id) = x.get("dpId").and_then(Value::as_i64) {
                        d.local.insert(id, s(x, "statusCode"));
                    }
                }
                if let Ok(custom) = api
                    .get(&format!("/v1.0/m/life/ha/{}/code/custom-type", d.id), None)
                    .await
                {
                    let on = match &custom["result"] {
                        Value::Bool(b) => *b,
                        Value::String(s) => s.eq_ignore_ascii_case("true"),
                        _ => false,
                    };
                    if on {
                        local = false;
                    }
                }
                d.support_local = local;
                if !local {
                    d.local.clear();
                }
            }
            devices.push(d);
        }
    }
    Ok((home_ids, devices))
}

/// Registry rows for the devices and entities, as HA's platforms make
/// them; rows an import brought are left as HA wrote them.
fn register(registry: &Registry, entry_id: &str, disable_new: bool, devices: &[Dev]) {
    if registry.read_only().is_some() {
        return;
    }
    for d in devices {
        let descs = descriptions(d);
        if descs.is_empty() {
            continue;
        }
        let device = registry.get_or_create_device(DeviceInfo {
            config_entry_id: entry_id.to_owned(),
            identifiers: vec![json!([DOMAIN, d.id])],
            manufacturer: Some("Tuya".into()),
            model: Some(d.product_name.clone()),
            model_id: Some(d.product_id.clone()),
            name: Some(d.name.clone()),
            ..Default::default()
        });
        for x in descs {
            let unique_id = format!("tuya.{}{}", d.id, x.key);
            let text = |s: Option<&str>| Some(s.map(str::to_owned));
            let unit = match &x.kind {
                Kind2::Number { code } | Kind2::Sensor { code } => x
                    .unit
                    .map(str::to_owned)
                    .or_else(|| d.spec(code).and_then(Spec::unit)),
                _ => None,
            };
            let patch = EntityPatch {
                config_entry_id: Some(Some(entry_id.to_owned())),
                device_id: Some(Some(device.id.clone())),
                entity_category: Some(x.entity_category.map(str::to_owned)),
                has_entity_name: Some(true),
                object_id_base: text(x.name),
                original_device_class: text(x.device_class),
                original_icon: text(x.icon),
                original_name: text(x.name),
                translation_key: text(x.translation_key),
                unit_of_measurement: Some(unit),
                pref_disable_new: disable_new,
                ..Default::default()
            };
            registry.get_or_create_entity(x.domain, DOMAIN, &unique_id, None, patch);
        }
    }
}

/// Apply one push message (`Manager.on_message`); the device it was for.
fn on_message(devices: &mut HashMap<String, Dev>, msg: &Value) -> Option<String> {
    let data = msg.get("data")?;
    match msg.get("protocol").and_then(Value::as_i64) {
        Some(4) => {
            let id = data.get("devId")?.as_str()?;
            let dev = devices.get_mut(id)?;
            for item in data.get("status")?.as_array()? {
                let Some(value) = item.get("value") else {
                    continue;
                };
                let code = if dev.support_local {
                    match item
                        .get("dpId")
                        .and_then(Value::as_i64)
                        .and_then(|i| dev.local.get(&i))
                    {
                        Some(c) => c.clone(),
                        None => continue,
                    }
                } else {
                    match item.get("code").and_then(Value::as_str) {
                        Some(c) => c.to_owned(),
                        None => continue,
                    }
                };
                dev.status.insert(code, value.clone());
            }
            Some(id.to_owned())
        }
        Some(20) => {
            let id = data.get("bizData")?.get("devId")?.as_str()?;
            let dev = devices.get_mut(id)?;
            match data.get("bizCode")?.as_str()? {
                "online" => dev.online = true,
                "offline" => dev.online = false,
                "nameUpdate" => {
                    if let Some(n) = data["bizData"].get("name").and_then(Value::as_str) {
                        dev.name = n.to_owned();
                    }
                }
                _ => return None,
            }
            Some(id.to_owned())
        }
        _ => None,
    }
}

/// Set up an entry and keep its devices' states current from the push
/// channel. `store` keeps the refreshed tokens.
pub async fn run(
    components: Arc<Components>,
    entry: Map<String, Value>,
    store: Option<Arc<ConfigEntries>>,
    ready: ha_core::SetupGuard,
    status: Status,
) {
    let mut ready = Some(ready);
    let hass = components.hass().clone();
    let data = entry
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let entry_id = entry
        .get("entry_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let endpoint = data
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let Some(token) = data
        .get("token_info")
        .map(Token::from_value)
        .filter(|t| !t.refresh_token.is_empty())
    else {
        tracing::error!("tuya: the entry has no token");
        status.set(EntryState::SetupError, None);
        return;
    };
    let client = match ha_http::Client::builder(crate::USER_AGENT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("tuya: {e}");
            status.set(EntryState::SetupError, None);
            return;
        }
    };
    let api = Api::new(client, endpoint, token);
    {
        let store = store.clone();
        let entry_id = entry_id.clone();
        let data = Mutex::new(data);
        api.on_token(move |t| {
            let mut d = data.lock();
            d.insert("token_info".into(), t.to_value());
            if let Some(store) = &store
                && !store.update_data(&entry_id, d.clone())
            {
                tracing::debug!("tuya: token unchanged in the entry");
            }
        });
    }
    let mut tries = 0u32;
    let (home_ids, devices) = loop {
        if tries > 0 {
            status.set(EntryState::SetupInProgress, None);
        }
        match fetch(&api).await {
            Ok(x) => break x,
            Err(ApiError::Api { code, msg }) if code == "1010" || msg.contains("token") => {
                tracing::error!(
                    "tuya: the login was refused ({code}: {msg}); add the integration again in Home Assistant and re-import"
                );
                status.set(EntryState::SetupError, None);
                return;
            }
            Err(e) => {
                let why = e.to_string();
                status.set(EntryState::SetupRetry, Some(why.clone()));
                if let Some(r) = ready.take() {
                    r.done();
                }
                let wait = (5u64 << tries.min(7)).min(600);
                tracing::info!(
                    "Config entry for tuya not ready yet: {why}; Retrying in {wait} seconds"
                );
                tries += 1;
                hass.setup_retry_wait(Duration::from_secs(wait)).await;
            }
        }
    };
    if let Some(registry) = components.registry() {
        let disable_new = entry
            .get("pref_disable_new_entities")
            .and_then(Value::as_bool)
            == Some(true);
        register(registry, &entry_id, disable_new, &devices);
    }
    let shared = Arc::new(Shared {
        hass: hass.clone(),
        api: api.clone(),
        devices: Mutex::new(devices.iter().map(|d| (d.id.clone(), d.clone())).collect()),
        alive: Mutex::new(true),
        last_command: Mutex::default(),
    });
    let registries = components.registries();
    let names = crate::naming::Names::new(&hass, &registries, DOMAIN);
    let reg = register_services(&hass, components.lookup());
    let mut ents = Vec::new();
    for d in &devices {
        for x in descriptions(d) {
            let unique_id = format!("tuya.{}{}", d.id, x.key);
            let Some((entity_id, friendly)) =
                names.entity(x.domain, &unique_id, &d.id, &d.name, x.name)
            else {
                continue;
            };
            ents.push(Arc::new(Ent {
                shared: shared.clone(),
                device: d.id.clone(),
                domain: x.domain,
                entity_id,
                friendly,
                d: x,
            }));
        }
    }
    for e in &ents {
        match e.domain {
            "switch" => components.switches().add(Arc::new(TuyaSwitch(e.clone()))),
            "humidifier" | "fan" | "number" | "select" => reg.items.write().push(e.clone()),
            _ => {}
        }
        e.write();
    }
    tracing::info!("tuya: {} devices, {} entities", devices.len(), ents.len());
    struct Stopped(Arc<Shared>, Arc<Registered>, Vec<EntityId>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            *self.0.alive.lock() = false;
            self.1
                .items
                .write()
                .retain(|e| !self.2.contains(&e.entity_id));
        }
    }
    let _stopped = Stopped(
        shared.clone(),
        reg.clone(),
        ents.iter().map(|e| e.entity_id.clone()).collect(),
    );
    status.set(EntryState::Loaded, None);
    drop(ready);
    if ents.is_empty() {
        return;
    }
    // `SharingMQ.run`: fetch credentials, connect, renew a minute before
    // they run out; back off on failures.
    let mut backoff = 1u64;
    loop {
        let link = format!(
            "tuya-device-sharing-sdk-python.{}",
            crate::switchbot_cloud::api::uuid4()
        );
        let cfg = match api
            .post("/v1.0/m/life/ha/access/config", json!({"linkId": link}))
            .await
        {
            Ok(c) => c["result"].clone(),
            Err(e) => {
                tracing::error!("tuya: mqtt config failed: {e}; retrying in {backoff} s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        let url = s(&cfg, "url");
        let expire = cfg
            .get("expireTime")
            .and_then(Value::as_u64)
            .unwrap_or(7200);
        let owner = s(&cfg["topic"]["ownerId"], "sub");
        let dev_topic = s(&cfg["topic"]["devId"], "sub");
        let mut topics: Vec<String> = home_ids
            .iter()
            .map(|h| owner.replace("{ownerId}", h))
            .collect();
        for d in &devices {
            let t = dev_topic.replace("{devId}", &d.id);
            topics.push(format!(
                "{t}{}",
                if d.support_local { "/pen" } else { "/sta" }
            ));
        }
        let renew = tokio::time::sleep(Duration::from_secs(expire.saturating_sub(60).max(60)));
        let started = tokio::time::Instant::now();
        let result = mqtt::run(
            &url,
            &s(&cfg, "clientId"),
            &s(&cfg, "username"),
            &s(&cfg, "password"),
            &topics,
            renew,
            |_topic, payload| {
                let Ok(msg) = serde_json::from_slice::<Value>(payload) else {
                    return;
                };
                let changed = on_message(&mut shared.devices.lock(), &msg);
                if let Some(id) = changed {
                    for e in ents.iter().filter(|e| e.device == id) {
                        e.write();
                    }
                }
            },
        )
        .await;
        match result {
            Ok(()) => backoff = 1,
            Err(e) => {
                tracing::warn!("tuya: mqtt link ended: {e}; reconnecting in {backoff} s");
                if started.elapsed() > Duration::from_secs(300) {
                    backoff = 1;
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dehumidifier() -> Dev {
        let spec = |kind: &str, v: Value| Spec {
            kind: kind.into(),
            values: v,
        };
        let mut d = Dev {
            id: "dev".into(),
            name: "Dry".into(),
            category: "cs".into(),
            online: true,
            support_local: true,
            ..Default::default()
        };
        d.function
            .insert("switch".into(), spec("Boolean", json!({})));
        d.function.insert(
            "dehumidify_set_value".into(),
            spec(
                "Integer",
                json!({"min":35,"max":70,"scale":0,"step":5,"unit":"%"}),
            ),
        );
        d.status_range.insert(
            "humidity_indoor".into(),
            spec(
                "Integer",
                json!({"min":0,"max":100,"scale":0,"step":1,"unit":"%"}),
            ),
        );
        d.status_range.insert(
            "fault".into(),
            spec(
                "Bitmap",
                json!({"label":["tankfull","defrost","E1","E2","L2","L3","L4","wet"]}),
            ),
        );
        d.status.insert("switch".into(), json!(true));
        d.status.insert("humidity_indoor".into(), json!(72));
        d.status.insert("fault".into(), json!(1));
        d.local.insert(16, "humidity_indoor".into());
        d
    }

    #[test]
    fn describes_a_dehumidifier() {
        let keys: Vec<(&str, &str)> = descriptions(&dehumidifier())
            .iter()
            .map(|x| (x.domain, x.key))
            .collect();
        assert_eq!(
            keys,
            [
                ("humidifier", "switch"),
                ("fan", ""),
                ("sensor", "humidity_indoor"),
                ("binary_sensor", "tankfull"),
                ("binary_sensor", "defrost"),
                ("binary_sensor", "wet"),
                ("binary_sensor", "fault_E1"),
            ]
        );
    }

    #[test]
    fn local_push_updates_by_dp_id() {
        let mut devices = HashMap::from([("dev".to_owned(), dehumidifier())]);
        let msg = json!({"protocol": 4, "data": {"devId": "dev", "status": [{"dpId": 16, "t": 1, "value": 60}]}});
        assert_eq!(on_message(&mut devices, &msg).as_deref(), Some("dev"));
        assert_eq!(devices["dev"].status["humidity_indoor"], json!(60));
        let off =
            json!({"protocol": 20, "data": {"bizCode": "offline", "bizData": {"devId": "dev"}}});
        on_message(&mut devices, &off);
        assert!(!devices["dev"].online);
    }
}
