//! Drives a [`Session`] over a NetherNet [`Connection`], carrying out what
//! its replies ask of the server.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bedrockrs_net::{Connection, Reliability};
use bedrockrs_plugins::{BlockChange, Damage, Dispatcher, Event, Player, Position};
use bedrockrs_protocol::batch::{self, Compression};
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::types::BlockPos;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::TICK_DURATION;
use crate::logins::{CONTROL_QUEUE, Control, LoginClaim};
use crate::players::{Joining, Membership, OUTBOUND_QUEUE};
use crate::server::{self, Server};

use super::{Reply, Session, SessionError, SessionEvent};

/// How long to wait for the client to hang up after we disconnect it, so the
/// Disconnect packet is delivered before the session is torn down.
const DISCONNECT_LINGER: Duration = Duration::from_secs(5);

/// How long after a player spawns plugins hear of it: 15 ticks, so messages
/// they send arrive once the client's HUD is ready.
const JOIN_EVENT_DELAY: Duration = Duration::from_millis(750);

/// Longest a chat message, damage, or a join or quit message waits for
/// plugins to decide whether to cancel it. Past that it goes ahead: a stuck
/// plugin must not stall the game.
const VERDICT_TIMEOUT: Duration = Duration::from_secs(2);

/// Serves one client until either side closes the connection.
pub async fn run(mut connection: Connection, server: Arc<Server>) {
    let entity_id = server.players.allocate_entity_id();
    let mut session = Session::new(
        connection.client_identity().cloned(),
        Arc::clone(&server.world),
        entity_id,
        server.default_game_mode,
    );
    session.set_game_rules(server.game_rules.values());
    let mut joined = None;
    serve(&mut connection, &server, &mut session, &mut joined).await;
    // However the session ended, remember where the player left. When the
    // server stops, everyone leaves at once and nobody needs telling.
    let closing = server.is_closing();
    if let Some((uuid, player)) = session.saved_player() {
        if !closing {
            tracing::info!("{} left the game", session.player());
        }
        server.save_player(uuid, &player);
    }
    // Plugins that heard of the join hear of the quit, and may replace
    // vanilla's message.
    if let Some(player) = joined {
        let event = Event::PlayerQuit(player.clone());
        if !cancelled(&server.plugins, event, "a quit message").await && !closing {
            server
                .players
                .broadcast_translation(LEFT_MESSAGE, &[player.name]);
        }
    }
}

/// How the packet loop goes on after delivering a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    /// The session ends; give the client time to read its last packets.
    Close,
    /// The connection is gone.
    Lost,
}

/// Vanilla's join and quit messages, in yellow, filled in with the player's name.
const JOINED_MESSAGE: &str = "§e%multiplayer.player.joined";
const LEFT_MESSAGE: &str = "§e%multiplayer.player.left";

