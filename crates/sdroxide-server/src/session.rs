//! One remote WebSocket session: Hello handshake, sign-in, codec negotiation,
//! three-lane sender, and the command/mic receive loop.
//!
//! A radio may have several of these at once and has exactly one operator. The
//! difference is the control key ([`crate::Control`]): the session holding it
//! may drive the radio, the rest are listening, and everything a listener sends
//! that would reach the hardware is dropped here. That is the only place it can
//! be dropped — past this loop a command is a command, and the engine has no
//! idea which socket it came from.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use sdroxide_proto::{AudioCaps, AudioCodec, ClientMsg, PROTO_VERSION, ServerMsg, decode, encode};
use sdroxide_types::{ClientInfo, Command, ControlStatus};

use crate::auth;
use crate::{Attached, SessionTx, Shared, Station};

/// `/ws` — the station's first radio, which is the whole of what a station
/// with one radio has. Every client that predates the roster arrives here, so
/// this address must never mean anything else.
pub async fn ws_route(State(station): State<Arc<Station>>, upgrade: WebSocketUpgrade) -> Response {
    let shared = station.first();
    upgrade.on_upgrade(|socket| session(socket, shared, station))
}

/// `/ws/<id>` — one named radio out of the station's roster. Unknown ids are
/// refused rather than rounded to a neighbour: a client that asked for the
/// Pluto and silently got the RTL-SDR would be operating the wrong radio.
pub async fn ws_route_for(
    State(station): State<Arc<Station>>,
    Path(id): Path<u32>,
    upgrade: WebSocketUpgrade,
) -> Response {
    match station.radio(id) {
        Some(shared) => upgrade.on_upgrade(|socket| session(socket, shared, station)),
        None => {
            let known: Vec<String> = station.list().iter().map(|r| r.id.to_string()).collect();
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("this station has no radio {id}; it serves {}", known.join(", ")),
            )
                .into_response()
        }
    }
}

fn msg(m: &ServerMsg) -> Message {
    Message::Binary(encode(m).expect("encode").into())
}

async fn session(mut socket: WebSocket, shared: Arc<Shared>, station: Arc<Station>) {
    // Hello and the sign-in first, and only then a place in the registry. The
    // order matters: taking one before knowing who this is would let anyone who
    // can open a socket displace the operator at their own radio without ever
    // proving they may touch it.
    let Some((audio_caps, who)) = handshake(&mut socket, &shared).await else {
        let _ = socket.close().await;
        return;
    };

    run_session(&mut socket, &shared, &station, audio_caps, who).await;
    info!(radio = shared.id, "remote session ended");
}

/// `Hello`, then the sign-in challenge if this server has one. `None` means the
/// socket is finished with — the caller closes it and claims nothing.
///
/// The version check comes first so a client on the wrong protocol is told
/// exactly that, rather than being asked to sign in to a server it could not
/// have talked to anyway.
async fn handshake(
    socket: &mut WebSocket,
    shared: &Arc<Shared>,
) -> Option<(AudioCaps, auth::Identity)> {
    // --- Hello (5 s budget) -------------------------------------------
    let hello = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    let audio_caps = match hello {
        Ok(Some(Ok(Message::Binary(bytes)))) => match decode::<ClientMsg>(&bytes) {
            Ok(ClientMsg::Hello { proto, audio }) if proto == PROTO_VERSION => audio,
            Ok(ClientMsg::Hello { proto, .. }) => {
                let _ = socket
                    .send(msg(&ServerMsg::Error(format!(
                        "protocol mismatch: server {PROTO_VERSION}, client {proto}"
                    ))))
                    .await;
                return None;
            }
            _ => {
                let _ = socket.send(msg(&ServerMsg::Error("expected Hello".into()))).await;
                return None;
            }
        },
        _ => return None,
    };

    // --- Sign-in ------------------------------------------------------
    let who = auth::challenge(
        socket,
        &shared.auth,
        auth::Frames {
            what: "/ws",
            required: encode(&ServerMsg::AuthRequired).expect("encode"),
            rejected: &|why| encode(&ServerMsg::AuthRejected(why.into())).expect("encode"),
            credentials: &|bytes| match decode::<ClientMsg>(bytes) {
                Ok(ClientMsg::Auth { username, password }) => Some((username, password)),
                _ => None,
            },
        },
    )
    .await?;
    Some((audio_caps, who))
}

