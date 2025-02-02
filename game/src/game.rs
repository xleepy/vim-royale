use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

use crate::{
    connection::{ConnectionMessage, SerializationType},
    game_comms::{GameComms, GameMessage},
    player::{spawn_player_stream, Player, PlayerSink, PlayerWebSink, PlayerWebStream},
};
use anyhow::{Result, anyhow};
use encoding::server::{self, ServerMessage, WHO_AM_I_CLIENT, WHO_AM_I_UNKNOWN};

use futures::StreamExt;
use log::{error, info, warn};
use map::map::Map;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_tungstenite::tungstenite::Message;

const PLAYER_COUNT: usize = 100;
const FPS: u128 = 16_666;
const ENTITY_RANGE: u16 = 500;

struct Game<const P: usize> {
    seed: u32,
    _map: Map,
    players: [Option<Player>; P],
    player_count: Arc<AtomicU8>,
    ser_type: SerializationType,
    game_id: u32,
    rx: Receiver<ConnectionMessage>,
    tx: Sender<ConnectionMessage>,
}

fn create_player_start_msg(player: &Player, seed: u32) -> server::Message {
    return server::Message::PlayerStart(server::PlayerStart {
        entity_id: player.id as usize * ENTITY_RANGE as usize,
        position: player.position,
        range: ENTITY_RANGE,
        seed,
    });
}

impl<const P: usize> Game<P> {
    pub fn new(
        seed: u32,
        game_id: u32,
        player_count: Arc<AtomicU8>,
        ser_type: SerializationType,
    ) -> Self {
        let players = std::array::from_fn(|_| None);
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        return Game {
            _map: Map::new(seed),
            player_count,
            players,
            game_id,
            seed,
            ser_type,
            rx,
            tx,
        };
    }

    fn process_message(&mut self, msg: ConnectionMessage) {
        match msg {
            ConnectionMessage::Msg(msg) => info!("[GAME]: ServerMessage {:?}", msg),

            ConnectionMessage::Close(id) => {
                info!("[GAME]: ConnectionClosed {:?}", id);
                self.players[id as usize] = None;
                self.player_count.fetch_sub(1, Ordering::Relaxed);
            },

            x => info!("[GAME]: ConnectionMessage {:?}", x),
        }
    }

    fn get_messages(&mut self) -> Vec<ConnectionMessage> {
        let mut msgs = vec![];
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }

        return msgs;
    }

    async fn run(&mut self) -> Result<()> {
        error!("[GAME]: game run game_id={}, seed={}", self.game_id, self.seed);
        let start = std::time::Instant::now();
        let mut tick = 0;

        loop {
            tick += 1;

            // 1. get every message sent to the sink
            // 2. process and update game state
            // 3. respond to any players with msgs
            // 4. sleep some amount of time

            // 1.
            let msgs = self.get_messages();
            if !msgs.is_empty() {
                for msg in msgs {
                    self.process_message(msg);
                }
            }

            let current = start.elapsed().as_micros();
            let next_frame = tick * FPS;

            if current < next_frame {
                let duration = (next_frame - current) as u64;
                let duration = std::time::Duration::from_micros(duration);
                tokio::time::sleep(duration).await;
            }

            // check leave conditions.
            if self.player_count.load(Ordering::Relaxed) == 0 {
                break;
            }
        }

        self.error("Game Completed");
        return Ok(());
    }

    fn is_ready(&self) -> bool {
        let id = self.player_count.load(Ordering::Relaxed);
        info!("[GAME] Ready check {} == {}", id, 1);
        return id == 1;
    }

    async fn add_player(
        &mut self,
        mut stream: PlayerWebStream,
        mut sink: PlayerWebSink,
    ) -> Result<()> {
        let player_id = self.player_count.fetch_add(1, Ordering::Relaxed);

        let clock_diff = Player::sync_clock(10, &mut stream, &mut sink).await.unwrap_or(0);
        self.error(&format!("creating player({}): synced clock with offset {}", player_id, clock_diff));

        let player = Player {
            position: (256, 256),
            id: player_id,
            sink: PlayerSink::new(player_id, sink),
            clock_diff,
        };

        spawn_player_stream(player_id, stream, self.ser_type, self.tx.clone());

        self.players[player_id as usize] = Some(player);

        return Ok(());
    }

    // TODO: this probably has to be more robust to not cause a panic
    async fn start_game(&mut self) -> Result<()> {
        let mut handles = vec![];

        self.warn("starting game");
        for player in self.players.iter_mut() {
            if let Some(player) = player {
                let msg = create_player_start_msg(player, self.seed);
                handles.push(player.sink.send(msg));
            }
        }

        let _ = futures::future::join_all(handles).await;

        // TODO: Close any connections that errored and get rid of them.

        return Ok(());
    }

    fn error(&self, msg: &str) {
        error!(
            "[GAME]: msg={} id={} player_count={} seed={}",
            msg,
            self.game_id,
            self.player_count.load(Ordering::Relaxed),
            self.seed
        );
    }

    fn warn(&self, msg: &str) {
        warn!(
            "[GAME]: msg={} id={} player_count={} seed={}",
            msg,
            self.game_id,
            self.player_count.load(Ordering::Relaxed),
            self.seed
        );
    }

    fn info_string(&self) -> String {
        return format!(
            "id={} player_count={} seed={}",
            self.game_id,
            self.player_count.load(Ordering::Relaxed),
            self.seed
        );
    }
}

