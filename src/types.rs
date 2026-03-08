use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Nostr core types
// ---------------------------------------------------------------------------

/// A Nostr event as defined by NIP-01.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// A NIP-01 subscription filter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Catch-all for `#e`, `#p`, etc. tag filters.
    #[serde(flatten)]
    pub tags: HashMap<String, Vec<String>>,
}

// ---------------------------------------------------------------------------
// Outbound messages (client → relay)
// ---------------------------------------------------------------------------

/// All messages the client can send to a relay.
#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// `["REQ", <id>, <filter>, ...]`
    Req { id: String, filters: Vec<Filter> },
    /// `["CLOSE", <id>]`
    Close { id: String },
    /// `["EVENT", <event>]`
    Event(NostrEvent),
    /// `["AUTH", <event>]` — NIP-42
    Auth(NostrEvent),
    /// `["COUNT", <id>, <filter>, ...]` — NIP-45
    Count { id: String, filters: Vec<Filter> },
}

impl ClientMessage {
    pub fn to_json(&self) -> String {
        match self {
            ClientMessage::Req { id, filters } => serialize_id_filters("REQ", id, filters),
            ClientMessage::Close { id } => serde_json::to_string(&["CLOSE", id]).unwrap(),
            ClientMessage::Event(ev) => {
                serde_json::to_string(&("EVENT", serde_json::to_value(ev).unwrap())).unwrap()
            }
            ClientMessage::Auth(ev) => {
                serde_json::to_string(&("AUTH", serde_json::to_value(ev).unwrap())).unwrap()
            }
            ClientMessage::Count { id, filters } => serialize_id_filters("COUNT", id, filters),
        }
    }
}

fn serialize_id_filters(verb: &str, id: &str, filters: &[Filter]) -> String {
    let mut parts: Vec<serde_json::Value> = vec![
        serde_json::Value::String(verb.into()),
        serde_json::Value::String(id.into()),
    ];
    for f in filters {
        parts.push(serde_json::to_value(f).unwrap());
    }
    serde_json::to_string(&parts).unwrap()
}

// ---------------------------------------------------------------------------
// Inbound messages (relay → client)
// ---------------------------------------------------------------------------

/// Raw parsed relay message, before any subscription filtering.
#[derive(Debug, Clone)]
pub enum RelayMessage {
    /// `["EVENT", <sub-id>, <event>]`
    Event { sub_id: String, event: NostrEvent },
    /// `["EOSE", <sub-id>]`
    Eose { sub_id: String },
    /// `["CLOSED", <sub-id>, <reason>]`
    Closed { sub_id: String, reason: String },
    /// `["OK", <event-id>, <ok>, <message>]`
    Ok {
        event_id: String,
        ok: bool,
        message: String,
    },
    /// `["AUTH", <challenge>]`
    Auth { challenge: String },
    /// `["NOTICE", <message>]`
    Notice { message: String },
    /// `["COUNT", <sub-id>, {"count": N}]`
    Count { sub_id: String, count: u64 },
}