// --- the control key ---------------------------------------------------------

/// What the clients on a radio are, for the status every one of them is sent.
fn who_is_here(shared: &Shared) -> Vec<ClientInfo> {
    shared
        .clients
        .lock()
        .unwrap()
        .iter()
        .map(|c| ClientInfo {
            slot: c.slot,
            name: c.who.name.clone(),
            may_transmit: c.who.may_transmit,
        })
        .collect()
}

/// Send every client on this radio the state of the sharing, each addressed to
/// itself.
///
/// One message per client rather than one broadcast, because each has to be told
/// which of the listed clients it is — `me`. Everything else in it is the same
/// for everybody.
fn announce_control(shared: &Shared) {
    let clients = who_is_here(shared);
    let (holder, waiting) = {
        let control = shared.control.lock().unwrap();
        let find = |slot: u64| clients.iter().find(|c| c.slot == slot).cloned();
        let waiting: Vec<ClientInfo> = control.waiting.iter().filter_map(|s| find(*s)).collect();
        (control.holder.and_then(find), waiting)
    };
    for c in clients.iter() {
        shared.tell(
            c.slot,
            ServerMsg::Control(ControlStatus {
                me: c.slot,
                holder: holder.clone(),
                clients: clients.clone(),
                waiting: waiting.clone(),
            }),
        );
    }
}

/// Drop every session already signed in as `who`, so a named operator who
/// reconnects — laptop off the cable and onto Wi-Fi, browser tab restored —
/// is not left listening to themselves.
///
/// Only a roster identity does this. A station with one shared password cannot
/// tell two people apart, and kicking the first of them would be the old
/// single-client lockout in a new shape.
fn drop_stale_sessions_of(shared: &Shared, who: &auth::Identity) {
    if !who.named || who.name.is_empty() {
        return;
    }
    let gone: Vec<u64> = {
        let mut clients = shared.clients.lock().unwrap();
        let mut gone = Vec::new();
        clients.retain(|c| {
            if c.who.named && c.who.name.eq_ignore_ascii_case(&who.name) {
                gone.push(c.slot);
                false
            } else {
                true
            }
        });
        gone
    };
    if gone.is_empty() {
        return;
    }
    info!(
        radio = shared.id,
        who = who.label(),
        previous = gone.len(),
        "the same operator is back; the previous session is dropped"
    );
    for slot in gone {
        release_control(shared, slot);
    }
}

/// Whether `slot` is the client working this radio.
fn holds_control(shared: &Shared, slot: u64) -> bool {
    shared.control.lock().unwrap().holder == Some(slot)
}

/// Give the control key to `slot`, releasing whoever had it.
///
/// The radio is put down on the way through. Whoever was holding the key may
/// have been holding the transmitter with it, and they are no longer the client
/// that can let go of it — see [`Shared::safe_state`].
fn give_control_to(shared: &Shared, slot: u64) {
    {
        let mut control = shared.control.lock().unwrap();
        if control.holder == Some(slot) {
            return;
        }
        control.holder = Some(slot);
        control.waiting.retain(|s| *s != slot);
    }
    shared.safe_state();
    announce_control(shared);
    apply_operator_identity(shared, slot);
}

/// Put this operator's callsign, grid and network credentials onto the radio.
///
/// A named holder is who goes on the air, not whoever last wrote `digi.json`.
/// A listener's file is left alone; a name that has never saved a `net.json`
/// does not overwrite the station's feeds with empty defaults.
fn apply_operator_identity(shared: &Shared, slot: u64) {
    let Some(roster) = shared.station.upgrade() else { return };
    let name = {
        let clients = shared.clients.lock().unwrap();
        let Some(c) = clients.iter().find(|c| c.slot == slot) else { return };
        if !c.who.named {
            return;
        }
        c.who.name.clone()
    };
    if let Some(load) = roster.load_user.as_ref() {
        let settings = load(&name);
        let call = if settings.my_call.trim().is_empty() {
            name.to_uppercase()
        } else {
            settings.my_call
        };
        if let Some(d) = shared.latest.lock().unwrap().digi.clone() {
            let mut cfg = d.config;
            cfg.my_call = call;
            if !settings.my_grid.trim().is_empty() {
                cfg.my_grid = settings.my_grid;
            }
            let _ = shared.cmd_tx.send(Command::SetDigiConfig(cfg));
        }
    }
    if let Some(load) = roster.load_user_network.as_ref() {
        if let Some(net) = load(&name) {
            let _ = shared.cmd_tx.send(Command::SetNetworkConfig(net));
        }
    }
}

