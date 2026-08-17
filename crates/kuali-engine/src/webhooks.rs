//! Delivers completed meetings to subscribed external applications.

use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD as BASE64_NO_PAD};
use base64::Engine as _;
use chrono::Utc;
use hmac::{Hmac, Mac};
use kuali_core::{Meeting, WebhookSubscription};
use reqwest::header::RETRY_AFTER;
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

const EVENT_COMPLETED: &str = "meeting.completed";
const EVENT_TEST: &str = "webhook.test";
/// Standard Webhooks' example schedule spans transient outages instead of
/// dropping an event after a few seconds. The first attempt has no delay.
const RETRY_DELAYS_SECS: [u64; 10] = [
    0, 5, 300, 1_800, 7_200, 18_000, 36_000, 50_400, 72_000, 86_400,
];

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
    Status {
        status: StatusCode,
        message: String,
        retry_after: Option<Duration>,
    },
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

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct Envelope {
    #[serde(rename = "type")]
    event_type: &'static str,
    timestamp: String,
    data: Value,
}

struct Delivery {
    id: String,
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
    signing_key(&subscription.secret)?;
    Ok(url)
}

fn client() -> Result<Client, WebhookError> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("Kuali/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn signing_key(secret: &str) -> Result<Vec<u8>, WebhookError> {
    let encoded = secret
        .trim()
        .strip_prefix("whsec_")
        .ok_or_else(|| WebhookError::Invalid("signing secret must start with whsec_".into()))?;
    let key = BASE64
        .decode(encoded)
        .or_else(|_| BASE64_NO_PAD.decode(encoded))
        .map_err(|_| WebhookError::Invalid("signing secret is not valid base64".into()))?;
    if !(24..=64).contains(&key.len()) {
        return Err(WebhookError::Invalid(
            "signing secret must contain between 24 and 64 bytes".into(),
        ));
    }
    Ok(key)
}

fn signature(
    secret: &str,
    message_id: &str,
    timestamp: &str,
    body: &[u8],
) -> Result<String, WebhookError> {
    let key = signing_key(secret)?;
    let mut mac = HmacSha256::new_from_slice(&key)
        .map_err(|error| WebhookError::Invalid(error.to_string()))?;
    mac.update(message_id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!("v1,{}", BASE64.encode(mac.finalize().into_bytes())))
}

fn delivery(
    event: &'static str,
    event_timestamp: String,
    data: Value,
) -> Result<Delivery, WebhookError> {
    let id = format!("msg_{}", Uuid::new_v4().simple());
    let body = serde_json::to_vec(&Envelope {
        event_type: event,
        timestamp: event_timestamp,
        data,
    })
    .map_err(|error| WebhookError::Invalid(error.to_string()))?;
    Ok(Delivery { id, body })
}

fn retry_delay(base_secs: u64) -> Duration {
    if base_secs == 0 {
        return Duration::ZERO;
    }
    // A random delivery-sized value adds up to 20% positive jitter so many
    // Kuali instances do not retry a recovering endpoint in lockstep.
    let max_jitter = (base_secs / 5).max(1);
    let jitter = (Uuid::new_v4().as_u128() as u64) % (max_jitter + 1);
    Duration::from_secs(base_secs + jitter)
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
    delivery: &Delivery,
) -> Result<(), WebhookError> {
    // The delivery ID stays stable for idempotency, while every attempt gets a
    // fresh timestamp so receivers can enforce a replay window.
    let timestamp = Utc::now().timestamp().to_string();
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("webhook-id", &delivery.id)
        .header("webhook-timestamp", &timestamp)
        .header(
            "webhook-signature",
            signature(
                &subscription.secret,
                &delivery.id,
                &timestamp,
                &delivery.body,
            )?,
        )
        .body(delivery.body.clone())
        .send()
        .await?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let message: String = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    Err(WebhookError::Status {
        status,
        message,
        retry_after,
    })
}

pub async fn deliver_completed(
    subscription: &WebhookSubscription,
    meeting: &Meeting,
    summary_status: SummaryStatus,
) -> Result<(), WebhookError> {
    let url = validate(subscription)?;
    let client = client()?;
    let event_timestamp = meeting.meta.ended_at.unwrap_or_else(Utc::now).to_rfc3339();
    let delivery = delivery(
        EVENT_COMPLETED,
        event_timestamp,
        meeting_data(meeting, summary_status),
    )?;
    let mut last_error = None;

    let mut delay = Duration::ZERO;
    for attempt in 0..RETRY_DELAYS_SECS.len() {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match send_once(&client, subscription, url.clone(), &delivery).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 < RETRY_DELAYS_SECS.len() && error.retryable() => {
                delay = error
                    .retry_after()
                    .unwrap_or_else(|| retry_delay(RETRY_DELAYS_SECS[attempt + 1]));
                last_error = Some(error);
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
        Utc::now().to_rfc3339(),
        json!({
            "subscriptionId": subscription.id,
            "subscriptionName": subscription.name,
            "message": "Kuali pudo entregar y firmar este webhook.",
        }),
    )?;
    send_once(&client, subscription, url, &delivery).await?;
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
            tags: Vec::new(),
            folder: None,
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
            is_self: false,
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
            notes: Vec::new(),
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
            secret: "whsec_MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=".into(),
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
    fn signature_matches_the_official_standard_webhooks_vector() {
        let secret = "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD";
        let body = br#"{"email":"test@example.com","username":"test_user"}"#;
        let signature = signature(
            secret,
            "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk",
            "1649367553",
            body,
        )
        .unwrap();
        assert_eq!(signature, "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=");
    }

    #[test]
    fn legacy_or_weak_signing_secrets_are_rejected() {
        assert!(signing_key("1234567890abcdef").is_err());
        assert!(signing_key("whsec_not-base64").is_err());
        assert!(signing_key("whsec_dG9vLXNob3J0").is_err());
    }

    #[test]
    fn padded_and_unpadded_standard_secrets_are_accepted() {
        let padded = "whsec_MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
        let unpadded = padded.trim_end_matches('=');
        assert_eq!(signing_key(padded).unwrap(), signing_key(unpadded).unwrap());
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
        assert!(headers.contains("webhook-id: msg_"));
        assert!(headers.contains("webhook-timestamp: "));
        assert!(headers.contains("webhook-signature: v1,"));
        assert!(!headers.contains("x-kuali-"));
        assert_eq!(payload["type"], EVENT_COMPLETED);
        assert_eq!(payload["timestamp"], "2023-11-14T22:14:20+00:00");
        assert!(payload.get("schemaVersion").is_none());
        assert!(payload.get("deliveryId").is_none());
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
            retry_after: None,
        }
        .retryable());
        assert!(WebhookError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: String::new(),
            retry_after: Some(Duration::from_secs(30)),
        }
        .retryable());
        assert!(!WebhookError::Status {
            status: StatusCode::BAD_REQUEST,
            message: String::new(),
            retry_after: None,
        }
        .retryable());
    }
}
