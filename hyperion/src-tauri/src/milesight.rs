// Hyperion — Milesight LoRaWAN gateway config → IoT topology (M1, Requirement #4).
//
// Parses an uploaded Milesight UG-series gateway configuration export into a
// *normalized* IoT topology: the gateway itself, its LoRa network settings, and
// the attached end-devices/sensors. `parse_gateway` is intentionally PURE over a
// `serde_json::Value` — it touches no DB, file, or network — so it is fully
// unit-testable with synthetic JSON. The Tauri `milesight_import` command (in
// lib.rs) is the only place that reads the file off disk (size-capped like
// `context_add_file`), parses the JSON, and feeds it in.
//
// ## Assumed format — VALIDATE AGAINST A REAL MILESIGHT EXPORT
// There is no sample config in this repo, so this parser assumes the common
// Milesight UG65/UG67 **JSON** export shape and is deliberately *tolerant*: every
// field is optional, key matching is case-insensitive, common aliases are tried,
// and gateway/LoRa fields are resolved whether they are nested under a section or
// flat at the top level. Nothing here panics on a missing, extra, or wrongly-typed
// key — an unknown shape simply yields an empty/partial `Topology`. The assumed
// shape is roughly:
//
// ```json
// {
//   "gateway": { "name": "UG65-A", "model": "UG65",
//                "eui": "24E124FFFEF12345", "ip": "192.168.23.1" },
//   "lora":    { "region": "EU868", "frequency": 868.1 },
//   "devices": [
//     { "name": "Temp-01", "dev_eui": "24E124...",
//       "type": "AM103", "last_seen": "2026-06-27T10:00:00Z" }
//   ]
// }
// ```
//
// Real exports vary (fields nested under `general`/`lorawan`/`network_server`,
// devices under `device_list`/`nodes`, camelCase keys, frequency as a string),
// so the lookups below accept a spread of aliases. The big boss flags this
// milestone as "assumed format — validate against a real Milesight export."

use serde::Serialize;
use serde_json::Value;

/// The gateway node. Every field is optional — a sparse or unknown export still
/// produces a `GatewayInfo`, just with `None`s.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct GatewayInfo {
    pub name: Option<String>,
    pub model: Option<String>,
    pub eui: Option<String>,
    pub ip: Option<String>,
}

/// The gateway's LoRa network settings. `frequency` is the raw numeric value as
/// exported (MHz or Hz — not normalized, since that varies by export), tolerantly
/// parsed from either a JSON number or a numeric string.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct LoraSettings {
    pub region: Option<String>,
    pub frequency: Option<f64>,
}

/// One attached end-device / sensor. `device_type` serializes as `type` to match
/// the renderer's convention for node typing.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct DeviceNode {
    pub name: Option<String>,
    pub dev_eui: Option<String>,
    #[serde(rename = "type")]
    pub device_type: Option<String>,
    pub last_seen: Option<String>,
}

/// The normalized IoT topology, serializable as
/// `{ gateway: {...}, lora: {...}, devices: [...] }`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Topology {
    pub gateway: GatewayInfo,
    pub lora: LoraSettings,
    pub devices: Vec<DeviceNode>,
}

/// First present, non-empty **string** value among `keys` in a JSON object,
/// matched case-insensitively. Tolerant: a non-object, a missing key, or a
/// non-string value simply yields `None`. Keys are tried in priority order.
fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    let obj = v.as_object()?;
    for &key in keys {
        for (k, val) in obj {
            if k.eq_ignore_ascii_case(key) {
                if let Some(s) = val.as_str() {
                    let t = s.trim();
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
            }
        }
    }
    None
}

