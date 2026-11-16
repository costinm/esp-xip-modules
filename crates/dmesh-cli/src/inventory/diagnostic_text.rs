//! Schema projection for durable, host-side structured text records.
use super::*;
use mesh::message::TextRecord;

fn host_api() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../../api/generated/host.api.json"))
        .expect("generated host schema")
}
fn project(
    value: &serde_json::Value,
    fields: &[serde_json::Value],
    api: &serde_json::Value,
    gaps: &mut Vec<String>,
    path: &str,
) -> serde_json::Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut out = serde_json::Map::new();
    for (tag, value) in object {
        let field = fields.iter().find(|f| {
            f["name"] == tag.as_str()
                || f["tag"].as_u64().map(|n| n.to_string()).as_deref() == Some(tag)
        });
        let Some(field) = field else {
            gaps.push(format!("{path}.@{tag}"));
            out.insert(format!("@{tag}"), value.clone());
            continue;
        };
        let name = field["name"].as_str().unwrap();
        let ty = field["type"].as_str().unwrap_or("");
        let types = api["types"].as_array();
        let nested = types
            .and_then(|t| t.iter().find(|t| t["name"] == ty.trim_end_matches("[]")))
            .and_then(|t| t["fields"].as_array());
        let value = if let Some(nested) = nested {
            if let Some(array) = value.as_array() {
                serde_json::Value::Array(
                    array
                        .iter()
                        .map(|v| project(v, nested, api, gaps, &format!("{path}.{name}")))
                        .collect(),
                )
            } else {
                project(value, nested, api, gaps, &format!("{path}.{name}"))
            }
        } else {
            value.clone()
        };
        out.insert(name.into(), value);
    }
    serde_json::Value::Object(out)
}
fn named_record(method: &str, value: serde_json::Value) -> Result<String, String> {
    Ok(TextRecord::from_json_object(method, &value).format() + "\n")
}
fn components(value: &serde_json::Value, gaps: &mut Vec<String>) -> serde_json::Value {
    let Some(records) = value["5"].as_object() else {
        return value["5"].clone();
    };
    let modules = value["4"].as_array();
    let mut output = serde_json::Map::new();
    for (tag, methods) in records {
        let name = modules
            .and_then(|a| {
                a.iter()
                    .find(|m| m["2"].as_u64().map(|n| n.to_string()).as_deref() == Some(tag))
            })
            .and_then(|m| m["1"].as_str());
        let catalog = name
            .and_then(|n| mesh::catalog::service_catalog_resolver().resolve(&format!("xip-{n}")))
            .and_then(Result::ok);
        let mut result = serde_json::Map::new();
        if let Some(methods) = methods.as_object() {
            for (method, snapshot) in methods {
                let tool = catalog
                    .as_ref()
                    .and_then(|c| c.tools.as_array().or_else(|| c.tools["tools"].as_array()))
                    .and_then(|a| {
                        a.iter().find(|t| {
                            t["x-method-index"]
                                .as_u64()
                                .map(|n| n.to_string())
                                .as_deref()
                                == Some(method)
                        })
                    });
                let (method_name, fields) = if let Some(tool) = tool {
                    let fields=tool["outputSchema"]["properties"].as_object().map(|p|p.iter().map(|(name,s)|serde_json::json!({"name":name,"tag":s["x-protobuf-index"],"type":s["x-protobuf-type"]})).collect::<Vec<_>>()).unwrap_or_default();
                    (tool["name"].as_str().unwrap_or(method).to_owned(), fields)
                } else {
                    gaps.push(format!("component @{tag} method @{method}"));
                    (format!("@{method}"), Vec::new())
                };
                let status = project(
                    &snapshot["2"],
                    &fields,
                    &serde_json::json!({}),
                    gaps,
                    &method_name,
                );
                result.insert(
                    method_name,
                    serde_json::json!({"updated_ms":snapshot["1"],"status":status}),
                );
            }
        }
        output.insert(
            name.map(str::to_owned).unwrap_or_else(|| format!("@{tag}")),
            result.into(),
        );
    }
    output.into()
}
fn telemetry(
    bytes: &[u8],
    images: &[ImageStatus],
    gaps: &mut Vec<String>,
) -> Option<(String, serde_json::Value)> {
    let record = mesh::cbor::decode_record(bytes).ok()?;
    for image in images.iter().filter(|i| i.kind == "module") {
        let Some(Ok(catalog)) =
            mesh::catalog::service_catalog_resolver().resolve(&format!("xip-{}", image.name))
        else {
            continue;
        };
        let Some(name) = catalog.catalog.method_name(&record) else {
            continue;
        };
        let tools = catalog
            .tools
            .as_array()
            .or_else(|| catalog.tools["tools"].as_array())?;
        let tool = tools.iter().find(|t| t["name"] == name)?;
        let shape = if record.result.is_some() {
            "outputSchema"
        } else {
            "inputSchema"
        };
        let fields=tool[shape]["properties"].as_object().map(|p|p.iter().map(|(name,s)|serde_json::json!({"name":name,"tag":s["x-protobuf-index"],"type":s["x-protobuf-type"]})).collect::<Vec<_>>()).unwrap_or_default();
        let mut value = if let Some(result) = record.result {
            serde_json::json!({"result":project(&result,&fields,&serde_json::json!({}),gaps,name)})
        } else {
            let env = record
                .env
                .iter()
                .map(|(key, value)| {
                    (
                        match key {
                            mesh::tagged::NameOrTag::Tag(n) => n.to_string(),
                            mesh::tagged::NameOrTag::Name(s) => s.clone(),
                        },
                        value.clone(),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            project(&env.into(), &fields, &serde_json::json!({}), gaps, name)
        };
        if let Some(id) = record.id {
            value["request_id"] = id;
        }
        if let Some(error) = record.error {
            value["error"] = error;
        }
        if let Some(data) = record.data {
            value["data"] = format!("hex:{}", hex(&data)).into();
        }
        return Some((name.to_owned(), value));
    }
    None
}
pub(super) fn render(
    source: &str,
    body: &[u8],
    observed_at: u64,
    images: &[ImageStatus],
) -> Result<(String, Vec<String>), String> {
    if source == "partial-panic" {
        return Ok((
            named_record(
                "diagnostic.partial_panic",
                serde_json::json!({"observed_at":observed_at,"bytes":format!("hex:{}",hex(body))}),
            )?,
            vec!["partial panic capture".into()],
        ));
    }
    let api = host_api();
    let mut gaps = Vec::new();
    let mut text = String::new();
    // A telemetry batch is a sequence of complete CBOR values.
    let mut decoder = minicbor::Decoder::new(body);
    while decoder.position() < body.len() {
        let start = decoder.position();
        decoder.skip().map_err(|e| e.to_string())?;
        let bytes = &body[start..decoder.position()];
        let mut value = crate::console::decode(bytes)?;
        let method: String;
        if source == "stats" {
            method = "host.device_info".into();
            let fields = api["methods"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == method)
                .unwrap()["response"]["fields"]
                .as_array()
                .unwrap();
            let cached = components(&value, &mut gaps);
            value = project(&value, fields, &api, &mut gaps, "host.device_info");
            if !cached.is_null() {
                value["components"] = cached;
            }
        } else if source=="power" {
            method="host.power_info".into();
            let fields=api["types"].as_array().unwrap().iter().find(|t|t["name"]=="PowerInfo").ok_or("missing power schema")?["fields"].as_array().unwrap();
            value=project(&value,fields,&api,&mut gaps,"host.power_info");
        } else if super::diagnostics::is_panic_record(bytes) {
            method = "host.panic".into();
            let fields = api["types"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == "Panic")
                .ok_or("missing Panic schema")?["fields"]
                .as_array()
                .unwrap();
            value.as_object_mut().unwrap().remove("0");
            value = project(&value, fields, &api, &mut gaps, "host.panic");
            value["kind"] = 3.into();
            if let Some(hex) = value["message"]
                .as_str()
                .and_then(|s| s.strip_prefix("hex:"))
            {
                if let Ok(bytes) = unhex(hex) {
                    if let Ok(message) = String::from_utf8(bytes) {
                        value["message"] = message.into();
                    }
                }
            }
        } else if let Some((name, projected)) = telemetry(bytes, images, &mut gaps) {
            method = name;
            value = projected;
        } else {
            // Preserve arbitrary telemetry losslessly; its missing schema is explicit.
            method = "diagnostic.record".into();
            gaps.push("unknown telemetry schema".into());
            value = serde_json::json!({"record":value});
        }
        value
            .as_object_mut()
            .ok_or("diagnostic is not a map")?
            .insert("observed_at".into(), observed_at.into());
        text.push_str(&named_record(&method, value)?);
    }
    gaps.sort();
    gaps.dedup();
    Ok((text, gaps))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stats_project_nested_fields_and_preserve_unknown_tags() {
        let bytes = [
            0xa2, 0x03, 0xa2, 0x01, 0x18, 0x20, 0x18, 0x63, 0x07, 0x04, 0x81, 0xa2, 0x01, 0x63,
            b'n', b'a', b'n', 0x0f, 0x18, 0x50,
        ];
        let (text, gaps) = render("stats", &bytes, 123, &[]).unwrap();
        assert!(text.contains("host.device_info"));
        assert!(text.contains("heap="));
        assert!(text.contains("free"));
        assert!(text.contains("heap_used"));
        assert_eq!(gaps, vec!["host.device_info.heap.@99"]);
        assert!(text.ends_with("\n"));
        assert!(TextRecord::parse(text.trim()).is_ok());
        assert!(text.contains("@99"));
    }
}

#[cfg(test)]
mod pressure_tests {
    use super::*;
    #[test]
    fn pressure_panic_uses_schema_names() {
        let mut e = minicbor::Encoder::new(Vec::new());
        e.map(19)
            .unwrap()
            .u8(0)
            .unwrap()
            .u8(3)
            .unwrap()
            .u8(1)
            .unwrap()
            .str("panic")
            .unwrap()
            .u8(2)
            .unwrap()
            .str("nan")
            .unwrap()
            .u8(3)
            .unwrap()
            .bytes(b"allocation failed\nat \"test\"")
            .unwrap();
        for tag in 4..=18 {
            e.u8(tag).unwrap().u32(42).unwrap();
        }
        let (text, gaps) = render("rtc-panic", &e.into_writer(), 123, &[]).unwrap();
        assert!(gaps.is_empty(), "{gaps:?}");
        assert!(text.contains("pool_cached=42"));
        assert!(text.contains("reclaimed_bytes=42"));
        assert!(text.contains("host.panic"));
        let parsed = TextRecord::parse(text.trim()).unwrap();
        assert_eq!(parsed.fields["message"], "allocation failed\nat \"test\"");
        assert_eq!(text.lines().count(), 1);
    }
}
