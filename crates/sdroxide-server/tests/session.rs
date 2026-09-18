//! End-to-end server test: real engine (signal generator) + real WebSocket
//! client. Covers the handshake, sign-in, state echo, spectrum/audio streaming,
//! and several clients sharing one radio.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use sdroxide_proto::{AudioCaps, ClientMsg, PROTO_VERSION, ServerMsg, decode, encode};
use sdroxide_radio::{AudioParams, EngineConfig, MicParams, SigGenSource, start_engine};
use sdroxide_server::{
    Access, AccessFn, LoadUserSettingsFn, RadioParams, SaveUserSettingsFn, ServerParams, serve,
};
use sdroxide_types::{Command, DeviceCaps, RemoteAccess, User, UserSettings, Users, Vfo};

const PORT: u16 = 39471;
/// The sign-in test's own engine, so the two tests cannot disturb each other's
/// turnstile — it is deliberately server-wide state.
const AUTH_PORT: u16 = 39472;
/// Sharing / handover, on its own engine so the two tests cannot share a slot
/// counter or a control key.
const SHARE_PORT: u16 = 39473;
/// Named-operator settings, on its own engine for the same reason.
const SETTINGS_PORT: u16 = 39474;
/// Same named operator reconnecting, on its own engine so it cannot share a
/// control key with the handover test.
const RECONNECT_PORT: u16 = 39475;

async fn recv_msg(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> ServerMsg {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("timeout waiting for server message")
            .expect("stream ended")
            .expect("ws error");
        if let Message::Binary(bytes) = m {
            return decode::<ServerMsg>(&bytes).expect("decode");
        }
    }
}

/// An engine on a signal generator, served on `port`.
async fn spawn_server(port: u16, access: Option<AccessFn>) {
    spawn_server_with(port, access, None, None).await;
}

async fn spawn_server_with(
    port: u16,
    access: Option<AccessFn>,
    load_user_settings: Option<LoadUserSettingsFn>,
    save_user_settings: Option<SaveUserSettingsFn>,
) {
    let (audio_producer, audio_consumer) = sdroxide_radio::rtrb::RingBuffer::<f32>::new(96_000);
    let (mic_producer, mic_consumer) = sdroxide_radio::rtrb::RingBuffer::<f32>::new(48_000);
    let source = SigGenSource::demo(1_536_000.0, 14_200_000.0);
    let caps = DeviceCaps {
        driver: "siggen".into(),
        label: "Test signal generator".into(),
        rx_channels: 1,
        freq_ranges_rx: vec![(0.0, 6e9)],
        ..DeviceCaps::default()
    };
    let handles = start_engine(
        Box::new(source),
        caps,
        EngineConfig {
            audio: Some(AudioParams { producer: audio_producer, out_rate: 48_000.0 }),
            mic: Some(MicParams { consumer: mic_consumer, rate: 48_000.0 }),
            ..Default::default()
        },
    );

    tokio::spawn(serve(ServerParams {
        radios: vec![RadioParams {
            id: 0,
            name: String::new(),
            cmd_tx: handles.cmd_tx,
            event_rx: handles.event_rx,
            spectrum_out: handles.spectrum_out,
            wide_spectrum_out: handles.wide_spectrum_out,
            audio_rx: audio_consumer,
            mic_tx: mic_producer,
        }],
        bind: "127.0.0.1".into(),
        port,
        web_root: None,
        access,
        // These tests are about the session, not about this machine's buses:
        // a prober that answers from a table keeps them off whatever hardware
        // the test runner happens to have, while still exercising the lane.
        probe: None,
        add_radio: None,
        remove_radio: None,
        rename_radio: None,
        radio_power: None,
        load_user_settings,
        save_user_settings,
    }));
    tokio::time::sleep(Duration::from_millis(400)).await;
}

fn hello() -> ClientMsg {
    ClientMsg::Hello {
        proto: PROTO_VERSION,
        audio: AudioCaps { opus_decode: false, opus_encode: false },
    }
}

