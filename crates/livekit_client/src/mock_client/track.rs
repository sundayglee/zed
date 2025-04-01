use std::sync::Arc;

use crate::{
    ParticipantIdentity, TrackSid,
    test::{TestServerAudioTrack, TestServerVideoTrack, WeakRoom},
};

#[derive(Clone, Debug)]
pub struct LocalVideoTrack {}

#[derive(Clone, Debug)]
pub struct LocalAudioTrack {}

#[derive(Clone, Debug)]
pub struct RemoteVideoTrack {
    pub(crate) server_track: Arc<TestServerVideoTrack>,
    pub(crate) _room: WeakRoom,
}

#[derive(Clone, Debug)]
pub struct RemoteAudioTrack {
    pub(crate) server_track: Arc<TestServerAudioTrack>,
    pub(crate) room: WeakRoom,
}

impl RemoteAudioTrack {
    pub fn sid(&self) -> TrackSid {
        self.server_track.sid.clone()
    }

    pub fn publisher_id(&self) -> ParticipantIdentity {
        self.server_track.publisher_id.clone()
    }

    pub fn enabled(&self) -> bool {
        if let Some(room) = self.room.upgrade() {
            !room
                .0
                .lock()
                .paused_audio_tracks
                .contains(&self.server_track.sid)
        } else {
            false
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        let Some(room) = self.room.upgrade() else {
            return;
        };
        if enabled {
            room.0
                .lock()
                .paused_audio_tracks
                .remove(&self.server_track.sid);
        } else {
            room.0
                .lock()
                .paused_audio_tracks
                .insert(self.server_track.sid.clone());
        }
    }
}

impl RemoteVideoTrack {
    pub fn sid(&self) -> TrackSid {
        self.server_track.sid.clone()
    }

    pub fn publisher_id(&self) -> ParticipantIdentity {
        self.server_track.publisher_id.clone()
    }

    pub(crate) fn set_enabled(&self, _enabled: bool) {}
}
