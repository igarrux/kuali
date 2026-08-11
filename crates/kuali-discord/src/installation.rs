//! Discord's official server-install flow for a user-owned Kuali bot.

use std::time::Duration;

use reqwest::{header, StatusCode};
use serde::Deserialize;
use serenity::model::Permissions;

const CURRENT_APPLICATION_URL: &str = "https://discord.com/api/v10/oauth2/applications/@me";
const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";

#[derive(Debug, thiserror::Error)]
pub enum DiscordInstallationError {
    #[error("pega el token del bot de Discord para continuar")]
    MissingToken,
    #[error("Discord rechazó el token. Restablécelo en Bot y pega el nuevo valor")]
    InvalidToken,
    #[error("Discord limitó temporalmente la solicitud. Espera un momento e inténtalo nuevamente")]
    RateLimited,
    #[error("Discord no permitió preparar la instalación (HTTP {0})")]
    Discord(u16),
    #[error("Discord devolvió una aplicación sin un identificador válido")]
    InvalidApplication,
    #[error("no pude consultar la aplicación del bot: {0}")]
    Network(#[from] reqwest::Error),
}

#[derive(Deserialize)]
struct CurrentApplication {
    id: String,
}

fn required_permissions() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::EMBED_LINKS
        | Permissions::ATTACH_FILES
        | Permissions::CONNECT
        | Permissions::SPEAK
        | Permissions::USE_VAD
}

fn installation_url_for(application_id: &str) -> Result<String, DiscordInstallationError> {
    if application_id.is_empty() || !application_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DiscordInstallationError::InvalidApplication);
    }
    Ok(format!(
        "{AUTHORIZE_URL}?client_id={application_id}&scope=bot%20applications.commands&permissions={}&integration_type=0",
        required_permissions().bits()
    ))
}

/// Validates a user-owned bot token and returns its exact server-install URL.
pub async fn installation_url(token: &str) -> Result<String, DiscordInstallationError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(DiscordInstallationError::MissingToken);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .get(CURRENT_APPLICATION_URL)
        .header(header::AUTHORIZATION, format!("Bot {token}"))
        .header(
            header::USER_AGENT,
            format!(
                "Kuali/{} (https://github.com/igarrux/kuali)",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .send()
        .await?;

    match response.status() {
        status if status.is_success() => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(DiscordInstallationError::InvalidToken);
        }
        StatusCode::TOO_MANY_REQUESTS => return Err(DiscordInstallationError::RateLimited),
        status => return Err(DiscordInstallationError::Discord(status.as_u16())),
    }

    let application: CurrentApplication = response.json().await?;
    installation_url_for(&application.id)
}

#[cfg(test)]
mod tests {
    use super::{installation_url_for, required_permissions, DiscordInstallationError};
    use serenity::model::Permissions;

    #[test]
    fn install_url_requests_only_kualis_required_permissions() {
        let expected = Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::SEND_MESSAGES_IN_THREADS
            | Permissions::EMBED_LINKS
            | Permissions::ATTACH_FILES
            | Permissions::CONNECT
            | Permissions::SPEAK
            | Permissions::USE_VAD;
        assert_eq!(required_permissions(), expected);

        let url = installation_url_for("123456789012345678").unwrap();
        assert_eq!(
            url,
            format!(
                "https://discord.com/oauth2/authorize?client_id=123456789012345678&scope=bot%20applications.commands&permissions={}&integration_type=0",
                expected.bits()
            )
        );
        assert!(!url.contains("administrator"));
    }

    #[test]
    fn install_url_rejects_non_snowflake_application_ids() {
        assert!(matches!(
            installation_url_for("../../settings"),
            Err(DiscordInstallationError::InvalidApplication)
        ));
    }
}
