use anyhow::{anyhow, Result};
use livekit_api::services::room::RoomClient;
use tracing::{info, warn};

// How many times to retry when the participant hasn't joined the LiveKit room yet.
const SUBSCRIBE_RETRIES: u32 = 10;
// Initial delay between retries (doubles each attempt, up to ~5 s).
const SUBSCRIBE_RETRY_INITIAL_MS: u64 = 300;

/// Thin wrapper around the LiveKit RoomClient scoped to one room.
pub struct LiveKitRoom {
    client: RoomClient,
    pub room_name: String,
}

impl LiveKitRoom {
    pub fn new(
        lk_url: &str,
        api_key: &str,
        api_secret: &str,
        room_name: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            client: RoomClient::with_api_key(lk_url, api_key, api_secret),
            room_name: room_name.into(),
        })
    }

    /// Subscribe `listener_identity` to the audio track published by `speaker_identity`.
    /// Called when speaker enters listener's zone.
    /// Retries with exponential backoff when the speaker hasn't joined LiveKit yet
    /// (race between token delivery and the client actually connecting).
    pub async fn subscribe(&self, listener_identity: &str, speaker_identity: &str) -> Result<()> {
        info!(
            listener = listener_identity,
            speaker = speaker_identity,
            "DyingstarAudioPlugin - subscribing"
        );

        let mut delay_ms = SUBSCRIBE_RETRY_INITIAL_MS;
        for attempt in 1..=SUBSCRIBE_RETRIES {
            match self.client.get_participant(&self.room_name, speaker_identity).await {
                Ok(participant) => {
                    let track_sids: Vec<String> = participant
                        .tracks
                        .iter()
                        .filter(|t| t.r#type == 0) // 0 = Audio in livekit_protocol
                        .map(|t| t.sid.clone())
                        .collect();

                    if track_sids.is_empty() {
                        // Participant is in the room but hasn't published audio yet — retry.
                        warn!(speaker = speaker_identity, attempt, "no audio track yet, retrying");
                    } else {
                        self.client
                            .update_subscriptions(
                                &self.room_name,
                                listener_identity,
                                track_sids,
                                true,
                            )
                            .await?;
                        info!("🔊 DyingstarAudioPlugin - subscribed {} to {}", listener_identity, speaker_identity);
                        return Ok(());
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not_found") || msg.contains("participant does not exist") {
                        warn!(
                            speaker = speaker_identity,
                            attempt,
                            "participant not in room yet, retrying in {}ms",
                            delay_ms
                        );
                    } else {
                        // Unexpected error — don't retry.
                        return Err(anyhow!("twirp error: {}", e));
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            delay_ms = (delay_ms * 2).min(5_000);
        }

        Err(anyhow!(
            "subscribe: speaker '{}' never appeared in room '{}' after {} attempts",
            speaker_identity, self.room_name, SUBSCRIBE_RETRIES
        ))
    }

    /// Unsubscribe `listener_identity` from the audio track of `speaker_identity`.
    /// Called when speaker exits listener's zone.
    pub async fn unsubscribe(&self, listener_identity: &str, speaker_identity: &str) -> Result<()> {
        tracing::info!(
            listener = listener_identity,
            speaker = speaker_identity,
            "DyingstarAudioPlugin - unsubscribing"
        );

        let participant = self
            .client
            .get_participant(&self.room_name, speaker_identity)
            .await?;

        let track_sids: Vec<String> = participant
            .tracks
            .iter()
            .filter(|t| t.r#type == 0)
            .map(|t| t.sid.clone())
            .collect();

        if track_sids.is_empty() {
            return Ok(()); // speaker already gone, nothing to do
        }

        self.client
            .update_subscriptions(
                &self.room_name,
                listener_identity,
                track_sids,
                false,
            )
            .await?;

        Ok(())
    }
}