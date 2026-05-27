use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use base64;
use uuid::Uuid;
use ds_common::events::DSErrorMessage;
use horizon_event_system::{
    ClientConnectionRef, EventSystem, PlayerId, Vec3
};
use tracing::{debug, error, info, warn};
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInit {
    pub data: PlayerInitData,
    pub player_id: PlayerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInitData {
    pub token: String,
}

/// JWT claims decoded from the token (without signature verification).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject — typically the user's unique ID in the auth provider
    pub sub: Option<String>,
    pub exp: Option<i64>,
    pub iat: Option<i64>,
    /// Any extra fields (username, email, roles, …)
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Decode a JWT payload without verifying the signature.
/// Returns `None` and logs a warning if the token is malformed.
fn decode_jwt_payload(token: &str) -> Option<JwtClaims> {
    // A JWT is three base64url segments separated by '.'.
    let payload_b64 = token.split('.').nth(1)?;

    // base64url uses '-' and '_' instead of '+' and '/', with no padding.
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload_b64,
    )
    .ok()?;

    serde_json::from_slice(&decoded).ok()
}

pub async fn handle_player_init(
    event: PlayerInit,
    player_id: PlayerId,
    connection: ClientConnectionRef,
    events: Arc<EventSystem>,
    server_state_ok: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {

    if !server_state_ok.load(Ordering::Relaxed) {
        warn!("Player {:?} tried to init but server is not ready yet", player_id);
        let payload = serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "code": 1338,
            "message": "Server not yet ready"
        }))
        .expect("failed to serialize error payload");
        if let Err(e) = connection.respond(&payload).await {
            error!("Failed to send server-not-ready error to client: {}", e);
        }
        return Ok(());
    }

    // Check if authentication bypass is enabled (development/testing only).
    let bypass_auth = std::env::var("BYPASS_AUTH")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if bypass_auth {
        warn!("BYPASS_AUTH is enabled — skipping JWT validation for player {:?}", player_id);
    }

    // Resolve player_db_id and player_name either via JWT or bypass path.
    let (player_db_id, player_name) = if bypass_auth {
        let id = Uuid::new_v4().to_string();
        let name = format!("Player_{}", &id[..8]);
        // id = "35941259-8205-4d46-b8a1-8837306d1587".to_string();
        // name = "ddurieux".to_string();
        (id, name)
    } else {
        // Decode the JWT payload (no signature verification — we trust the token
        // was already validated upstream, or verification will be added later).
        let claims = decode_jwt_payload(&event.data.token);
        if let Some(ref c) = claims {
            debug!("JWT claims decoded: sub={:?} extra={:?}", c.sub, c.extra);
        } else {
            warn!("Could not decode JWT payload for player_id {:?}", player_id);
        }

        // Extract the player name from the JWT 'preferred_username' claim.
        let player_name: String = match claims.as_ref()
            .and_then(|c| c.extra.get("preferred_username"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(name) => name.to_string(),
            None => {
                error!("JWT missing or empty 'preferred_username' claim for player_id {:?}", player_id);
                return Err("missing preferred_username in JWT".into());
            }
        };

        // Use the UUID from the JWT 'sub' claim as the player's database ID.
        let sub = claims.as_ref().and_then(|c| c.sub.as_deref()).unwrap_or("");
        let player_db_id = match Uuid::parse_str(sub) {
            Ok(id) => id.to_string(),
            Err(e) => {
                error!("JWT 'sub' is not a valid UUID ({:?}): {}", sub, e);
                return Err(Box::new(e));
            }
        };

        (player_db_id, player_name)
    };


    let payload = serde_json::to_vec(&serde_json::json!({
        "player_id": player_db_id,
        "type": "init_ack"
    })).expect("failed to serialize payload");

    if let Err(e) = connection.respond(&payload).await
    {
        error!("Failed to send init_ack to client: {}", e);
    }

    // Update the player_id stored in the connection manager
    // This replaces the temporary connection-level player_id with the database player_id
    if let Err(e) = events.emit_core("update_player_id", &serde_json::json!({
        "old_player_id": player_id,
        "new_player_id": player_db_id,
        "connection_id": player_id,  // The connection_id is currently the old player_id
    })).await
    {
        error!("Failed to emit update_player_id event: {}", e);
    }

    info!(
        "auth (player_init): emitting bridge_persistence:player_spawn for player_uuid={} player_name={}",
        player_db_id, player_name
    );
    if let Err(e) = events.emit_plugin("bridge_persistence", "player_spawn", &serde_json::json!({
        "object_type": "player",
        "object_uuid": player_db_id,
        "object_data": {
            "name": player_name,
        }
    })).await
    {
        error!("Failed to emit plugin event to gorcplugin: {}", e);
    } else {
        info!("auth (player_init): bridge_persistence:player_spawn emitted successfully");
    }

    Ok(())
}
