//! Human-oriented, flat timestamped view of the complete CBOR telemetry.
use super::*;
use mesh::message::TextRecord;
use serde_json::{Map, Value};
fn value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| text.into())
}
fn flat(prefix: &str, v: &Value, out: &mut Map<String, Value>) {
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                flat(
                    &if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    },
                    v,
                    out,
                )
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                flat(&format!("{prefix}.{i}"), v, out)
            }
        }
        _ => {
            out.insert(prefix.into(), v.clone());
        }
    }
}
fn emit(out: &mut String, time: &str, dev: &str, kind: &str, fields: &Map<String, Value>) {
    let mut record = TextRecord::new(kind);
    for (k, v) in fields {
        let text = match v {
            Value::String(s) => s.clone(),
            _ => v.to_string(),
        };
        if text.starts_with("hex:") && text.len() > 132 {
            record
                .fields
                .insert(format!("{k}_bytes"), ((text.len() - 4) / 2).to_string());
            record
                .fields
                .insert(format!("{k}_prefix"), format!("{}…", &text[..68]));
        } else {
            record.fields.insert(k.clone(), text);
        }
    }
    let formatted = record.format();
    let fields = formatted.strip_prefix(kind).unwrap();
    out.push_str(&format!("{time} {kind} dev={dev}{fields}\n"));
}
fn group(out: &mut String, time: &str, dev: &str, kind: &str, v: &Value) {
    let mut fields = Map::new();
    flat("", v, &mut fields);
    if !fields.is_empty() {
        emit(out, time, dev, kind, &fields);
    }
}
fn array(out: &mut String, time: &str, dev: &str, kind: &str, v: &Value) {
    if let Some(items) = v.as_array() {
        for item in items {
            let mut fields = Map::new();
            if let Some(o) = item.as_object() {
                for (k, v) in o {
                    if !v.is_array() && !v.is_object() {
                        fields.insert(k.clone(), v.clone());
                    }
                }
            }
            emit(out, time, dev, kind, &fields);
            if let Some(shadows) = item.get("shadowed").and_then(Value::as_array) {
                for shadow in shadows {
                    let mut fields = Map::new();
                    flat("", shadow, &mut fields);
                    fields.insert("name".into(), item["name"].clone());
                    emit(out, time, dev, "module.shadow", &fields);
                }
            }
        }
    }
}
fn emit_power(out: &mut String, time: &str, dev: &str, power: &Value) {
    if let Some(p) = power.as_object() {
        let (mut sleep, mut policy, mut cpu, mut radio, mut heap) =
            (Map::new(), Map::new(), Map::new(), Map::new(), Map::new());
        for (k, v) in p {
            let target = match k.as_str() {
                "pm_configured" | "auto_light_sleep" | "tickless_idle" | "active_window"
                | "ip_sockets" | "usb_sleep_blocked" | "usb_sleep_allowed"
                | "awake_remaining_ms" => &mut policy,
                "cpu_mhz" | "min_mhz" | "max_mhz" | "tick_hz" | "ticks" | "tasks" => &mut cpu,
                "promiscuous" | "filter_dropped" | "promisc_frames" => &mut radio,
                "heap_free" | "heap_min" | "heap_largest" => &mut heap,
                _ => &mut sleep,
            };
            target.insert(k.clone(), v.clone());
        }
        for (kind, fields) in [
            ("sleep", sleep),
            ("sleep.policy", policy),
            ("cpu", cpu),
            ("radio", radio),
            ("sleep.heap", heap),
        ] {
            if !fields.is_empty() {
                emit(out, time, dev, kind, &fields);
            }
        }
    } else {
        group(
            out,
            time,
            dev,
            "sleep",
            &serde_json::json!({"available":false}),
        );
    }
}
pub(super) fn render(
    record: &TextRecord,
    time_ns: u64,
    dev: &str,
    priority: &str,
    images: &[ImageStatus],
    gaps: &[String],
) -> Result<String, String> {
    let mut date = super::telemetry::dated(time_ns / 1_000_000_000)?;
    date.replace_range(13..14, ":");
    date.replace_range(16..17, ":");
    date.truncate(19);
    let time = format!("{date}.{:03}Z", time_ns / 1_000_000 % 1000);
    let mut out = String::new();
    if record.kind == "host.device_info" {
        for (name, kind) in [
            ("device", "device"),
            ("heap", "heap"),
            ("packets", "packets"),
        ] {
            if let Some(v) = record.fields.get(name) {
                group(&mut out, &time, dev, kind, &value(v));
            }
        }
        let power = record
            .fields
            .get("power")
            .map(|s| value(s))
            .unwrap_or(Value::Null);
        emit_power(&mut out, &time, dev, &power);
        if let Some(v) = record.fields.get("modules") {
            array(&mut out, &time, dev, "module", &value(v));
        }
        for image in images {
            group(
                &mut out,
                &time,
                dev,
                "image",
                &serde_json::to_value(image).map_err(|e| e.to_string())?,
            );
        }
        if let Some(v) = record.fields.get("components").map(|s| value(s)) {
            if let Some(components) = v.as_object() {
                for (name, methods) in components {
                    if let Some(methods) = methods.as_object() {
                        for (method, snapshot) in methods {
                            let mut fields = Map::new();
                            flat("", &snapshot["status"], &mut fields);
                            fields.insert("name".into(), name.clone().into());
                            fields.insert("method".into(), method.clone().into());
                            fields.insert("updated_ms".into(), snapshot["updated_ms"].clone());
                            emit(&mut out, &time, dev, "component", &fields);
                        }
                    }
                }
            }
        }
    } else if record.kind == "host.power_info" {
        let power = Value::Object(
            record
                .fields
                .iter()
                .filter(|(k, _)| k.as_str() != "observed_at")
                .map(|(k, v)| (k.clone(), value(v)))
                .collect(),
        );
        emit_power(&mut out, &time, dev, &power);
    } else {
        let mut fields = Map::new();
        for (k, v) in &record.fields {
            if k == "observed_at" {
                continue;
            }
            if k == "message" {
                fields.insert(k.clone(), v.clone().into());
            } else {
                flat(k, &value(v), &mut fields);
            }
        }
        if priority == "high" {
            fields.insert("priority".into(), "high".into());
        }
        emit(
            &mut out,
            &time,
            dev,
            if priority == "high" {
                "panic"
            } else {
                &record.kind
            },
            &fields,
        );
    }
    for gap in gaps {
        group(
            &mut out,
            &time,
            dev,
            "schema_gap",
            &serde_json::json!({"field":gap}),
        );
    }
    if out.is_empty() {
        group(
            &mut out,
            &time,
            dev,
            "snapshot",
            &serde_json::json!({"source":record.kind}),
        );
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_flat_fields_and_utc_timestamps() {
        let record = TextRecord::from_json_object(
            "host.device_info",
            &serde_json::json!({"heap":{"free":1234},"power":{"sleep_entries":7,"cpu_mhz":80,"usb_sleep_blocked":false},"modules":[{"name":"nan","heap_used":80,"running":true}]}),
        );
        let text = render(
            &record,
            1_000_000_000,
            "0123456789abcdef",
            "normal",
            &[],
            &[],
        )
        .unwrap();
        assert!(text.contains("1970-01-01T00:00:01.000Z heap dev=0123456789abcdef free=1234"));
        assert!(text.contains(" sleep dev="));
        assert!(text.contains(" module dev="));
        assert!(!text.contains('{'));
        assert!(!text.contains('['));
        assert!(text.contains("sleep_entries=7"));
        for line in text.lines() {
            let (time, rest) = line.split_once(' ').unwrap();
            assert!(time.ends_with('Z'));
            assert!(TextRecord::parse(rest).is_ok());
        }
    }
    #[test]
    fn panic_message_remains_quoted_on_one_line() {
        let record = TextRecord::from_json_object(
            "host.panic",
            &serde_json::json!({"module":"nan","message":"failed\nat foo"}),
        );
        let text = render(&record, 0, "0123456789abcdef", "high", &[], &[]).unwrap();
        assert_eq!(text.lines().count(), 1);
        let parsed = TextRecord::parse(text.split_once(' ').unwrap().1.trim()).unwrap();
        assert_eq!(parsed.fields["message"], "failed\nat foo");
    }
}