/// Keep this name's callsign and grid in their settings file, without
/// touching volume or band stacks.
fn remember_operator_call(roster: &Station, name: &str, call: &str, grid: &str) {
    let Some(load) = roster.load_user.as_ref() else { return };
    let Some(save) = roster.save_user.as_ref() else { return };
    let mut settings = load(name);
    if settings.my_call == call && settings.my_grid == grid {
        return;
    }
    settings.my_call = call.to_string();
    settings.my_grid = grid.to_string();
    if let Err(e) = save(name, &settings) {
        warn!(who = name, "could not store operator identity: {e}");
    }
}

/// A copy of the station bundle this client is allowed to see: passwords and
/// API keys stay with the named operator they belong to.
fn station_config_for(
    who: &auth::Identity,
    mut cfg: Box<sdroxide_types::StationConfig>,
) -> Box<sdroxide_types::StationConfig> {
    if who.named {
        cfg.net = cfg.net.without_secrets();
    }
    cfg
}

/// Take the control key off `slot`, if it has it, and offer it to whoever has
/// been waiting longest.
///
/// Called when the holder gives it up and when their socket goes away. Nobody
/// waiting leaves the radio unattended, which is the state a station spends most
/// of its time in and the one where the next client to attach simply gets the
/// key.
fn release_control(shared: &Shared, slot: u64) {
    // Read before the control lock is taken, never under it: the two locks are
    // always taken clients-first, and holding this one across the other is the
    // one way this deadlocks. See [`Shared::clients`].
    let here: Vec<u64> = shared.clients.lock().unwrap().iter().map(|c| c.slot).collect();
    let next = {
        let mut control = shared.control.lock().unwrap();
        if control.holder != Some(slot) {
            // Not the holder: only their own place in the queue goes.
            control.waiting.retain(|s| *s != slot);
            drop(control);
            announce_control(shared);
            return;
        }
        control.holder = None;
        control.waiting.retain(|s| *s != slot);
        // Offered rather than granted only if somebody is actually still here to
        // take it: a queue of clients that have since disconnected must not leave
        // the radio holding a key for nobody.
        control.waiting.iter().find(|s| here.contains(s)).copied()
    };
    shared.safe_state();
    match next {
        Some(next) => {
            info!(radio = shared.id, "the control key passes to the next client waiting");
            give_control_to(shared, next);
        }
        None => announce_control(shared),
    }
}

/// Take a request for the control key.
///
/// A radio nobody is working hands it over at once — there is nobody to ask, and
/// making an operator wait for a permission that cannot be given would be a dead
/// button. Otherwise the request goes on the queue and the holder is told, which
/// is what [`announce_control`] carries.
fn ask_for_control(shared: &Shared, slot: u64) {
    let free = {
        let mut control = shared.control.lock().unwrap();
        if control.holder == Some(slot) {
            return;
        }
        if control.holder.is_none() {
            true
        } else {
            if !control.waiting.contains(&slot) {
                control.waiting.push(slot);
            }
            false
        }
    };
    if free {
        info!(radio = shared.id, "the radio was free; the control key goes to whoever asked");
        give_control_to(shared, slot);
    } else {
        announce_control(shared);
    }
}

/// Turn a request down. The asker is told by the status losing their entry, and
/// a notice, because a queue place quietly vanishing is indistinguishable from
/// the request never having arrived.
fn deny_control(shared: &Shared, holder: u64, asked: u64) {
    if !holds_control(shared, holder) {
        return;
    }
    let was_waiting = {
        let mut control = shared.control.lock().unwrap();
        let before = control.waiting.len();
        control.waiting.retain(|s| *s != asked);
        control.waiting.len() != before
    };
    if was_waiting {
        shared.tell(
            asked,
            ServerMsg::Notice(Some("the operator is not handing the radio over".into())),
        );
        announce_control(shared);
    }
}

