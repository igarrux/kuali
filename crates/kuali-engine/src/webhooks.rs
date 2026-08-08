//! Delivers completed meetings to subscribed external applications.

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use hmac::{Hmac, Mac};
use kuali_core::{Meeting, WebhookSubscription};
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

const EVENT_COMPLETED: &str = "meeting.completed";
const EVENT_TEST: &str = "webhook.test";
const MAX_ATTEMPTS: u8 = 3;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryStatus {
    Ready,
    Disabled,
    Failed,
    Empty,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("connection failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("endpoint returned HTTP {status}: {message}")]
    Status { status: StatusCode, message: String },
}

impl WebhookError {
    fn retryable(&self) -> bool {
        match self {
            Self::Http(_) => true,
            Self::Status { status, .. } => {
                status.is_server_error()
                    || *status == StatusCode::REQUEST_TIMEOUT
                    || *status == StatusCode::TOO_MANY_REQUESTS
            }
            Self::Invalid(_) => false,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    schema_version: u8,
    event: &'static str,
    delivery_id: String,
    sent_at: String,
    data: Value,
}

struct Delivery {
    id: String,
    timestamp: String,
    body: Vec<u8>,
}

pub fn validate(subscription: &WebhookSubscription) -> Result<Url, WebhookError> {
    subscription
        .validate_fields()
        .map_err(WebhookError::Invalid)?;
    let url = Url::parse(subscription.url.trim())
        .map_err(|error| WebhookError::Invalid(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebhookError::Invalid(
            "URL must start with http:// or https://".into(),
        ));
    }
    Ok(url)
}

fn client() -> Result<Client, WebhookError> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("Kuali/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn delivery(event: &'static str, data: Value) -> Result<Delivery, WebhookError> {
    let now = Utc::now();
    let id = Uuid::new_v4().to_string();
    let body = serde_json::to_vec(&Envelope {
        schema_version: 1,
        event,
        delivery_id: id.clone(),
        sent_at: now.to_rfc3339(),
        data,
    })
    .map_err(|error| WebhookError::Invalid(error.to_string()))?;
    Ok(Delivery {
        id,
        timestamp: now.timestamp().to_string(),
        body,
    })
}

fn meeting_data(meeting: &Meeting, summary_status: SummaryStatus) -> Value {
    let speaker_names: HashMap<_, _> = meeting
        .speakers
        .iter()
        .map(|speaker| (speaker.user_id, speaker.display_name.as_str()))
        .collect();
    let participants: Vec<_> = meeting
        .speakers
        .iter()
        .filter(|speaker| !speaker.is_bot)
        .map(|speaker| {
            json!({
                "id": speaker.user_id.to_string(),
                "sourceId": speaker.source_id,
                "audioKind": speaker.audio_kind,
                "displayName": speaker.display_name,
                "username": speaker.username,
                "avatarUrl": speaker.avatar_url,
            })
        })
        .collect();
    let transcript: Vec<_> = meeting
        .utterances
        .iter()
        .map(|utterance| {
            json!({
                "id": utterance.id,
                "speakerId": utterance.speaker_id.to_string(),
                "speakerName": speaker_names
                    .get(&utterance.speaker_id)
                    .copied()
                    .unwrap_or("Desconocido"),
                "startMs": utterance.start_ms,
                "endMs": utterance.end_ms,
                "text": utterance.text,
                "confidence": utterance.confidence,
            })
        })
        .collect();
    let duration_ms = meeting.meta.ended_at.map(|ended| {
        ended
            .signed_duration_since(meeting.meta.started_at)
            .num_milliseconds()
            .max(0) as u64
    });

    json!({
        "id": meeting.meta.id,
        "guild": {
            "id": meeting.meta.guild_id.to_string(),
            "name": meeting.meta.guild_name,
        },
        "channel": {
            "id": meeting.meta.channel_id.to_string(),
            "name": meeting.meta.channel_name,
        },
        "startedAt": meeting.meta.started_at,
        "endedAt": meeting.meta.ended_at,
        "durationMs": duration_ms,
        "participants": participants,
        "transcript": transcript,
        "summaryStatus": summary_status,
        "summary": meeting.summary,
    })
}

async fn send_once(
    client: &Client,
    subscription: &WebhookSubscription,
    url: Url,
    event: &'static str,
    delivery: &Delivery,
    attempt: u8,
) -> Result<(), WebhookError> {
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-kuali-event", event)
        .header("x-kuali-delivery", &delivery.id)
        .header("x-kuali-timestamp", &delivery.timestamp)
        .header("x-kuali-attempt", u16::from(attempt))
        .header(
            "x-kuali-signature",
            signature(&subscription.secret, &delivery.timestamp, &delivery.body),
        )
        .body(delivery.body.clone())
        .send()
        .await?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let message: String = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    Err(WebhookError::Status { status, message })
}

pub async fn deliver_completed(
    subscription: &WebhookSubscription,
    meeting: &Meeting,
    summary_status: SummaryStatus,
) -> Result<(), WebhookError> {
    let url = validate(subscription)?;
    let client = client()?;
    let delivery = delivery(EVENT_COMPLETED, meeting_data(meeting, summary_status))?;
    let mut last_error = None;

    for attempt in 1..=MAX_ATTEMPTS {
        match send_once(
            &client,
            subscription,
            url.clone(),
            EVENT_COMPLETED,
            &delivery,
            attempt,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if attempt < MAX_ATTEMPTS && error.retryable() => {
                last_error = Some(error);
                tokio::time::sleep(if attempt == 1 {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(5)
                })
                .await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.expect("every failed attempt retains its error"))
}

pub async fn test(subscription: &WebhookSubscription) -> Result<String, WebhookError> {
    let url = validate(subscription)?;
    let client = client()?;
    let delivery = delivery(
        EVENT_TEST,
        json!({
            "subscriptionId": subscription.id,
            "subscriptionName": subscription.name,
            "message": "Kuali pudo entregar y firmar este webhook.",
        }),
    )?;
    send_once(&client, subscription, url, EVENT_TEST, &delivery, 1).await?;
    Ok("Webhook recibido correctamente".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kuali_core::{ActionItem, MeetingMeta, MeetingSummary, Speaker, Utterance, WebhookScope};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn complete_meeting() -> Meeting {
        let mut meeting = Meeting::new(MeetingMeta {
            id: "meeting-1".into(),
            display_title: None,
            guild_id: 123,
            guild_name: "Servidor".into(),
            channel_id: 456,
            channel_name: "Producto".into(),
            started_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            ended_at: Some(Utc.timestamp_opt(1_700_000_060, 0).unwrap()),
        });
        meeting.speakers.push(Speaker {
            user_id: 789,
            source_id: Some("teams-user-42".into()),
            audio_kind: Some("separate".into()),
            display_name: "Ana".into(),
            username: "ana".into(),
            avatar_url: None,
            color: "#fff".into(),
            is_bot: false,
        });
        meeting.utterances.push(Utterance {
            id: "u1".into(),
            speaker_id: 789,
            start_ms: 1_000,
            end_ms: 2_000,
            text: "Hay que publicar el viernes".into(),
            confidence: Some(0.95),
        });
        meeting.summary = Some(MeetingSummary {
            title: "Publicación del viernes".into(),
            overview: "Se acordó publicar".into(),
            key_points: vec!["Publicación".into()],
            decisions: vec!["Publicar el viernes".into()],
            action_items: vec![ActionItem {
                id: "task-1".into(),
                text: "Publicar".into(),
                assignee: Some("Ana".into()),
                due: Some("viernes".into()),
                source_ms: Some(1_000),
                done: false,
            }],
            open_questions: vec!["¿A qué hora?".into()],
            generated_by: "test".into(),
        });
        meeting
    }

    fn subscription(scope: WebhookScope) -> WebhookSubscription {
        WebhookSubscription {
            id: "hook-1".into(),
            name: "App externa".into(),
            url: "http://localhost:3000/kuali".into(),
            secret: "1234567890abcdef".into(),
            enabled: true,
            scope,
        }
    }

    #[test]
    fn a_channel_subscription_matches_only_its_channel() {
        let meeting = complete_meeting();
        let matching = subscription(WebhookScope::Channel {
            guild_id: "123".into(),
            channel_id: "456".into(),
        });
        let other = subscription(WebhookScope::Channel {
            guild_id: "123".into(),
            channel_id: "999".into(),
        });
        assert!(matching.matches(&meeting.meta));
        assert!(!other.matches(&meeting.meta));
    }

    #[test]
    fn completed_payload_contains_transcript_and_every_summary_section() {
        let payload = meeting_data(&complete_meeting(), SummaryStatus::Ready);
        assert_eq!(
            payload["transcript"][0]["text"],
            "Hay que publicar el viernes"
        );
        assert_eq!(payload["transcript"][0]["speakerId"], "789");
        assert_eq!(payload["participants"][0]["sourceId"], "teams-user-42");
        assert_eq!(payload["participants"][0]["audioKind"], "separate");
        assert_eq!(payload["summary"]["keyPoints"][0], "Publicación");
        assert_eq!(payload["summary"]["decisions"][0], "Publicar el viernes");
        assert_eq!(payload["summary"]["actionItems"][0]["assignee"], "Ana");
        assert_eq!(payload["summary"]["openQuestions"][0], "¿A qué hora?");
    }

    #[test]
    fn signature_covers_timestamp_and_exact_body() {
        let secret = "1234567890abcdef";
        let first = signature(secret, "100", br#"{"a":1}"#);
        assert_eq!(first.len(), "sha256=".len() + 64);
        assert_ne!(first, signature(secret, "101", br#"{"a":1}"#));
        assert_ne!(first, signature(secret, "100", br#"{"a":2}"#));
    }

    #[tokio::test]
    async fn delivery_posts_the_signed_complete_envelope() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header_end = at + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    break (header_end, content_length);
                }
            };
            while request.len() < header_end + content_length {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending the body");
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            request
        });

        let mut hook = subscription(WebhookScope::All);
        hook.url = format!("http://{address}/kuali");
        deliver_completed(&hook, &complete_meeting(), SummaryStatus::Ready)
            .await
            .unwrap();

        let request = receiver.await.unwrap();
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
        let payload: Value = serde_json::from_slice(&request[header_end..]).unwrap();
        assert!(headers.contains("x-kuali-event: meeting.completed"));
        assert!(headers.contains("x-kuali-signature: sha256="));
        assert_eq!(payload["schemaVersion"], 1);
        assert_eq!(payload["data"]["transcript"][0]["speakerName"], "Ana");
        assert_eq!(
            payload["data"]["summary"]["actionItems"][0]["text"],
            "Publicar"
        );
    }

    #[test]
    fn only_transient_failures_are_retried() {
        assert!(WebhookError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: String::new(),
        }
        .retryable());
        assert!(WebhookError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: String::new(),
        }
        .retryable());
        assert!(!WebhookError::Status {
            status: StatusCode::BAD_REQUEST,
            message: String::new(),
        }
        .retryable());
    }
}