async fn send(
    ws: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    m: &ClientMsg,
) {
    ws.send(Message::Binary(encode(m).unwrap().into())).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn full_session_flow() {
    // No credentials configured: the server is open, exactly as it was before
    // sign-in existed.
    spawn_server(PORT, None).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{PORT}/ws"))
        .await
        .expect("connect");

    // Hello → HelloAck with the device caps, PCM16 negotiated.
    send(&mut ws, &hello()).await;
    match recv_msg(&mut ws).await {
        ServerMsg::HelloAck { proto, caps, state, rx_codec, .. } => {
            assert_eq!(proto, PROTO_VERSION);
            assert_eq!(caps.label, "Test signal generator");
            assert!(state.sample_rate > 0.0);
            assert_eq!(rx_codec, sdroxide_proto::AudioCodec::Pcm16_48k);
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }

    // Streams flow: within a few seconds we must see spectrum AND audio. The
    // station config has to arrive too — the engine announces it once, long
    // before anybody connects, so it only reaches a client if the server kept
    // it and replayed it. Without that the settings dialog here shows every
    // server-side tab as unconfigured.
    let (mut got_spectrum, mut got_audio, mut got_station) = (false, false, false);
    while !(got_spectrum && got_audio && got_station) {
        match recv_msg(&mut ws).await {
            ServerMsg::Spectrum(f) => {
                assert!(!f.bins.is_empty());
                assert!(f.span_hz > 0.0);
                got_spectrum = true;
            }
            ServerMsg::RxAudio { payload, .. } => {
                assert_eq!(payload.len(), 1920, "20 ms of PCM16");
                got_audio = true;
            }
            ServerMsg::StationConfig(_) => got_station = true,
            _ => {}
        }
    }

    // Command → state echo.
    let cmd = ClientMsg::Command(Command::SetVfo { vfo: Vfo::A, hz: 14_100_000.0 });
    send(&mut ws, &cmd).await;
    loop {
        if let ServerMsg::State(s) = recv_msg(&mut ws).await {
            if (s.vfo_a_hz - 14_100_000.0).abs() < 1.0 {
                break;
            }
        }
    }

    // Second client is let in as a listener: the radio is shared, not Busy.
    let (mut ws2, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{PORT}/ws"))
        .await
        .expect("connect 2");
    send(&mut ws2, &hello()).await;
    match recv_msg(&mut ws2).await {
        ServerMsg::HelloAck { .. } => {}
        other => panic!("expected HelloAck for the second client, got {other:?}"),
    }
    let status = wait_for_control(&mut ws2).await;
    assert!(status.shared(), "two clients on one radio is a shared radio");
    assert!(!status.i_hold(), "the second client is listening");

    // First client drops; the listener is offered the key.
    drop(ws);
    drop(ws2);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (mut ws3, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{PORT}/ws"))
        .await
        .expect("reconnect");
    send(&mut ws3, &hello()).await;
    match recv_msg(&mut ws3).await {
        ServerMsg::HelloAck { .. } => {}
        other => panic!("expected HelloAck on reconnect, got {other:?}"),
    }
}

/// The sign-in, over a real socket: what a wrong password costs, and the two
/// things a client that has not given one must not be able to do — take the
/// single-client slot, or reach the radio.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_with_credentials_signs_clients_in() {
    spawn_server(
        AUTH_PORT,
        Some(Box::new(|| Access {
            shared: RemoteAccess { username: "oe1test".into(), password: "hunter2".into() },
            users: Default::default(),
        })),
    )
    .await;
    let url = format!("ws://127.0.0.1:{AUTH_PORT}/ws");

    // Hello is answered with the challenge, not with the radio.
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");
    send(&mut ws, &hello()).await;
    assert_eq!(recv_msg(&mut ws).await, ServerMsg::AuthRequired);

    // A command sent instead of credentials must not reach the engine. Nothing
    // comes back for it — the socket simply stays in the challenge — so the
    // proof is that the *next* thing the server says is still a rejection.
    send(&mut ws, &ClientMsg::Command(Command::SetPtt(true))).await;
    send(&mut ws, &ClientMsg::Auth { username: "oe1test".into(), password: "wrong".into() }).await;
    match recv_msg(&mut ws).await {
        ServerMsg::AuthRejected(why) => {
            assert!(!why.is_empty());
            // Which half was wrong is exactly what an attacker with a list of
            // callsigns wants to be told, so it must not be in there.
            let why = why.to_lowercase();
            assert!(!why.contains("unknown user"), "the message must not name the wrong half");
        }
        other => panic!("expected AuthRejected, got {other:?}"),
    }

    // A client that has not signed in holds nothing: a second one is still
    // offered the challenge rather than being turned away as Busy. This is the
    // part that stops a stranger locking the operator out of their own radio
    // simply by opening a socket.
    let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.expect("connect 2");
    send(&mut ws2, &hello()).await;
    assert_eq!(recv_msg(&mut ws2).await, ServerMsg::AuthRequired);

    // The right credentials get in — but not before the wrong answer above has
    // finished shutting the door, which is what makes guessing impractical.
    let started = std::time::Instant::now();
    send(&mut ws2, &ClientMsg::Auth { username: "oe1test".into(), password: "hunter2".into() })
        .await;
    match recv_msg(&mut ws2).await {
        ServerMsg::HelloAck { proto, caps, .. } => {
            assert_eq!(proto, PROTO_VERSION);
            assert_eq!(caps.label, "Test signal generator");
        }
        other => panic!("expected HelloAck after signing in, got {other:?}"),
    }
    assert!(
        started.elapsed() >= Duration::from_millis(2_500),
        "a correct sign-in was judged {:?} after a wrong one, without serving out the lockout",
        started.elapsed()
    );

    // ...and now that somebody is in, a second signed-in client is a listener
    // rather than being turned away: the radio is shared.
    send(&mut ws, &ClientMsg::Auth { username: "oe1test".into(), password: "hunter2".into() })
        .await;
    match recv_msg(&mut ws).await {
        ServerMsg::HelloAck { .. } => {}
        other => panic!("expected HelloAck for the second signed-in client, got {other:?}"),
    }
}

async fn wait_for_control(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> sdroxide_types::ControlStatus {
    loop {
        if let ServerMsg::Control(status) = recv_msg(ws).await {
            return status;
        }
    }
}

/// Two clients on one radio: the first holds the key, the second asks, the
/// first hands it over, and a listener's PTT never reaches the engine.
#[tokio::test(flavor = "multi_thread")]
async fn the_control_key_changes_hands() {
    spawn_server(SHARE_PORT, None).await;
    let url = format!("ws://127.0.0.1:{SHARE_PORT}/ws");

    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.expect("connect a");
    send(&mut a, &hello()).await;
    assert!(matches!(recv_msg(&mut a).await, ServerMsg::HelloAck { .. }));
    let a_status = wait_for_control(&mut a).await;
    assert!(a_status.i_hold(), "the first client holds an unattended radio");
    assert!(!a_status.shared());

    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.expect("connect b");
    send(&mut b, &hello()).await;
    assert!(matches!(recv_msg(&mut b).await, ServerMsg::HelloAck { .. }));
    let b_status = wait_for_control(&mut b).await;
    assert!(b_status.shared());
    assert!(!b_status.i_hold());
    let a_slot = a_status.me;
    let b_slot = b_status.me;

    send(&mut b, &ClientMsg::RequestControl).await;
    let a_asked = wait_for_control(&mut a).await;
    assert!(a_asked.waiting.iter().any(|c| c.slot == b_slot), "the holder sees the request");

    send(&mut a, &ClientMsg::GrantControl { to: b_slot }).await;
    let b_holds = wait_for_control(&mut b).await;
    assert!(b_holds.i_hold(), "the grant gives the asker the key");
    assert_eq!(b_holds.holder.as_ref().map(|h| h.slot), Some(b_slot));

    // A client that is only listening cannot key the transmitter. The engine
    // would otherwise go into PTT; the proof is a Notice rather than a State
    // with ptt = true.
    send(&mut a, &ClientMsg::Command(Command::SetPtt(true))).await;
    loop {
        match recv_msg(&mut a).await {
            ServerMsg::Notice(Some(why)) => {
                assert!(why.contains("listening"), "{why}");
                break;
            }
            ServerMsg::State(s) if s.ptt => panic!("a listener keyed the transmitter"),
            _ => {}
        }
    }

    // The holder going away leaves the radio unattended (nobody is waiting),
    // and the remaining client can pick the key up.
    drop(b);
    let vacated = wait_for_control(&mut a).await;
    assert!(!vacated.i_hold(), "the key is not silently handed to a listener");
    send(&mut a, &ClientMsg::RequestControl).await;
    let a_again = wait_for_control(&mut a).await;
    assert!(a_again.i_hold(), "a free radio is handed to whoever asks");
    let _ = a_slot;
}

/// A named operator is given back the settings the station kept for them, and
/// a change they send is what the station writes.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_operator_gets_their_settings_back() {
    let saved = std::sync::Arc::new(std::sync::Mutex::new(None::<UserSettings>));
    let saved_write = saved.clone();
    spawn_server_with(
        SETTINGS_PORT,
        Some(Box::new(|| Access {
            shared: RemoteAccess::default(),
            users: Users {
                users: vec![User {
                    name: "oe1test".into(),
                    password: "hunter2".into(),
                    ..User::default()
                }],
            },
        })),
        Some(Box::new(|_| {
            let mut s = UserSettings::default();
            s.volume = 0.42;
            s
        })),
        Some(Box::new(move |name, settings| {
            assert_eq!(name, "oe1test");
            *saved_write.lock().unwrap() = Some(settings.clone());
            Ok(())
        })),
    )
    .await;

    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{SETTINGS_PORT}/ws"))
            .await
            .expect("connect");
    send(&mut ws, &hello()).await;
    assert_eq!(recv_msg(&mut ws).await, ServerMsg::AuthRequired);
    send(&mut ws, &ClientMsg::Auth { username: "oe1test".into(), password: "hunter2".into() })
        .await;
    assert!(matches!(recv_msg(&mut ws).await, ServerMsg::HelloAck { .. }));

    loop {
        if let ServerMsg::UserSettings(s) = recv_msg(&mut ws).await {
            assert!((s.volume - 0.42).abs() < f32::EPSILON);
            break;
        }
    }

    let mut pushed = UserSettings::default();
    pushed.volume = 0.3;
    send(&mut ws, &ClientMsg::SetUserSettings(pushed.clone())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stored = saved.lock().unwrap().clone().expect("the station wrote the settings");
    assert!((stored.volume - 0.3).abs() < f32::EPSILON);
}

/// A named operator who reconnects — the same person on a new socket, the old
/// one still half-open — is working the radio again, not listening to themselves.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_operator_who_reconnects_takes_the_radio_back() {
    spawn_server(
        RECONNECT_PORT,
        Some(Box::new(|| Access {
            shared: RemoteAccess::default(),
            users: Users {
                users: vec![User {
                    name: "df7zz".into(),
                    password: "hunter2".into(),
                    ..User::default()
                }],
            },
        })),
    )
    .await;
    let url = format!("ws://127.0.0.1:{RECONNECT_PORT}/ws");

    let (mut first, _) = tokio_tungstenite::connect_async(&url).await.expect("first");
    send(&mut first, &hello()).await;
    assert_eq!(recv_msg(&mut first).await, ServerMsg::AuthRequired);
    send(&mut first, &ClientMsg::Auth { username: "df7zz".into(), password: "hunter2".into() })
        .await;
    assert!(matches!(recv_msg(&mut first).await, ServerMsg::HelloAck { .. }));
    let held = wait_for_control(&mut first).await;
    assert!(held.i_hold());
    assert!(!held.shared());

    let (mut second, _) = tokio_tungstenite::connect_async(&url).await.expect("second");
    send(&mut second, &hello()).await;
    assert_eq!(recv_msg(&mut second).await, ServerMsg::AuthRequired);
    send(&mut second, &ClientMsg::Auth { username: "df7zz".into(), password: "hunter2".into() })
        .await;
    assert!(matches!(recv_msg(&mut second).await, ServerMsg::HelloAck { .. }));
    let back = wait_for_control(&mut second).await;
    assert!(back.i_hold(), "the reconnecting operator has the radio");
    assert!(!back.shared(), "the stale session is not still sitting beside them: {back:?}");
    let _ = first;
}