/// Whether a command from this session reaches the engine, and why not.
///
/// Two gates, and the second is not implied by the first. A listener may not
/// touch the radio at all. An operator who holds the key but whose roster entry
/// says `may_transmit = false` may work the receiver and is refused the three
/// commands that put a signal on the air — checked by name rather than by
/// trusting the client not to send them, because the client is the half of this
/// that anybody can rewrite.
fn refuse_reason(
    shared: &Shared,
    slot: u64,
    who: &auth::Identity,
    cmd: &Command,
) -> Option<&'static str> {
    if !holds_control(shared, slot) {
        return Some("you are listening to this radio, not working it");
    }
    let keys_the_transmitter =
        matches!(cmd, Command::SetPtt(true) | Command::SetTune(true) | Command::CwKey(true));
    if keys_the_transmitter && !who.may_transmit {
        return Some("this sign-in may not transmit");
    }
    None
}

/// Tell the client that asked why a roster edit did not happen.
///
/// A notice rather than a [`ServerMsg::Error`]: the client reads `Error` as the
/// session being over, and a refused edit — the first radio, a name for a radio
/// that has just gone — leaves a perfectly good connection standing. Nothing is
/// sent when it worked: what happened is the new roster, which every session
/// has already been given.
fn report<T>(
    shared: &Shared,
    // Who asked, so the answer goes to them rather than to everybody on the
    // radio: a refusal is about the button somebody just pressed, and the others
    // never pressed it.
    slot: u64,
    outcome: Result<Result<T, String>, tokio::task::JoinError>,
    what: &str,
) {
    let why = match outcome {
        Ok(Ok(_)) => return,
        Ok(Err(e)) => e,
        // The blocking task panicked or was cancelled. The client is still
        // owed an answer, or its button would simply have done nothing.
        Err(e) => format!("the station could not answer ({e})"),
    };
    warn!(radio = shared.id, "{what}: {why}");
    shared.tell(slot, ServerMsg::Notice(Some(format!("{what}: {why}"))));
}