impl RelayMessage {
    /// Parse a raw JSON frame received from the relay.
    pub fn from_json(text: &str) -> Result<Self, ParseError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ParseError::Json(e.to_string()))?;
        let arr = value.as_array().ok_or(ParseError::NotArray)?;
        let kind = arr
            .first()
            .and_then(|v| v.as_str())
            .ok_or(ParseError::MissingKind)?;

        match kind {
            "EVENT" => {
                let sub_id = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or(ParseError::MissingField("sub_id"))?
                    .to_string();
                let event: NostrEvent = serde_json::from_value(
                    arr.get(2)
                        .cloned()
                        .ok_or(ParseError::MissingField("event"))?,
                )
                .map_err(|e| ParseError::Json(e.to_string()))?;
                Ok(RelayMessage::Event { sub_id, event })
            }
            "EOSE" => {
                let sub_id = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or(ParseError::MissingField("sub_id"))?
                    .to_string();
                Ok(RelayMessage::Eose { sub_id })
            }
            "CLOSED" => {
                let sub_id = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or(ParseError::MissingField("sub_id"))?
                    .to_string();
                let reason = arr
                    .get(2)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(RelayMessage::Closed { sub_id, reason })
            }
            "OK" => {
                let event_id = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or(ParseError::MissingField("event_id"))?
                    .to_string();
                let ok = arr
                    .get(2)
                    .and_then(|v| v.as_bool())
                    .ok_or(ParseError::MissingField("ok"))?;
                let message = arr
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(RelayMessage::Ok {
                    event_id,
                    ok,
                    message,
                })
            }
            "AUTH" => {
                let challenge = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or(ParseError::MissingField("challenge"))?
                    .to_string();
                Ok(RelayMessage::Auth { challenge })
            }
            "NOTICE" => {
                let message = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(RelayMessage::Notice { message })
            }
            "COUNT" => {
                let sub_id = arr
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or(ParseError::MissingField("sub_id"))?
                    .to_string();
                let count_obj = arr.get(2).ok_or(ParseError::MissingField("count_obj"))?;
                let count = count_obj
                    .get("count")
                    .and_then(|v| v.as_u64())
                    .ok_or(ParseError::MissingField("count"))?;
                Ok(RelayMessage::Count { sub_id, count })
            }
            other => Err(ParseError::UnknownKind(other.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-subscription message types (what req() emits)
// ---------------------------------------------------------------------------

/// Messages emitted by `Relay::req()` for a specific subscription.
#[derive(Debug, Clone)]
pub enum ReqMessage {
    /// Subscription is open and active.
    Open {
        from: String,
        id: String,
        filters: Vec<Filter>,
    },
    /// An event matching the subscription filters.
    Event {
        from: String,
        id: String,
        event: NostrEvent,
    },
    /// End of stored events — live subscription follows.
    Eose { from: String, id: String },
    /// Relay closed the subscription.
    Closed {
        from: String,
        id: String,
        reason: String,
    },
}

/// Response from publishing an event.
#[derive(Debug, Clone)]
pub struct PublishResponse {
    pub from: String,
    pub event_id: String,
    pub ok: bool,
    pub message: String,
}

/// NIP-45 COUNT response.
#[derive(Debug, Clone)]
pub struct CountResponse {
    pub from: String,
    pub sub_id: String,
    pub count: u64,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error)]
pub enum ParseError {
    #[error("JSON parse error: {0}")]
    Json(String),
    #[error("message is not a JSON array")]
    NotArray,
    #[error("message array is missing the kind string")]
    MissingKind,
    #[error("missing field: {0}")]
    MissingField(&'static str),
    #[error("unknown relay message kind: {0}")]
    UnknownKind(String),
}

#[derive(Debug, Clone, Error)]
pub enum RelayError {
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("Relay closed subscription {id} with reason: {reason}")]
    RelayClosed { id: String, reason: String },
    #[error("Auth required")]
    AuthRequired,
    #[error("URL parse error: {0}")]
    Url(String),
    #[error("Operation timed out")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: Filter defaults are all None / empty.
    #[test]
    fn filter_default() {
        let f = Filter::default();
        assert!(f.ids.is_none());
        assert!(f.authors.is_none());
        assert!(f.kinds.is_none());
        assert!(f.since.is_none());
        assert!(f.until.is_none());
        assert!(f.limit.is_none());
    }

    /// Smoke test: ClientMessage serialization produces valid NIP-01 JSON.
    #[test]
    fn client_message_req_serialization() {
        let msg = ClientMessage::Req {
            id: "test123".into(),
            filters: vec![Filter {
                kinds: Some(vec![1]),
                limit: Some(5),
                ..Default::default()
            }],
        };
        let json = msg.to_json();
        assert!(json.starts_with(r#"["REQ","test123","#));
    }

    #[test]
    fn relay_message_parse_event() {
        let json = r#"["EVENT","sub1",{"id":"abc","pubkey":"def","created_at":1000,"kind":1,"tags":[],"content":"hello","sig":"xyz"}]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Event { sub_id, event } => {
                assert_eq!(sub_id, "sub1");
                assert_eq!(event.content, "hello");
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn relay_message_parse_eose() {
        let json = r#"["EOSE","sub1"]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Eose { sub_id } => assert_eq!(sub_id, "sub1"),
            _ => panic!("expected Eose"),
        }
    }

    #[test]
    fn relay_message_parse_ok() {
        let json = r#"["OK","eventid123",true,""]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Ok {
                event_id,
                ok,
                message,
            } => {
                assert_eq!(event_id, "eventid123");
                assert!(ok);
                assert_eq!(message, "");
            }
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn relay_message_parse_notice() {
        let json = r#"["NOTICE","hello world"]"#;
        let msg = RelayMessage::from_json(json).unwrap();
        match msg {
            RelayMessage::Notice { message } => assert_eq!(message, "hello world"),
            _ => panic!("expected Notice"),
        }
    }
}