/// The session's packet loop, until the connection closes or the session
/// ends it. Packets, ticks and what the rest of the server tells the session
/// all produce [`Reply`]s, which [`Link::deliver`] carries out alike.
/// `joined` is set to the player once plugins heard of their join.
async fn serve(
    connection: &mut Connection,
    server: &Server,
    session: &mut Session,
    joined: &mut Option<Player>,
) {
    let network_id = connection.network_id();
    // Packets other sessions and plugins send this player, e.g. chat.
    let (outbound, mut queued) = mpsc::channel::<Bytes>(OUTBOUND_QUEUE);
    // How the rest of the server reaches the session, once it claims its
    // player's UUID: a newer login kicks it, commands change its game mode.
    let (controls, mut control) = mpsc::channel::<Control>(CONTROL_QUEUE);
    let mut link = Link {
        server,
        entity_id: session.entity_id,
        compression: None,
        outbound,
        controls,
        membership: None,
        login_claim: None,
        plugin_player: None,
        joined: None,
    };
    // The plugin join event, waiting out [`JOIN_EVENT_DELAY`].
    let mut join_event: Option<(Pin<Box<tokio::time::Sleep>>, Player)> = None;
    // The player's own tick: picking items up, the void, regeneration.
    let mut ticks = tokio::time::interval(TICK_DURATION);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let flow = tokio::select! {
            message = connection.recv() => {
                let Some(message) = message else {
                    tracing::debug!(network_id, "client closed the connection");
                    return;
                };
                match batch::decode(&message.payload, link.compression.is_some()) {
                    Err(err) => {
                        let reply = SessionError::from(err).into_reply();
                        link.deliver(connection, session, reply).await
                    }
                    Ok(packets) => {
                        // Packets are handled in order, each with what it leads to.
                        let mut flow = Flow::Continue;
                        for packet in &packets {
                            let reply = session.handle(packet).unwrap_or_else(|err| err.into_reply());
                            flow = link.deliver(connection, session, reply).await;
                            if flow != Flow::Continue {
                                break;
                            }
                        }
                        flow
                    }
                }
            }
            Some(control) = control.recv() => {
                let reply = link.control(session, control);
                link.deliver(connection, session, reply).await
            }
            () = async { join_event.as_mut().expect("guarded").0.as_mut().await }, if join_event.is_some() => {
                // The player is past the loading screen: plugins hear of the
                // join, and may replace vanilla's message.
                let (_, player) = join_event.take().expect("guarded");
                *joined = Some(player.clone());
                let event = Event::PlayerJoin(player.clone());
                if !cancelled(&server.plugins, event, "a join message").await {
                    server
                        .players
                        .broadcast_translation(JOINED_MESSAGE, &[player.name]);
                }
                Flow::Continue
            }
            _ = ticks.tick(), if link.membership.is_some() => {
                let reply = session.tick(&server.items);
                link.deliver(connection, session, reply).await
            }
            Some(packet) = queued.recv() => {
                // Send everything already waiting in one batch.
                let mut packets = vec![packet];
                while let Ok(packet) = queued.try_recv() {
                    packets.push(packet);
                }
                if send(connection, packets.iter().map(|packet| &packet[..]), link.compression).await {
                    Flow::Continue
                } else {
                    Flow::Lost
                }
            }
        };
        if let Some(player) = link.joined.take() {
            // Plugins hear of the join a little later: the client's HUD
            // shows messages that arrive while it is still starting up twice.
            join_event = Some((Box::pin(tokio::time::sleep(JOIN_EVENT_DELAY)), player));
        }
        match flow {
            Flow::Continue => {}
            Flow::Lost => return,
            Flow::Close => {
                // Out of the world and its UUID released before lingering.
                drop(link.membership.take());
                drop(link.login_claim.take());
                linger(connection).await;
                return;
            }
        }
    }
}

/// What the packet loop keeps beyond the session: the player's place in the
/// world, and how they are known to the rest of the server. It carries out
/// what the session's replies ask of the server.
struct Link<'a> {
    server: &'a Server,
    entity_id: u64,
    compression: Option<Compression>,
    outbound: mpsc::Sender<Bytes>,
    controls: mpsc::Sender<Control>,
    /// Set once the player is in the world; dropping it takes them out.
    membership: Option<Membership<'a>>,
    /// Set once the player's identity is verified: the one session for that UUID.
    login_claim: Option<LoginClaim<'a>>,
    /// The player as plugins know them, once they are in the world.
    plugin_player: Option<Player>,
    /// A join plugins should hear of, for the loop to schedule.
    joined: Option<Player>,
}