async fn run_session(
    socket: &mut WebSocket,
    shared: &Arc<Shared>,
    // The station this radio belongs to, for the roster it announces. Named
    // apart from the `station` below, which is the *config* of the station.
    roster: &Arc<Station>,
    audio_caps: AudioCaps,
    who: auth::Identity,
) {
    let rx_codec =
        if audio_caps.opus_decode { AudioCodec::Opus48kMono } else { AudioCodec::Pcm16_48k };
    let tx_codec =
        if audio_caps.opus_encode { AudioCodec::Opus48kMono } else { AudioCodec::Pcm16_48k };

    let (
        caps,
        state,
        memories,
        mem_folders,
        scanner,
        digi,
        voice,
        images,
        notice,
        station,
        tle_subs,
        sat_track,
        radio,
        rds,
        ism_reports,
        ism_status,
        adsb_status,
        vdl2_status,
        ais_status,
        drm,
        hd,
        relay,
        profiles,
    ) = {
        let latest = shared.latest.lock().unwrap();
        (
            latest.caps.clone(),
            latest.state.clone(),
            latest.memories.clone(),
            latest.mem_folders.clone(),
            latest.scanner.clone(),
            latest.digi.clone(),
            latest.voice.clone(),
            latest.images.clone(),
            latest.notice.clone(),
            latest.station.clone(),
            latest.tle_subs.clone(),
            latest.sat_track.clone(),
            latest.radio.clone(),
            latest.rds.clone(),
            latest.ism_reports.clone(),
            latest.ism_status.clone(),
            latest.adsb_status.clone(),
            latest.vdl2_status.clone(),
            latest.ais_status.clone(),
            latest.drm.clone(),
            latest.hd.clone(),
            latest.relay.clone(),
            latest.profiles.clone(),
        )
    };
    let ack = ServerMsg::HelloAck { proto: PROTO_VERSION, caps, state, rx_codec, tx_codec };
    if socket.send(msg(&ack)).await.is_err() {
        return;
    }
    // Which radio this is, and what else the station has. Straight after the
    // acknowledgement because a client that can hold several radios opens the
    // rest from it, and it should do that before the operator has finished
    // looking at the first one.
    let _ = socket
        .send(msg(&ServerMsg::Radios {
            me: shared.id,
            radios: roster.roster(),
            editable: roster.editable(),
        }))
        .await;
    let _ = socket.send(msg(&ServerMsg::Memories(memories))).await;
    let _ = socket.send(msg(&ServerMsg::MemoryFolders(mem_folders))).await;
    let _ = socket.send(msg(&ServerMsg::Scanner(scanner))).await;
    let _ = socket.send(msg(&ServerMsg::Profiles(profiles))).await;
    // This operator's own settings, where the station knows who they are. A
    // shared-password station cannot tell its clients apart and has nowhere to
    // keep a per-operator file, so nothing is sent and the client keeps what
    // it already had.
    if who.named {
        if let Some(load) = roster.load_user.as_ref() {
            let mut settings = load(&who.name);
            if settings.my_call.trim().is_empty() {
                settings.my_call = who.name.to_uppercase();
            }
            let _ = socket.send(msg(&ServerMsg::UserSettings(settings))).await;
        }
        if let Some(load) = roster.load_user_network.as_ref() {
            if let Some(net) = load(&who.name) {
                let _ = socket.send(msg(&ServerMsg::UserNetwork(net))).await;
            }
        }
        if let Some(load) = roster.load_user_qso.as_ref() {
            let log = load(&who.name);
            let _ = socket.send(msg(&ServerMsg::UserQsoLog(log))).await;
        }
    }
    // The operator config, which the engine announced once at startup. Without
    // this replay the client's callsign and grid come up empty and greyed out.
    if let Some(d) = digi {
        let _ = socket.send(msg(&ServerMsg::Ft8Status(d))).await;
    }
    // Likewise the voice keyer's slots, announced once at engine start.
    if let Some(v) = voice {
        let _ = socket.send(msg(&ServerMsg::VoiceStatus(v))).await;
    }
    // The station the radio is sitting on, if it is a WFM broadcast carrying
    // RDS. A condition rather than an event: the name and programme type may
    // have arrived minutes ago and will not be sent again until they change.
    if let Some(d) = rds {
        let _ = socket.send(msg(&ServerMsg::Rds(d))).await;
    }
    // The DRM broadcast being decoded, for the same reason as the RDS station
    // above: sync and a service label are conditions, not events.
    if let Some(d) = drm {
        let _ = socket.send(msg(&ServerMsg::Drm(d))).await;
    }
    // The HD Radio broadcast being decoded, for the same reason again.
    if let Some(d) = hd {
        let _ = socket.send(msg(&ServerMsg::Hd(d))).await;
    }
    // The ISM device table and where the decoder is listening. Both are slow
    // conditions — see `Latest::ism_reports`.
    if let Some(st) = ism_status {
        let _ = socket.send(msg(&ServerMsg::IsmStatus(st))).await;
    }
    if !ism_reports.is_empty() {
        let _ = socket.send(msg(&ServerMsg::IsmReports(ism_reports))).await;
    }
    // The aircraft table, for the same reason: what is overhead is a condition.
    if let Some(st) = adsb_status {
        let _ = socket.send(msg(&ServerMsg::AdsbStatus(st))).await;
    }
    // ...and the VDL2 log, for the same reason: what has been said is a record,
    // not an event that has already happened to somebody else.
    if let Some(st) = vdl2_status {
        let _ = socket.send(msg(&ServerMsg::Vdl2Status(st))).await;
    }
    // ...and the vessel table, most of all: at three minutes between reports
    // from a ship at anchor, a client that had to wait for one would sit in
    // front of a blank chart long enough to conclude the decoder was broken.
    if let Some(st) = ais_status {
        let _ = socket.send(msg(&ServerMsg::AisStatus(st))).await;
    }
    // And the transmit-image presets, for the same reason. The received
    // galleries are not replayed: a panel lists its store when it opens, which
    // is both authoritative and the only view that can be paged.
    if let Some(p) = images {
        let _ = socket.send(msg(&ServerMsg::ImagePresets(p))).await;
    }
    // What the station is set up to do, announced at engine start like the
    // operator config. Without this replay the settings dialog here shows
    // defaults for every server-side tab — and applying them would write those
    // defaults over the operator's real configuration.
    if let Some(s) = station {
        let _ = socket.send(msg(&ServerMsg::StationConfig(station_config_for(&who, s)))).await;
        let _ = socket.send(msg(&ServerMsg::TleSubStatus(tle_subs))).await;
    }
    // And which interface this machine has open, with every backend's settings.
    // Same reason again, and the same failure without it: the Radio tab would
    // come up on defaults, and the first thing touched there would write a
    // default sample rate and an empty device selection over the operator's.
    if let Some(r) = radio {
        let _ = socket.send(msg(&ServerMsg::RadioConfig(r))).await;
    }
    // The satellite lock is a condition too: a client attaching mid-pass has
    // to see it immediately, and must not offer to start one that is running.
    if sat_track.is_some() {
        let _ = socket.send(msg(&ServerMsg::SatTrack(sat_track))).await;
    }
    // And the T/R switch, for the strongest version of the same reason: a relay
    // that is not answering is the one standing condition here that a client
    // needs to know about *before* it touches the PTT button.
    if let Some(r) = relay {
        let _ = socket.send(msg(&ServerMsg::RelayStatus(r))).await;
    }
    // A standing condition rather than an event: whoever attaches next has to
    // know the radio is refusing tunes or reconnecting, not just whoever
    // happened to be connected when it started.
    if notice.is_some() {
        let _ = socket.send(msg(&ServerMsg::Notice(notice))).await;
    }
    // --- register lanes -----------------------------------------------
    let (rel_tx, mut rel_rx) = mpsc::channel::<ServerMsg>(256);
    let (aud_tx, mut aud_rx) = mpsc::channel::<ServerMsg>(8);
    let me = Attached {
        slot: shared.next_slot.fetch_add(1, Ordering::Relaxed),
        who: who.clone(),
        tx: SessionTx { reliable: rel_tx, audio: aud_tx, rx_codec },
    };
    let slot = me.slot;
    drop_stale_sessions_of(shared, &who);
    // A radio nobody is working is handed to whoever attaches to it, so a
    // single-operator station behaves exactly as it always did: connect, and the
    // radio is yours. Only a second client has to ask.
    let take_control = {
        // Both locks, in the order everything here takes them, and the key
        // claimed under the same guard that reads it free: two clients arriving
        // together must not both conclude the radio was unattended.
        let mut clients = shared.clients.lock().unwrap();
        clients.push(me);
        let mut control = shared.control.lock().unwrap();
        let free = control.holder.is_none();
        if free {
            control.holder = Some(slot);
        }
        free
    };
    info!(
        radio = shared.id,
        who = who.label(),
        ?rx_codec,
        ?tx_codec,
        listening = !take_control,
        "remote client connected"
    );
    // After the lanes are registered either way, so the client is told how the
    // radio is being shared on its own socket rather than having to ask.
    announce_control(shared);
    if take_control {
        apply_operator_identity(shared, slot);
    }

    let (mut ws_tx, mut ws_rx) = futures_util::StreamExt::split(socket);

    // Sender: reliable first, then audio, then latest spectrum.
    let mut spectrum_rx = shared.spectrum_rx.clone();
    let mut wide_rx = shared.wide_spectrum_rx.clone();
    let sender = async {
        let mut last_spectrum_seq = 0u32;
        let mut last_wide_seq = 0u32;
        loop {
            tokio::select! {
                biased;
                m = rel_rx.recv() => {
                    let Some(m) = m else { break };
                    if ws_tx.send(msg(&m)).await.is_err() { break; }
                }
                m = aud_rx.recv() => {
                    let Some(m) = m else { break };
                    if ws_tx.send(msg(&m)).await.is_err() { break; }
                }
                changed = wide_rx.changed() => {
                    if changed.is_err() { break; }
                    let frame = wide_rx.borrow_and_update().clone();
                    if let Some(f) = frame {
                        if f.seq != last_wide_seq {
                            last_wide_seq = f.seq;
                            if ws_tx.send(msg(&ServerMsg::WideSpectrum(f))).await.is_err() { break; }
                        }
                    }
                }
                changed = spectrum_rx.changed() => {
                    if changed.is_err() { break; }
                    let frame = spectrum_rx.borrow_and_update().clone();
                    if let Some(f) = frame {
                        if f.seq != last_spectrum_seq {
                            last_spectrum_seq = f.seq;
                            if ws_tx.send(msg(&ServerMsg::Spectrum(f))).await.is_err() { break; }
                        }
                    }
                }
            }
        }
    };

    // Receiver: commands, mic frames, pings.
    let receiver = async {
        let mut opus_dec: Option<opus::Decoder> = None;
        let mut pcm = vec![0.0f32; 5760];
        while let Some(Ok(m)) = ws_rx.next().await {
            let Message::Binary(bytes) = m else {
                if matches!(m, Message::Close(_)) {
                    break;
                }
                continue;
            };
            match decode::<ClientMsg>(&bytes) {
                Ok(ClientMsg::Command(cmd)) => {
                    // A named operator's callsign, grid and network credentials
                    // are theirs even while they are only listening: saving them
                    // must not wait for the control key, and must not reach the
                    // engine unless they hold it.
                    let skip_engine = who.named
                        && match &cmd {
                            Command::SetNetworkConfig(cfg) => {
                                if let Some(save) = roster.save_user_network.as_ref() {
                                    if let Err(e) = save(&who.name, cfg) {
                                        warn!(
                                            radio = shared.id,
                                            who = who.label(),
                                            "could not store operator network config: {e}"
                                        );
                                    }
                                }
                                !holds_control(shared, slot)
                            }
                            Command::SetDigiConfig(cfg) => {
                                remember_operator_call(
                                    roster,
                                    &who.name,
                                    &cfg.my_call,
                                    &cfg.my_grid,
                                );
                                !holds_control(shared, slot)
                            }
                            _ => false,
                        };
                    if skip_engine {
                        continue;
                    }
                    // The one gate between a listening client and the hardware.
                    // Past this line a command is a command and the engine has
                    // no idea which socket it arrived on, so nothing further
                    // down can make this decision — see [`refuse_reason`].
                    match refuse_reason(shared, slot, &who, &cmd) {
                        None => {
                            let _ = shared.cmd_tx.send(cmd);
                        }
                        Some(why) => {
                            // Rate-limited by being sent only for a command that
                            // would have keyed something or changed the radio; a
                            // client that has been told it is listening does not
                            // send these, and one that does is either stale or
                            // being driven by hand.
                            warn!(radio = shared.id, who = who.label(), "refused a command: {why}");
                            shared.tell(slot, ServerMsg::Notice(Some(why.into())));
                        }
                    }
                }
                // Nobody's microphone but the operator's reaches the transmitter.
                // A listener's frames are dropped without a word: a browser tab
                // holds the microphone open for as long as the page has it, and
                // saying so for every 20 ms frame would be fifty log lines a
                // second.
                Ok(ClientMsg::MicFrame { .. })
                    if !holds_control(shared, slot) || !who.may_transmit => {}
                Ok(ClientMsg::MicFrame { payload, .. }) => {
                    let n = match tx_codec {
                        AudioCodec::Opus48kMono => {
                            let dec = opus_dec.get_or_insert_with(|| {
                                opus::Decoder::new(48_000, opus::Channels::Mono)
                                    .expect("opus decoder")
                            });
                            match dec.decode_float(&payload, &mut pcm, false) {
                                Ok(n) => n,
                                Err(e) => {
                                    warn!("opus decode: {e}");
                                    continue;
                                }
                            }
                        }
                        AudioCodec::Pcm16_48k => {
                            let mut n = 0;
                            for (i, c) in payload.chunks_exact(2).enumerate() {
                                if i >= pcm.len() {
                                    break;
                                }
                                pcm[i] = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
                                n += 1;
                            }
                            n
                        }
                    };
                    let mut mic = shared.mic_tx.lock().unwrap();
                    for &s in &pcm[..n] {
                        if mic.push(s).is_err() {
                            break; // ring full — engine will catch up
                        }
                    }
                }
                // Not into the engine: a bus scan or a connection test blocks
                // for as long as the hardware takes, and the engine thread is
                // the one carrying the radio. Its own worker answers, in the
                // order these arrive — see `crate::probe`.
                Ok(ClientMsg::Probe(req)) => crate::probe::ask(shared, req),
                // The station's roster, not this radio's engine. Answered here
                // rather than forwarded: a command goes to *a* radio, and how
                // many radios there are belongs to the station.
                //
                // Awaited in the receive loop, so roster edits are taken one at
                // a time in the order they arrive — the same rule as probes,
                // and for a stronger reason: adding a radio starts an engine,
                // and two of them racing would both read the same `next_id`.
                Ok(ClientMsg::AddRadio { name }) => {
                    let station = roster.clone();
                    // On a blocking thread: it creates the radio's
                    // configuration scope and starts an engine, neither of
                    // which belongs on the socket task.
                    let done = tokio::task::spawn_blocking(move || station.add_radio(&name)).await;
                    report(shared, slot, done, "adding a radio");
                }
                Ok(ClientMsg::RemoveRadio { id }) => {
                    let station = roster.clone();
                    let done = tokio::task::spawn_blocking(move || station.remove_radio(id)).await;
                    report(shared, slot, done, "closing a radio");
                }
                Ok(ClientMsg::RenameRadio { id, name }) => {
                    let station = roster.clone();
                    let done =
                        tokio::task::spawn_blocking(move || station.rename_radio(id, &name)).await;
                    report(shared, slot, done, "renaming a radio");
                }
                // Also the station's business, not this radio's engine: it is
                // the roster that says whether a radio has an interface at all.
                // Blocking for the same reason as the rest — it writes the
                // host's roster file.
                Ok(ClientMsg::SetRadioEnabled { id, on }) => {
                    let station = roster.clone();
                    let done =
                        tokio::task::spawn_blocking(move || station.set_radio_power(id, on)).await;
                    report(shared, slot, done, "switching a radio");
                }
                Ok(ClientMsg::Ping(t)) => shared.tell(slot, ServerMsg::Pong(t)),
                // The control key. Handled here rather than forwarded anywhere:
                // who is working the radio is a fact about the station's clients,
                // and the engine neither knows nor needs to.
                Ok(ClientMsg::RequestControl) => ask_for_control(shared, slot),
                Ok(ClientMsg::ReleaseControl) => release_control(shared, slot),
                Ok(ClientMsg::GrantControl { to }) => {
                    // Only from the holder, and only to a client that is still
                    // here and still asking. A grant that has been overtaken by
                    // the asker disconnecting is dropped rather than applied to
                    // whoever took their place in the queue.
                    let asking = holds_control(shared, slot)
                        && shared.control.lock().unwrap().waiting.contains(&to);
                    if asking {
                        info!(radio = shared.id, "the operator hands the radio over");
                        give_control_to(shared, to);
                    }
                }
                Ok(ClientMsg::DenyControl { to }) => deny_control(shared, slot, to),
                Ok(ClientMsg::SetUserSettings(settings)) => {
                    if who.named {
                        if let Some(save) = roster.save_user.as_ref() {
                            if let Err(e) = save(&who.name, &settings) {
                                warn!(
                                    radio = shared.id,
                                    who = who.label(),
                                    "could not store operator settings: {e}"
                                );
                            }
                        }
                    }
                }
                Ok(ClientMsg::SetUserQsoLog(log)) => {
                    if who.named {
                        if let Some(save) = roster.save_user_qso.as_ref() {
                            if let Err(e) = save(&who.name, &log) {
                                warn!(
                                    radio = shared.id,
                                    who = who.label(),
                                    "could not store operator logbook: {e}"
                                );
                            }
                        }
                    }
                }
                Ok(ClientMsg::Hello { .. }) => {} // ignore late Hello
                // Likewise a late `Auth`: this socket is already signed in, so
                // there is nothing to re-check, and running it through the gate
                // would let an established client lock everybody else's sign-in
                // out for three seconds at a time.
                Ok(ClientMsg::Auth { .. }) => {}
                Err(e) => warn!("bad client message: {e}"),
            }
        }
    };

    tokio::select! {
        _ = sender => {}
        _ = receiver => {}
    }

    // Whatever happened, this client is gone: out of the registry, out of the
    // queue, and — if it was the one working the radio — the key offered to
    // whoever has been waiting longest. `release_control` puts the radio down on
    // the way through, which is the part that matters: a socket that died with
    // the PTT held would otherwise leave the transmitter on.
    shared.clients.lock().unwrap().retain(|c| c.slot != slot);
    release_control(shared, slot);
}
