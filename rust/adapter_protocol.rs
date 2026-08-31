use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_MESSAGE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AdapterRequest {
    pub(crate) protocol_version: i64,
    pub(crate) delivery: AdapterDelivery,
    pub(crate) target: AdapterTarget,
    pub(crate) event: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AdapterDelivery {
    #[serde(rename = "subscriptionID")]
    pub(crate) subscription_id: String,
    #[serde(rename = "eventID")]
    pub(crate) event_id: String,
    pub(crate) attempt: i64,
    pub(crate) created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AdapterTarget {
    #[serde(rename = "consumerID")]
    pub(crate) consumer_id: String,
    #[serde(rename = "actionID")]
    pub(crate) action_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AdapterResponse {
    pub(crate) protocol_version: i64,
    #[serde(rename = "subscriptionID")]
    pub(crate) subscription_id: String,
    #[serde(rename = "eventID")]
    pub(crate) event_id: String,
    pub(crate) created_at: i64,
    pub(crate) replay: bool,
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn contains_private_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| crate::watch::secret_key(key) || contains_private_key(value)),
        Value::Array(values) => values.iter().any(contains_private_key),
        _ => false,
    }
}

pub(crate) fn encode_request(request: &AdapterRequest) -> Result<Vec<u8>> {
    if request.protocol_version != 1 {
        bail!(
            "unsupported adapter request protocol version {}",
            request.protocol_version
        );
    }
    if request.delivery.attempt < 1 {
        bail!("adapter delivery attempt must be at least 1");
    }
    if !is_lower_hex_64(&request.delivery.event_id) {
        bail!("adapter delivery event ID must be lowercase 64-hex");
    }
    let event = request
        .event
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("adapter event must be an object"))?;
    if event.get("eventID").and_then(Value::as_str) != Some(request.delivery.event_id.as_str()) {
        bail!("adapter event ID does not match delivery");
    }
    if event.get("eventHash").and_then(Value::as_str) != Some(request.delivery.event_id.as_str()) {
        bail!("adapter event hash does not match delivery");
    }
    if event.get("timestamp").and_then(Value::as_i64) != Some(request.delivery.created_at) {
        bail!("adapter event timestamp does not match delivery");
    }
    if contains_private_key(&request.event) {
        bail!("adapter event contains a private key");
    }
    let encoded = serde_json::to_vec(request)?;
    if encoded.len() > MAX_MESSAGE_BYTES {
        bail!("adapter request exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    Ok(encoded)
}

pub(crate) fn decode_response(bytes: &[u8], request: &AdapterRequest) -> Result<AdapterResponse> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        bail!("adapter response exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let response = AdapterResponse::deserialize(&mut deserializer)?;
    deserializer.end()?;
    if response.protocol_version != 1 {
        bail!(
            "unsupported adapter response protocol version {}",
            response.protocol_version
        );
    }
    if response.subscription_id != request.delivery.subscription_id {
        bail!("adapter response subscription identity does not match request");
    }
    if response.event_id != request.delivery.event_id {
        bail!("adapter response event identity does not match request");
    }
    if response.created_at != request.delivery.created_at {
        bail!("adapter response timestamp does not match request");
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> AdapterRequest {
        AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-test".into(),
                event_id: "a".repeat(64),
                attempt: 2,
                created_at: 123,
            },
            target: AdapterTarget {
                consumer_id: "consumer.test".into(),
                action_id: "enqueue".into(),
            },
            event: json!({
                "eventID": "a".repeat(64),
                "eventHash": "a".repeat(64),
                "timestamp": 123
            }),
        }
    }

    fn response_value() -> Value {
        json!({
            "protocolVersion": 1,
            "subscriptionID": "sub-test",
            "eventID": "a".repeat(64),
            "createdAt": 123,
            "replay": false
        })
    }

    #[test]
    fn request_uses_the_exact_protocol_v1_shape() {
        let value = serde_json::to_value(request()).unwrap();
        assert_eq!(
            value,
            json!({
                "protocolVersion": 1,
                "delivery": {
                    "subscriptionID": "sub-test",
                    "eventID": "a".repeat(64),
                    "attempt": 2,
                    "createdAt": 123
                },
                "target": {
                    "consumerID": "consumer.test",
                    "actionID": "enqueue"
                },
                "event": {
                    "eventID": "a".repeat(64),
                    "eventHash": "a".repeat(64),
                    "timestamp": 123
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<AdapterRequest>(value).unwrap(),
            request()
        );
    }

    #[test]
    fn response_uses_the_exact_protocol_v1_shape() {
        let response = AdapterResponse {
            protocol_version: 1,
            subscription_id: "sub-test".into(),
            event_id: "b".repeat(64),
            created_at: 456,
            replay: false,
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(
            value,
            json!({
                "protocolVersion": 1,
                "subscriptionID": "sub-test",
                "eventID": "b".repeat(64),
                "createdAt": 456,
                "replay": false
            })
        );
        assert_eq!(
            serde_json::from_value::<AdapterResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn unknown_request_fields_fail_at_every_structured_level() {
        let mut top = serde_json::to_value(request()).unwrap();
        top["unknown"] = json!(true);
        assert!(serde_json::from_value::<AdapterRequest>(top).is_err());

        let mut delivery = serde_json::to_value(request()).unwrap();
        delivery["delivery"]["unknown"] = json!(true);
        assert!(serde_json::from_value::<AdapterRequest>(delivery).is_err());

        let mut target = serde_json::to_value(request()).unwrap();
        target["target"]["unknown"] = json!(true);
        assert!(serde_json::from_value::<AdapterRequest>(target).is_err());
    }

    #[test]
    fn unknown_response_fields_fail_closed() {
        let value = json!({
            "protocolVersion": 1,
            "subscriptionID": "sub-test",
            "eventID": "c".repeat(64),
            "createdAt": 789,
            "replay": true,
            "unknown": true
        });
        assert!(serde_json::from_value::<AdapterResponse>(value).is_err());
    }

    #[test]
    fn strict_response_decoder_accepts_one_matching_value_and_whitespace() {
        let mut bytes = serde_json::to_vec(&response_value()).unwrap();
        bytes.extend_from_slice(b" \n\t");
        assert!(!decode_response(&bytes, &request()).unwrap().replay);
    }

    #[test]
    fn strict_response_decoder_enforces_the_inclusive_size_limit() {
        let mut at_limit = serde_json::to_vec(&response_value()).unwrap();
        at_limit.resize(MAX_MESSAGE_BYTES, b' ');
        assert!(decode_response(&at_limit, &request()).is_ok());

        at_limit.push(b' ');
        assert!(decode_response(&at_limit, &request()).is_err());
    }

    #[test]
    fn strict_response_decoder_rejects_extra_json_or_non_whitespace() {
        let encoded = serde_json::to_string(&response_value()).unwrap();
        assert!(decode_response(format!("{encoded} {{}}").as_bytes(), &request()).is_err());
        assert!(decode_response(format!("{encoded} junk").as_bytes(), &request()).is_err());
    }

    #[test]
    fn strict_response_decoder_rejects_unknown_fields() {
        let mut value = response_value();
        value["unknown"] = json!(true);
        assert!(decode_response(&serde_json::to_vec(&value).unwrap(), &request()).is_err());
    }

    #[test]
    fn strict_response_decoder_rejects_version_identity_and_timestamp_mismatches() {
        for (field, value) in [
            ("protocolVersion", json!(2)),
            ("subscriptionID", json!("sub-other")),
            ("eventID", json!("b".repeat(64))),
            ("createdAt", json!(124)),
        ] {
            let mut response = response_value();
            response[field] = value;
            assert!(
                decode_response(&serde_json::to_vec(&response).unwrap(), &request()).is_err(),
                "field {field} must fail closed"
            );
        }
    }

    #[test]
    fn request_encoder_accepts_a_matching_public_event() {
        let fixture = request();
        let encoded = encode_request(&fixture).unwrap();
        assert_eq!(
            serde_json::from_slice::<AdapterRequest>(&encoded).unwrap(),
            fixture
        );
    }

    #[test]
    fn request_encoder_rejects_version_attempt_and_event_hash_shape() {
        let mut unsupported = request();
        unsupported.protocol_version = 2;
        assert!(encode_request(&unsupported).is_err());

        let mut zero_attempt = request();
        zero_attempt.delivery.attempt = 0;
        assert!(encode_request(&zero_attempt).is_err());

        for event_id in ["a".repeat(63), "g".repeat(64), "A".repeat(64)] {
            let mut malformed = request();
            malformed.delivery.event_id = event_id;
            assert!(encode_request(&malformed).is_err());
        }
    }

    #[test]
    fn request_encoder_rejects_missing_or_mismatched_event_identity() {
        for field in ["eventID", "eventHash"] {
            let mut missing = request();
            missing.event.as_object_mut().unwrap().remove(field);
            assert!(encode_request(&missing).is_err(), "missing {field}");

            let mut mismatched = request();
            mismatched.event[field] = json!("b".repeat(64));
            assert!(encode_request(&mismatched).is_err(), "mismatched {field}");
        }
    }

    #[test]
    fn request_encoder_rejects_missing_mismatched_or_noninteger_timestamp() {
        let mut missing = request();
        missing.event.as_object_mut().unwrap().remove("timestamp");
        assert!(encode_request(&missing).is_err());

        for value in [json!(124), json!(123.0), json!("123")] {
            let mut malformed = request();
            malformed.event["timestamp"] = value;
            assert!(encode_request(&malformed).is_err());
        }
    }

    #[test]
    fn request_encoder_rejects_nonobject_events_and_recursive_private_keys() {
        let mut nonobject = request();
        nonobject.event = json!([]);
        assert!(encode_request(&nonobject).is_err());

        for payload in [
            json!({"authToken": "private"}),
            json!({"nested": {"credential": "private"}}),
            json!({"nested": [{"materialValue": "private"}]}),
        ] {
            let mut private = request();
            private.event["payload"] = payload;
            assert!(encode_request(&private).is_err());
        }
    }

    #[test]
    fn request_encoder_rejects_messages_over_one_mibibyte() {
        let mut oversized = request();
        oversized.event["metadata"] = json!("x".repeat(MAX_MESSAGE_BYTES));
        assert!(encode_request(&oversized).is_err());
    }
}