impl Link<'_> {
    /// What a control from the rest of the server does to the session.
    fn control(&self, session: &mut Session, control: Control) -> Reply {
        let in_world = self.membership.is_some();
        match control {
            // A newer login for this player, or a plugin: this session ends.
            Control::Kick(notice) => Reply::disconnect(notice.reason, notice.message),
            Control::SetGameMode(mode) => session.set_game_mode(mode),
            Control::SetOperator(operator) => {
                let mut reply = session.operator_changed(operator);
                if in_world {
                    reply
                        .packets
                        .push(available_commands(self.server, session).to_vec());
                }
                reply
            }
            Control::RefreshCommands if in_world => {
                Reply::send(vec![available_commands(self.server, session).to_vec()])
            }
            // Players still joining get the commands once they are in.
            Control::RefreshCommands => Reply::default(),
            Control::SetHealth(health) => session.set_health(health),
            Control::Damage { cause, amount } => session.damage(cause, amount),
            Control::GameRules(rules) => session.set_game_rules(rules),
        }
    }

    /// Sends a reply's packets and carries out its events, then the replies
    /// those lead to, in order.
    async fn deliver(
        &mut self,
        connection: &Connection,
        session: &mut Session,
        reply: Reply,
    ) -> Flow {
        let mut pending = VecDeque::from([reply]);
        while let Some(reply) = pending.pop_front() {
            let packets = reply.packets.iter().map(Vec::as_slice);
            if !send(connection, packets, self.compression).await {
                return Flow::Lost;
            }
            if let Some(agreed) = reply.enable_compression {
                self.compression = Some(agreed);
            }
            for event in reply.events {
                if let Some(next) = self.event(session, event).await {
                    pending.push_back(next);
                }
            }
            if reply.close {
                return Flow::Close;
            }
        }
        Flow::Continue
    }

    /// Carries out one event; some lead to another reply.
    async fn event(&mut self, session: &mut Session, event: SessionEvent) -> Option<Reply> {
        let server = self.server;
        match event {
            SessionEvent::Authenticate(token) => {
                let result = server.authenticator.verify(&token).await;
                return Some(session.authenticated(result));
            }
            SessionEvent::LoggedIn(uuid) => {
                // Kicks this player's older session, if any.
                self.login_claim = Some(server.logins.claim(uuid, self.controls.clone()));
                session.set_operator(server.ops.contains(uuid));
            }
            SessionEvent::Joined {
                profile,
                movement,
                view,
                inventory,
                held,
                armor,
            } => {
                let player = Player {
                    name: profile.name.clone(),
                    uuid: profile.uuid.to_string(),
                };
                self.plugin_player = Some(player.clone());
                // Join first, so plugins greeting the player reach them too.
                self.membership = Some(server.players.join(Joining {
                    entity_id: self.entity_id,
                    profile,
                    movement,
                    view,
                    inventory,
                    held,
                    armor: *armor,
                    game_mode: session.game_mode(),
                    health: session.health().value,
                    outbound: self.outbound.clone(),
                }));
                let _ = self.outbound.try_send(available_commands(server, session));
                self.joined = Some(player);
            }
            SessionEvent::Moved(movement) => self.with_membership(|it| it.moved(movement)),
            SessionEvent::Viewing(view) => self.with_membership(|it| it.viewing(view)),
            SessionEvent::BrokeBlock(pos) => {
                if let Some(broken) = server.break_block(pos, session.game_mode())
                    && let Some(player) = &self.plugin_player
                {
                    server
                        .plugins
                        .dispatch(Event::BlockBreak(block_change(server, player, pos, broken)));
                }
            }
            SessionEvent::PlacedBlock {
                pos,
                block,
                replacing,
            } => {
                let placed = match replacing {
                    Some(old) => server.place_block_over(pos, block, old),
                    None => server.place_block(pos, block),
                };
                if placed {
                    if let Some(player) = &self.plugin_player {
                        server
                            .plugins
                            .dispatch(Event::BlockPlace(block_change(server, player, pos, block)));
                    }
                } else {
                    // Someone may have filled the spot since it was checked;
                    // undo the client's prediction.
                    let _ = self
                        .outbound
                        .try_send(server::block_update(pos, server.world.block(pos)));
                }
            }
            SessionEvent::Swing => self.with_membership(|it| it.swing()),
            SessionEvent::Sneaking(sneaking) => {
                self.with_membership(|it| it.sneaking(sneaking));
            }
            SessionEvent::InventoryChanged(inventory) => {
                self.with_membership(|it| it.inventory(inventory));
            }
            SessionEvent::Dropped {
                stacks,
                feet,
                pitch,
                yaw,
            } => {
                server.throw_items(feet, pitch, yaw, &stacks);
                // Others see the throw: the arm swings.
                self.with_membership(|it| it.swing());
            }
            SessionEvent::PickedUp(picked) => server.show_pickups(&picked, self.entity_id),
            SessionEvent::Holding { item, slot } => {
                self.with_membership(|it| it.holding(item, slot));
            }
            SessionEvent::ArmorChanged(armor) => self.with_membership(|it| it.armor(armor)),
            SessionEvent::Equipped(sound) => server.play_sound(session.movement.position, sound),
            SessionEvent::Flying(flying) => self.with_membership(|it| it.flying(flying)),
            SessionEvent::Command { line, origin } => {
                let reply = server.run_command(&session.command_sender(), &line).await;
                return Some(Reply::send(vec![session.command_output(origin, &reply)]));
            }
            SessionEvent::GameModeChanged(mode) => self.with_membership(|it| it.game_mode(mode)),
            SessionEvent::Chat(message) => {
                let cancelled = match &self.plugin_player {
                    Some(player) => {
                        let event = Event::PlayerChat {
                            player: player.clone(),
                            message: message.clone(),
                        };
                        cancelled(&server.plugins, event, "a chat message").await
                    }
                    None => false,
                };
                if cancelled {
                    tracing::info!(target: "chat", "[cancelled] <{}> {message}", session.player());
                } else {
                    tracing::info!(target: "chat", "<{}> {message}", session.player());
                    server.players.chat(session.player(), &message);
                }
            }
            SessionEvent::Damage { cause, amount } => {
                // Plugins may spare the player.
                if let Some(player) = &self.plugin_player {
                    let event = Event::PlayerDamage(Damage {
                        player: player.clone(),
                        cause: cause.name().to_owned(),
                        amount,
                        health: session.health().value,
                    });
                    if cancelled(&server.plugins, event, "damage").await {
                        return None;
                    }
                }
                return Some(session.apply_damage(cause, amount));
            }
            SessionEvent::Hurt => self.with_membership(|it| it.hurt()),
            SessionEvent::HealthChanged(health) => self.with_membership(|it| it.health(health)),
            SessionEvent::Died { cause, drops, feet } => {
                self.with_membership(|it| it.died());
                server.drop_death_items(feet, &drops);
                server.announce_death(session.player(), cause);
                if let Some(player) = &self.plugin_player {
                    server.plugins.dispatch(Event::PlayerDeath {
                        player: player.clone(),
                        cause: cause.name().to_owned(),
                        message: cause.death_message_english(session.player()),
                    });
                }
            }
            SessionEvent::Respawned => {
                let health = session.health().value;
                self.with_membership(|it| it.respawned(health));
                if let Some(player) = &self.plugin_player {
                    server
                        .plugins
                        .dispatch(Event::PlayerRespawn(player.clone()));
                }
            }
        }
        None
    }

    fn with_membership(&self, action: impl FnOnce(&Membership<'_>)) {
        if let Some(membership) = &self.membership {
            action(membership);
        }
    }
}

