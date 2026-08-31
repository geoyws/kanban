use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_RESPONSE_BYTES: usize = 1 << 20;

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

pub(crate) fn decode_response(bytes: &[u8], request: &AdapterRequest) -> Result<AdapterResponse> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        bail!("adapter response exceeds {MAX_RESPONSE_BYTES} bytes");
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
            event: json!({"eventID": "a".repeat(64), "timestamp": 123}),
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
        at_limit.resize(MAX_RESPONSE_BYTES, b' ');
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
}