/// First present **numeric** value among `keys`, accepting either a JSON number or
/// a numeric string (e.g. `"868.1"`). Tolerant of missing/wrong-typed keys.
fn first_num(v: &Value, keys: &[&str]) -> Option<f64> {
    let obj = v.as_object()?;
    for &key in keys {
        for (k, val) in obj {
            if k.eq_ignore_ascii_case(key) {
                if let Some(n) = val.as_f64() {
                    return Some(n);
                }
                if let Some(n) = val.as_str().and_then(|s| s.trim().parse::<f64>().ok()) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// The first direct child object whose key matches one of `section_keys`
/// (case-insensitive), or the value itself when none match. Lets gateway/LoRa
/// fields be read whether they are nested under a section or flat at the top.
fn section<'a>(v: &'a Value, section_keys: &[&str]) -> &'a Value {
    if let Some(obj) = v.as_object() {
        for &key in section_keys {
            for (k, val) in obj {
                if k.eq_ignore_ascii_case(key) && val.is_object() {
                    return val;
                }
            }
        }
    }
    v
}

/// Pull a string field that may live inside a named section or flat at the root:
/// the section is searched first, then the root as a fallback.
fn pick_str(root: &Value, section_keys: &[&str], field_keys: &[&str]) -> Option<String> {
    let sec = section(root, section_keys);
    first_str(sec, field_keys).or_else(|| first_str(root, field_keys))
}

/// Numeric counterpart to `pick_str`.
fn pick_num(root: &Value, section_keys: &[&str], field_keys: &[&str]) -> Option<f64> {
    let sec = section(root, section_keys);
    first_num(sec, field_keys).or_else(|| first_num(root, field_keys))
}

/// Candidate keys under which a device/sensor list may be exported.
const DEVICE_LIST_KEYS: &[&str] = &[
    "devices",
    "device_list",
    "devicelist",
    "nodes",
    "sensors",
    "endpoints",
    "end_devices",
    "enddevices",
    "devs",
];

/// Locate the device/sensor array, searching the root object's direct keys first
/// and then descending into nested objects (bounded depth) so a list buried under
/// e.g. `lorawan.network_server.devices` is still found. Returns the device
/// `Value`s in order, or an empty vec when no list is present.
fn find_devices(v: &Value, depth: u8) -> Vec<&Value> {
    if let Some(obj) = v.as_object() {
        for &key in DEVICE_LIST_KEYS {
            for (k, val) in obj {
                if k.eq_ignore_ascii_case(key) {
                    if let Some(arr) = val.as_array() {
                        return arr.iter().collect();
                    }
                }
            }
        }
        if depth > 0 {
            for (_, val) in obj {
                let found = find_devices(val, depth - 1);
                if !found.is_empty() {
                    return found;
                }
            }
        }
    }
    Vec::new()
}

/// Normalize one device/sensor entry, pulling common fields tolerantly.
fn parse_device(v: &Value) -> DeviceNode {
    DeviceNode {
        name: first_str(v, &["name", "device_name", "dev_name", "label", "title"]),
        dev_eui: first_str(v, &["dev_eui", "deveui", "device_eui", "eui"]),
        device_type: first_str(v, &["type", "device_type", "dev_type", "profile", "model"]),
        last_seen: first_str(
            v,
            &[
                "last_seen",
                "lastseen",
                "last_uplink",
                "last_seen_at",
                "lastseenat",
                "last_active",
                "updated_at",
            ],
        ),
    }
}

/// Parse a Milesight gateway configuration `Value` into a normalized [`Topology`].
/// Pure and total: any JSON (object, array, scalar, null) is accepted and never
/// panics; unknown shapes degrade to an empty/partial topology.
pub fn parse_gateway(json: &Value) -> Topology {
    let gw_sections = &[
        "gateway", "gw", "general", "system", "device", "basic", "info",
    ];
    let gateway = GatewayInfo {
        name: pick_str(
            json,
            gw_sections,
            &["name", "gateway_name", "gw_name", "hostname", "device_name"],
        ),
        model: pick_str(
            json,
            gw_sections,
            &[
                "model",
                "device_model",
                "product",
                "product_model",
                "hardware",
                "hw_model",
            ],
        ),
        eui: pick_str(
            json,
            gw_sections,
            &["eui", "gateway_eui", "gw_eui", "gateway_id", "gw_id"],
        ),
        ip: pick_str(
            json,
            gw_sections,
            &[
                "ip",
                "ip_address",
                "ipaddress",
                "address",
                "lan_ip",
                "wan_ip",
            ],
        ),
    };

    let lora_sections = &[
        "lora",
        "lorawan",
        "radio",
        "network_server",
        "networkserver",
        "ns",
        "lora_network",
        "radios",
    ];
    let lora = LoraSettings {
        region: pick_str(
            json,
            lora_sections,
            &[
                "region",
                "band",
                "frequency_plan",
                "freq_plan",
                "frequency_band",
                "frequencyplan",
            ],
        ),
        frequency: pick_num(
            json,
            lora_sections,
            &[
                "frequency",
                "freq",
                "center_frequency",
                "center_freq",
                "centerfreq",
            ],
        ),
    };

    let devices = find_devices(json, 3)
        .into_iter()
        .map(parse_device)
        .collect();

    Topology {
        gateway,
        lora,
        devices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_config_parses_gateway_lora_and_devices() {
        let cfg = json!({
            "gateway": {
                "name": "UG65-Lobby",
                "model": "UG65",
                "eui": "24E124FFFEF12345",
                "ip": "192.168.23.1"
            },
            "lora": { "region": "EU868", "frequency": 868.1 },
            "devices": [
                { "name": "Temp-01", "dev_eui": "24E124AABB001122",
                  "type": "AM103", "last_seen": "2026-06-27T10:00:00Z" },
                { "name": "Door-02", "dev_eui": "24E124AABB003344",
                  "type": "WS301", "last_seen": "2026-06-27T09:55:00Z" }
            ]
        });
        let t = parse_gateway(&cfg);
        assert_eq!(t.gateway.name.as_deref(), Some("UG65-Lobby"));
        assert_eq!(t.gateway.model.as_deref(), Some("UG65"));
        assert_eq!(t.gateway.eui.as_deref(), Some("24E124FFFEF12345"));
        assert_eq!(t.gateway.ip.as_deref(), Some("192.168.23.1"));
        assert_eq!(t.lora.region.as_deref(), Some("EU868"));
        assert_eq!(t.lora.frequency, Some(868.1));
        assert_eq!(t.devices.len(), 2);
        assert_eq!(t.devices[0].name.as_deref(), Some("Temp-01"));
        assert_eq!(t.devices[0].device_type.as_deref(), Some("AM103"));
        assert_eq!(t.devices[1].dev_eui.as_deref(), Some("24E124AABB003344"));
    }

    #[test]
    fn tolerates_flat_layout_aliases_and_nested_device_list() {
        // Fields flat at the top level, camelCase / alias keys, frequency as a
        // string, and the device list nested under network_server.
        let cfg = json!({
            "gateway_name": "UG67",
            "device_model": "UG67-868M",
            "gw_eui": "24E124FFFEFABCDE",
            "ip_address": "10.0.0.5",
            "lorawan": {
                "band": "US915",
                "freq": "902.3",
                "network_server": {
                    "device_list": [
                        { "label": "CO2-Room4", "deveui": "24E124DEAD0001",
                          "profile": "AM319", "lastSeen": "2026-06-26T12:00:00Z" }
                    ]
                }
            }
        });
        let t = parse_gateway(&cfg);
        assert_eq!(t.gateway.name.as_deref(), Some("UG67"));
        assert_eq!(t.gateway.model.as_deref(), Some("UG67-868M"));
        assert_eq!(t.gateway.eui.as_deref(), Some("24E124FFFEFABCDE"));
        assert_eq!(t.gateway.ip.as_deref(), Some("10.0.0.5"));
        assert_eq!(t.lora.region.as_deref(), Some("US915"));
        assert_eq!(t.lora.frequency, Some(902.3));
        assert_eq!(t.devices.len(), 1);
        assert_eq!(t.devices[0].name.as_deref(), Some("CO2-Room4"));
        assert_eq!(t.devices[0].dev_eui.as_deref(), Some("24E124DEAD0001"));
        assert_eq!(t.devices[0].device_type.as_deref(), Some("AM319"));
        assert_eq!(
            t.devices[0].last_seen.as_deref(),
            Some("2026-06-26T12:00:00Z")
        );
    }

    #[test]
    fn sparse_config_yields_mostly_defaults() {
        // Only a gateway name; everything else absent → None / empty, no panic.
        let cfg = json!({ "gateway": { "name": "OnlyName" } });
        let t = parse_gateway(&cfg);
        assert_eq!(t.gateway.name.as_deref(), Some("OnlyName"));
        assert_eq!(t.gateway.model, None);
        assert_eq!(t.gateway.eui, None);
        assert_eq!(t.gateway.ip, None);
        assert_eq!(t.lora, LoraSettings::default());
        assert!(t.devices.is_empty());
    }

    #[test]
    fn garbage_and_empty_values_never_panic() {
        // A spread of degenerate inputs must all return an empty topology.
        for v in [
            json!(null),
            json!({}),
            json!([]),
            json!(42),
            json!("not a config"),
            json!(true),
            json!([1, 2, 3]),
            json!({ "gateway": "should-be-an-object", "lora": 7, "devices": "nope" }),
            json!({ "devices": [null, 3, "x", { "name": "Survivor" }] }),
        ] {
            let t = parse_gateway(&v);
            // The only case with a recoverable device is the last one.
            if v.get("devices").and_then(|d| d.as_array()).is_some() {
                assert_eq!(t.devices.len(), 4);
                assert_eq!(t.devices[3].name.as_deref(), Some("Survivor"));
                // Non-object entries normalize to all-None devices.
                assert_eq!(t.devices[0], DeviceNode::default());
            }
        }
        // Empty-string fields are treated as absent (trimmed → None).
        let t = parse_gateway(&json!({ "gateway": { "name": "   ", "ip": "" } }));
        assert_eq!(t.gateway.name, None);
        assert_eq!(t.gateway.ip, None);
    }

    #[test]
    fn serializes_to_documented_shape() {
        let cfg = json!({
            "gateway": { "name": "G", "model": "UG65" },
            "lora": { "region": "AS923", "frequency": 923.2 },
            "devices": [{ "name": "S", "dev_eui": "EUI", "type": "AM103" }]
        });
        let v = serde_json::to_value(parse_gateway(&cfg)).unwrap();
        assert_eq!(v["gateway"]["name"], "G");
        assert_eq!(v["gateway"]["model"], "UG65");
        assert_eq!(v["lora"]["region"], "AS923");
        assert_eq!(v["lora"]["frequency"], json!(923.2));
        // device_type serializes under `type`, and last_seen is null when absent.
        assert_eq!(v["devices"][0]["type"], "AM103");
        assert_eq!(v["devices"][0]["last_seen"], Value::Null);
    }
}