/// The commands this session's player may run, for their client.
fn available_commands(server: &Server, session: &Session) -> Bytes {
    Bytes::from(
        server
            .commands
            .available_to(&session.command_sender())
            .encode(),
    )
}

/// Asks plugins whether to cancel `event` (`what` names it in the log). What
/// they take too long over goes ahead: a stuck plugin must not stall the game.
async fn cancelled(plugins: &Dispatcher, event: Event, what: &str) -> bool {
    match tokio::time::timeout(VERDICT_TIMEOUT, plugins.dispatch_cancellable(event)).await {
        Ok(cancelled) => cancelled,
        Err(_) => {
            tracing::warn!("plugins took too long to decide on {what}; it goes ahead");
            false
        }
    }
}

/// A block `player` changed at `pos`, as plugins see it.
fn block_change(server: &Server, player: &Player, pos: BlockPos, block: u32) -> BlockChange {
    BlockChange {
        player: player.clone(),
        position: Position {
            x: pos.x,
            y: pos.y,
            z: pos.z,
        },
        block: server.world.block_name(block).to_owned(),
    }
}

/// Sends packets as one reliable batch. Returns whether the connection is still usable.
async fn send<'a>(
    connection: &Connection,
    packets: impl ExactSizeIterator<Item = &'a [u8]>,
    compression: Option<Compression>,
) -> bool {
    if packets.len() == 0 {
        return true;
    }
    match batch::encode(packets, compression) {
        Ok(batch) => connection
            .send(Bytes::from(batch), Reliability::Reliable)
            .await
            .is_ok(),
        Err(err) => {
            tracing::warn!("Couldn't encode packets for a client: {err}");
            false
        }
    }
}

/// Gives the client time to read our last packets and hang up by itself.
async fn linger(connection: &mut Connection) {
    let _ = tokio::time::timeout(DISCONNECT_LINGER, async {
        while connection.recv().await.is_some() {}
    })
    .await;
}
