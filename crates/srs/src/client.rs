use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use crate::message::{create_sguid, GameMessage, LatLngPosition};
use crate::voice_stream::VoiceStream;
use futures::channel::mpsc;
use tokio::sync::oneshot::Receiver;

#[derive(Debug, Clone)]
pub struct UnitInfo {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Client {
    sguid: String,
    name: String,
    freq: u64,
    m: String,
    pos: Arc<RwLock<LatLngPosition>>,
    unit: Option<UnitInfo>,
}

impl Client {
    pub fn new(name: &str, freq: u64, m: &str) -> Self {
        Client {
            sguid: create_sguid(),
            name: name.to_string(),
            freq,
            m: m.to_string(),
            pos: Arc::new(RwLock::new(LatLngPosition::default())),
            unit: None,
        }
    }

    pub fn sguid(&self) -> &str {
        &self.sguid
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn m(&self) -> &str {
        &self.m
    }
    pub fn freq(&self) -> u64 {
        self.freq
    }

    pub fn position(&self) -> LatLngPosition {
        let p = self.pos.read().unwrap();
        p.clone()
    }

    pub fn position_handle(&self) -> Arc<RwLock<LatLngPosition>> {
        self.pos.clone()
    }

    pub fn unit(&self) -> Option<&UnitInfo> {
        self.unit.as_ref()
    }

    pub fn set_position(&mut self, pos: LatLngPosition) {
        let mut p = self.pos.write().unwrap();
        *p = pos;
    }

    pub fn set_unit(&mut self, id: u32, name: &str) {
        self.unit = Some(UnitInfo {
            id,
            name: name.to_string(),
        });
    }

    /**
      Start sending updates to the specified server. If `game_source` is None,
      the client will act as a stationary transmitter using the position and
      frequency specified in the `Client` struct. It will not request any voice
      messages

      If the `game_source` is set, the position and frequencies of the game
      message will be sent, and voice requested
    */
    pub async fn start(
        self,
        addr: SocketAddr,
        game_source: Option<mpsc::UnboundedReceiver<GameMessage>>,
        shutdown_signal: Receiver<()>,
    ) -> Result<VoiceStream, anyhow::Error> {
        let stream = VoiceStream::new(self, addr, game_source, shutdown_signal).await?;
        Ok(stream)
    }
}
