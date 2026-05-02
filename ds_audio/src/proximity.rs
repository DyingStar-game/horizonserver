use anyhow::Result;
use livekit_api::services::room::RoomClient;

/// Thin wrapper around the LiveKit RoomClient scoped to one room.
pub struct LiveKitRoom {
    client: RoomClient,
    pub room_name: String,
}

impl LiveKitRoom {
    pub fn new(lk_url: &str, room_name: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: RoomClient::new(lk_url)?,
            room_name: room_name.into(),
        })
    }

    /// Subscribe `listener_identity` to the audio track published by `speaker_identity`.
    /// Called when speaker enters listener's zone.
    pub async fn subscribe(&self, listener_identity: &str, speaker_identity: &str) -> Result<()> {
        tracing::debug!(
            listener = listener_identity,
            speaker = speaker_identity,
            "subscribing"
        );

        // Fetch the speaker's current track SIDs
        let participant = self
            .client
            .get_participant(&self.room_name, speaker_identity)
            .await?;

        let track_sids: Vec<String> = participant
            .tracks
            .iter()
            .filter(|t| t.r#type == 0) // 0 = Audio in livekit_protocol
            .map(|t| t.sid.clone())
            .collect();

        if track_sids.is_empty() {
            tracing::warn!(speaker = speaker_identity, "no audio track found yet");
            return Ok(());
        }

        self.client
            .update_subscriptions(
                &self.room_name,
                listener_identity,
                track_sids,
                true,
            )
            .await?;

        Ok(())
    }

    /// Unsubscribe `listener_identity` from the audio track of `speaker_identity`.
    /// Called when speaker exits listener's zone.
    pub async fn unsubscribe(&self, listener_identity: &str, speaker_identity: &str) -> Result<()> {
        tracing::debug!(
            listener = listener_identity,
            speaker = speaker_identity,
            "unsubscribing"
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