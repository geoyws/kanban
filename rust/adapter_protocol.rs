use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}