fn whoami<T>(msg: Option<Result<Message, T>>) -> Result<u8> {
    match msg {
        Some(Ok(Message::Binary(msg))) => {
            let msg = ServerMessage::deserialize(&msg)?;
            match msg.msg {
                server::Message::Whoami(whoami) => {
                    return Ok(whoami);
                }
                _ => {
                    return Err(anyhow!("expected whoami message"));
                }
            }
        }
        _ => return Ok(WHO_AM_I_UNKNOWN),
    }
}

pub async fn game_run(
    seed: u32,
    player_count: Arc<AtomicU8>,
    game_id: u32,
    mut comms: GameComms,
    ser_type: SerializationType,
) {
    let mut game = Game::<PLAYER_COUNT>::new(seed, game_id, player_count, ser_type);
    error!("[GAME-RUNNER]: New game started game_id={}, seed={}", game_id, seed);

    loop {
        match comms.receiver.recv().await {
            Some(GameMessage::Connection(mut stream, sink)) => {
                info!(
                    "[GAME-RUNNER] new player connection for game {}",
                    game.info_string()
                );

                let msg = whoami(stream.next().await);

                if let Ok(WHO_AM_I_CLIENT) = msg {
                    _ = game.add_player(stream, sink).await;
                    if game.is_ready() {
                        break;
                    }
                } else {
                    _ = sink.reunite(stream).map(|mut x| {
                        _ = x.close(None)
                    });
                    continue;
                }
            }

            Some(msg) => {
                game.error(&format!(
                    "Game comms channel gave a non connection message {:?}.",
                    msg
                ));
                unreachable!("this should never happen");
            }

            None => {
                game.error("Game comms channel closed");
                unreachable!("this should never happen");
            }
        }
    }

    /*
    match comms.sender.send(GameMessage::Start).await {
        Ok(_) => {
            game.warn("Game sent start");
        }
        Err(_) => {
            game.error("Game failed to send start");
            unreachable!("this should never happen in production.");
        }
    }
    */

    match game.start_game().await {
        Ok(_) => {
            game.warn("started");
        }
        Err(e) => {
            game.error(&format!("faled to start: {:?}", e));
        }
    }

    match game.run().await {
        Ok(_) => {
            game.warn("finished successfully");
        }
        Err(e) => {
            game.warn(&format!("finished with error {}", e));
        }
    }

    /*
    _ = comms.sender.send(GameMessage::Close(game.game_id as usize)).await;
    */
}
