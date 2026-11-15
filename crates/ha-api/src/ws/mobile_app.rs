//! `mobile_app/push_notification_channel` and `_confirm`
//! (`mobile_app/websocket_api.py`): the companion app's local push channel.
//! The channel itself (sending, the confirm handshake, falling back to
//! cloud push) is `ha_integrations::mobile_app::notify`'s; here are the
//! commands' checks and the WebSocket messages.

use super::{Conn, Invalid, Subscription};
use ha_core::ServiceError;
use ha_integrations::mobile_app::Webhook;
use ha_integrations::mobile_app::notify::Notifier;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    let res = match ty {
        "mobile_app/push_notification_channel" => channel(conn, id, msg),
        "mobile_app/push_notification_confirm" => confirm(conn, id, msg),
        _ => return None,
    };
    Some(res)
}

/// The schema: `str` fields (strict), `bool` ones, nothing else.
fn schema(msg: &Map<String, Value>, strs: &[&str], bools: &[&str]) -> Result<(), Invalid> {
    let mut errors = Vec::new();
    // voluptuous's path: `data[<repr(key)>]`.
    let at = |k: &str| {
        let mut r = String::new();
        super::py_repr_str(&mut r, k);
        format!("@ data[{r}]")
    };
    for (k, v) in msg {
        let k = k.as_str();
        if matches!(k, "id" | "type") {
            continue;
        }
        if strs.contains(&k) {
            if !v.is_string() {
                errors.push(format!("expected str for dictionary value {}", at(k)));
            }
        } else if bools.contains(&k) {
            if !v.is_boolean() {
                errors.push(format!("expected bool for dictionary value {}", at(k)));
            }
        } else {
            errors.push(format!("extra keys not allowed {}", at(k)));
        }
    }
    for k in strs {
        if !msg.contains_key(*k) {
            errors.push(format!("required key not provided {}", at(k)));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// `_ensure_webhook_access`: the registration exists and belongs to the
/// connection's user. `false` once the error is sent.
fn access(conn: &Conn, id: u64, webhook_id: &str) -> bool {
    let hook = match &conn.shared.config.mobile_app {
        Some(app) => app.webhook(webhook_id),
        None => Webhook::Unknown,
    };
    let Webhook::Registered(reg) = hook else {
        conn.out
            .error(json!(id), "not_found", "Webhook ID not found");
        return false;
    };
    if reg.user_id().as_deref() != Some(conn.user.id.as_str()) {
        conn.out.error(
            json!(id),
            "unauthorized",
            "User not linked to this webhook ID",
        );
        return false;
    }
    true
}

/// The app's notifier, once its registrations are set up.
fn notifier(conn: &Conn) -> Option<Arc<Notifier>> {
    conn.shared.config.mobile_app.as_ref()?.notifier().cloned()
}

/// `handle_push_notification_channel`: the channel replaces any the app
/// had (what that one had waiting goes by cloud first); its messages are
/// events of this subscription.
fn channel(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    schema(msg, &["webhook_id"], &["support_confirm"])?;
    let webhook_id = msg["webhook_id"].as_str().unwrap_or_default();
    if !access(conn, id, webhook_id) {
        return Ok(());
    }
    let Some(notifier) = notifier(conn) else {
        conn.out
            .error(json!(id), "not_found", "Webhook ID not found");
        return Ok(());
    };
    let support_confirm = msg
        .get("support_confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let out = conn.out.clone();
    // `event_message(msg["id"], data)`.
    let send = Box::new(move |event: &str| {
        out.send(format!(r#"{{"id":{id},"type":"event","event":{event}}}"#));
    });
    let (channel, opening) = notifier.open_channel(webhook_id, support_confirm, send);
    conn.subs.insert(id, Subscription::Push(channel));
    match opening {
        None => {
            conn.out.result(id, Value::Null);
        }
        Some(opening) => {
            let out = conn.out.clone();
            tokio::spawn(async move {
                match opening.await {
                    Ok(()) => out.result(id, Value::Null),
                    Err(e) => out.error_with(json!(id), error(&e)),
                };
            });
        }
    }
    Ok(())
}

/// `async_handle_exception` for what replacing a channel can raise: a
/// `HomeAssistantError` with its translation, anything else unknown.
fn error(e: &ServiceError) -> Value {
    match e {
        ServiceError::Translated(r) => json!({
            "code": "home_assistant_error",
            "message": r.message,
            "translation_key": r.key,
            "translation_placeholders": r.placeholders,
            "translation_domain": r.domain,
        }),
        ServiceError::Failed(m) => json!({"code": "home_assistant_error", "message": m}),
        _ => json!({"code": "unknown_error", "message": "Unknown error"}),
    }
}

/// `handle_push_notification_confirm`.
fn confirm(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    schema(msg, &["webhook_id", "confirm_id"], &[])?;
    let webhook_id = msg["webhook_id"].as_str().unwrap_or_default();
    if !access(conn, id, webhook_id) {
        return Ok(());
    }
    let confirm_id = msg["confirm_id"].as_str().unwrap_or_default();
    if notifier(conn).is_some_and(|n| n.confirm(webhook_id, confirm_id)) {
        conn.out.result(id, Value::Null);
    } else {
        conn.out.error(
            json!(id),
            "not_found",
            "Push notification channel not found",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
