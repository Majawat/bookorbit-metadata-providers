mod mapping;
mod metron;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub auth_token: Option<String>,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let base_url = env("METRON_BASE_URL", "https://metron.cloud/api/");
    let api_key = match require_api_key(std::env::var("METRON_API_KEY").ok()) {
        Ok(key) => key,
        Err(msg) => {
            eprintln!("[provider] [fatal] {msg}");
            std::process::exit(1);
        }
    };
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-metron-provider/{VERSION}"),
    );
    let min_interval = env_millis("METRON_MIN_INTERVAL_MS", 3500);
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
    };
    let client = metron::Client::new(&base_url, &api_key, &user_agent, min_interval);
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let listener = tokio::net::TcpListener::bind((
        "0.0.0.0",
        port.parse::<u16>().expect("PORT must be a port number"),
    ))
    .await
    .expect("could not bind the port");
    eprintln!("[provider] [start] port={port} api={base_url} version={VERSION} - listening");
    axum::serve(listener, app).await.expect("server stopped");
}

fn require_api_key(raw: Option<String>) -> Result<String, &'static str> {
    raw.map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or("METRON_API_KEY is required")
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn env_millis(key: &str, fallback: u64) -> Duration {
    Duration::from_millis(
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_missing_or_blank_api_key() {
        assert!(require_api_key(None).is_err());
        assert!(require_api_key(Some(String::new())).is_err());
        assert!(require_api_key(Some("   ".into())).is_err());
        assert_eq!(
            require_api_key(Some("  token-123 ".into())),
            Ok("token-123".into())
        );
    }
}
