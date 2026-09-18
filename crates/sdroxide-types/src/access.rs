//! Who may drive this station from somewhere else.
//!
//! The server hands out one radio, and a radio is a transmitter. Left open, a
//! port forwarded so the operator can listen from work is also a port anybody
//! else can key, on the operator's licence and callsign — so the server asks
//! for a username and a password before it hands anything over.
//!
//! This is the *configuration* half, kept here rather than in `sdroxide-config`
//! because three crates need to name it: the config file that stores it, the
//! server that enforces it, and the settings dialog that edits it.

use serde::{Deserialize, Serialize};

use crate::{Band, BandStackEntry, InputSettings, SQUELCH_OPEN_DB, UiSettings};

/// The credentials a remote client has to present before the server will let it
/// near the radio.
///
/// Both fields empty means the server is open to anyone who can reach the port,
/// which is what every version before this one was — so an existing
/// `config.toml` upgrades into exactly the behaviour it already had, and the
/// operator turns this on when they mean to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteAccess {
    pub username: String,
    pub password: String,
}

impl RemoteAccess {
    /// Whether connecting clients are challenged at all.
    ///
    /// A password on its own is enough. A single-operator station has no use
    /// for a username, and insisting on one would only produce a shack full of
    /// accounts called `admin` — the password is the secret either way.
    pub fn is_enforced(&self) -> bool {
        !self.username.is_empty() || !self.password.is_empty()
    }

    /// Whether these are the configured credentials.
    ///
    /// Both halves are always compared and neither comparison stops early, so
    /// the time this takes says nothing about how much of a guess was right.
    /// The server's turnstile already makes a timing attack impractical; this
    /// costs a few hundred nanoseconds and means it never has to be the only
    /// thing standing in the way.
    pub fn accepts(&self, username: &str, password: &str) -> bool {
        let user_ok = ct_eq(self.username.as_bytes(), username.as_bytes());
        let pass_ok = ct_eq(self.password.as_bytes(), password.as_bytes());
        // Non-short-circuiting `&`, deliberately: `&&` would skip the second
        // test whenever the first failed, which is a timing signal telling an
        // attacker they have the username right.
        user_ok & pass_ok
    }
}

/// Byte comparison that takes as long when the first byte differs as when the
/// last one does.
///
/// The lengths are folded in rather than checked first — a length check that
/// returned early would leak the length of the secret, which for a password is
/// most of the search space. The loop runs over the longer of the two, so its
/// duration depends only on what the caller supplied.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    diff == 0
}

/// One operator of a station that more than one person uses.
///
/// [`RemoteAccess`] above is the whole of what a single-operator station needs:
/// one secret, and whoever knows it is the operator. A station several people
/// work needs to tell them apart — not to keep them from each other's radio,
/// which the control key does, but because "whose settings are these" and "who
/// is on the transmitter" are questions with a name for an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    /// What this operator signs in as. A callsign is the obvious choice and
    /// what the log will show, but nothing here requires one.
    pub name: String,
    /// The password as a PHC-string hash — what [`User::password`] becomes.
    pub password_hash: String,
    /// A password written in by hand, still in the clear.
    ///
    /// Accepted so the roster can be edited with a text editor, and cleared
    /// again the first time the station reads it: the server hashes it into
    /// `password_hash` and writes the file back. This is somebody *else's*
    /// password, which is the difference from every other secret sdroxide
    /// keeps — those are the operator's own, and a file they can read anyway.
    pub password: String,
    /// Whether this operator may key the transmitter.
    ///
    /// `false` leaves them everything else: they can hold the control key,
    /// tune around and work the receiver, and PTT, TUNE and the CW key are
    /// refused. For a listener, or an unlicensed guest at the radio.
    #[serde(default = "yes")]
    pub may_transmit: bool,
}

fn yes() -> bool {
    true
}

impl User {
    /// Whether `password` is the one written into this entry by hand.
    ///
    /// Only for an entry that still holds one — a hashed entry is checked by
    /// whoever knows the hash function, which is the server. Compared in
    /// constant time for the same reason as [`RemoteAccess::accepts`].
    pub fn accepts_plaintext(&self, password: &str) -> bool {
        !self.password.is_empty() & ct_eq(self.password.as_bytes(), password.as_bytes())
    }
}

impl Default for User {
    /// Deliberately hand-written rather than derived: the derive would make a
    /// new entry one that may not transmit, while an entry in the file with no
    /// `may_transmit` line reads back as one that may. The two have to agree,
    /// and permission to transmit is the sane default for somebody the
    /// operator has just put on the roster.
    fn default() -> Self {
        User {
            name: String::new(),
            password_hash: String::new(),
            password: String::new(),
            may_transmit: true,
        }
    }
}

/// The station's operators, as `users.toml` holds them.
///
/// An empty roster is the default and means this file has nothing to say:
/// [`RemoteAccess`] decides, exactly as it did before any of this existed. That
/// is the upgrade path — an installation that never creates the file keeps the
/// sign-in it already had.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Users {
    pub users: Vec<User>,
}

impl Users {
    /// Whether this roster is what the server should ask against.
    pub fn is_enforced(&self) -> bool {
        self.users.iter().any(|u| !u.name.is_empty())
    }

    /// Which entry goes by `name`, if any.
    ///
    /// Every entry is compared, and the answer is taken from the last match
    /// rather than the first, so the search costs the same whoever is asked
    /// for — a loop that returned early would time out the difference between
    /// a name on the roster and one that is not, which is exactly what an
    /// attacker holding a list of callsigns wants to know.
    pub fn find(&self, name: &str) -> Option<usize> {
        let mut found = None;
        for (i, u) in self.users.iter().enumerate() {
            if ct_eq(u.name.as_bytes(), name.as_bytes()) {
                found = Some(i);
            }
        }
        found
    }

    /// The entries still holding a hand-written password, for the server to
    /// hash and write back.
    pub fn plaintext_entries(&self) -> Vec<usize> {
        self.users
            .iter()
            .enumerate()
            .filter(|(_, u)| !u.password.is_empty())
            .map(|(i, _)| i)
            .collect()
    }
}

/// One client on a radio, as the others see it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// The station's number for this client, and what a grant or a refusal is
    /// addressed to. Never reused within a run of the server, so an answer
    /// cannot land on whoever attached after the asker left.
    pub slot: u64,
    /// What to call them: the roster name they signed in under. Empty on a
    /// station with one shared password, which cannot tell its clients apart —
    /// the client shows something like "another client" for those.
    pub name: String,
    /// Whether this one may key the transmitter at all. A [`User`] with
    /// `may_transmit = false` can hold the control key and work the receiver;
    /// the station refuses PTT, TUNE and the CW key from them.
    pub may_transmit: bool,
}

impl ClientInfo {
    /// What to call this client on somebody else's screen.
    ///
    /// A station with one shared password cannot tell its clients apart, so
    /// there is genuinely no name to show — and saying so is better than showing
    /// an empty chip or inventing "Client 3", which would look like a name and
    /// is only a counter.
    pub fn label(&self) -> String {
        if self.name.is_empty() { "another client".to_string() } else { self.name.clone() }
    }
}

/// How a radio is being shared right now.
///
/// The whole of what a client knows about the other clients on its radio: who is
/// working it, who else is listening, and who has asked for a turn. Sent on
/// connect and on every change, so it is a standing condition rather than an
/// event — a client attaching to a radio somebody else is working has to know
/// that before it draws a PTT button.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlStatus {
    /// Which client this was sent to, so it can find itself in the lists below.
    /// The one field a client cannot work out for itself.
    pub me: u64,
    /// The client working the radio, if anybody is. `None` means the key is
    /// free and the next request for it is granted without anybody being asked.
    pub holder: Option<ClientInfo>,
    /// Everybody signed in, the holder included, in the order they arrived.
    pub clients: Vec<ClientInfo>,
    /// Who has asked for the control key, oldest first. Shown to the holder as
    /// something to answer; shown to an asker as their own place in the queue.
    pub waiting: Vec<ClientInfo>,
}

/// What follows a named operator from one screen to another.
///
/// The radio's own settings — the interface, the sample rate, the memories —
/// stay with the station. These are the operator's: how the screen looks, how
/// the knobs are bound, the volume and squelch they last left, and the band
/// stacks they work from. A station with a roster keeps one of these per name
/// under `users/<name>/settings.json` and hands it over on sign-in, so the
/// same operator at a PC and on a phone sees the same screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    pub ui: UiSettings,
    pub input: InputSettings,
    /// Main-receiver AF volume, 0.0..=1.0. Applied to the radio only while
    /// this operator holds the control key, so a listener's preference does
    /// not turn the holder's audio down.
    pub volume: f32,
    /// Main-receiver squelch, in dBFS. Same rule as [`Self::volume`].
    pub squelch_db: f32,
    /// Band stacks, as `(band, entries)` rather than a map so the wire form
    /// does not depend on HashMap iteration order.
    pub bandstacks: Vec<(Band, Vec<BandStackEntry>)>,
}

impl Default for UserSettings {
    fn default() -> Self {
        UserSettings {
            ui: UiSettings::default(),
            input: InputSettings::default(),
            volume: 0.5,
            squelch_db: SQUELCH_OPEN_DB,
            bandstacks: Vec::new(),
        }
    }
}

impl ControlStatus {
    /// Whether the client this was sent to is the one working the radio.
    ///
    /// The question every control in the interface hangs off, so it is answered
    /// in one place: a client that is only listening shows no PTT and no dial,
    /// because the station would refuse them anyway.
    pub fn i_hold(&self) -> bool {
        self.holder.as_ref().is_some_and(|h| h.slot == self.me)
    }

    /// Whether this client has already asked for the control key.
    pub fn i_am_waiting(&self) -> bool {
        self.waiting.iter().any(|c| c.slot == self.me)
    }

    /// Whether anybody else is signed in to this radio.
    ///
    /// What decides whether the interface mentions sharing at all: a station
    /// with one operator on it looks exactly as it did before any of this
    /// existed.
    pub fn shared(&self) -> bool {
        self.clients.len() > 1
    }

    /// What this client may transmit with, if it may at all.
    ///
    /// Both halves of the answer in one place: holding the key is not permission
    /// to transmit, and permission to transmit is not the key.
    pub fn may_transmit(&self) -> bool {
        self.holder.as_ref().is_some_and(|h| h.slot == self.me && h.may_transmit)
    }
}

/// The other end of the same wire: which sdroxide server *this* screen dials.
///
/// [`RemoteAccess`] above is what a station demands of its visitors;
/// this is where a visitor goes. It belongs to the machine the operator is
/// sitting at rather than to the station — two laptops pointed at the same
/// shack each remember their own — which is why it is persisted next to the
/// sound-device selection in `config.toml` and never travels in the
/// [`StationConfig`](crate::StationConfig) bundle.
///
/// No credentials here on purpose. The server challenges the socket once it is
/// open and the sign-in dialog answers it, so a password would be a second copy
/// of a secret this file has no need to hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteServer {
    /// Host name or address of the machine running `sdroxide --server`. Empty
    /// until the operator has entered one.
    pub host: String,
    /// The port that server listens on — `server_port` in *its* `config.toml`.
    pub port: u16,
    /// Dial `wss://` rather than `ws://`: the server is behind something that
    /// terminates TLS for it.
    ///
    /// sdroxide's own server speaks plain WebSocket and nothing else, so this
    /// is never about the server itself — it is about the reverse proxy in
    /// front of it, which is how a station ends up answering on port 443. A
    /// browser client never needs the setting because the page it was served
    /// from already says which scheme to use; the native client has no page to
    /// follow and would otherwise offer the proxy a `ws://` handshake it can
    /// only refuse (issue #360).
    ///
    /// Off by default, so a `config.toml` written before this reads back as the
    /// plain link it was.
    pub tls: bool,
}

impl Default for RemoteServer {
    fn default() -> Self {
        // The port every sdroxide server binds unless it was told otherwise, so
        // the operator only has to type the half that is actually theirs.
        RemoteServer { host: String::new(), port: 4950, tls: false }
    }
}

impl RemoteServer {
    /// The WebSocket URL to dial, matching what `--connect` builds.
    ///
    /// A host that already carries a scheme is taken as a complete URL and used
    /// as typed: pasting a `ws://…/ws` (or a `wss://` one from a reverse proxy)
    /// into the address box is a reasonable thing to do, and rebuilding it
    /// around the port box would only break it — so [`Self::tls`] does not
    /// touch it either. An IPv6 literal is bracketed if the operator did not
    /// bracket it themselves, because `::1:4950` is not an address.
    pub fn url(&self) -> String {
        let host = self.host.trim();
        if host.contains("://") {
            return host.to_string();
        }
        let scheme = self.scheme();
        if host.contains(':') && !host.starts_with('[') {
            return format!("{scheme}://[{host}]:{}/ws", self.port);
        }
        format!("{scheme}://{host}:{}/ws", self.port)
    }

    /// Which WebSocket scheme [`Self::url`] will build, for a caller that wants
    /// to say so rather than dial it.
    pub fn scheme(&self) -> &'static str {
        if self.tls { "wss" } else { "ws" }
    }

    /// What to call the tab this connection opens: the address as typed, minus
    /// the scheme and the endpoint path that are the same for every server.
    pub fn label(&self) -> String {
        let host = self.host.trim();
        if host.contains("://") {
            return host
                .split_once("://")
                .map_or(host, |(_, rest)| rest)
                .trim_end_matches("/ws")
                .to_string();
        }
        format!("{host}:{}", self.port)
    }
}

/// What a station says when it turned an answer down: those credentials are
/// not the ones it wants.
///
/// Deliberately the same sentence whether the username or the password was
/// wrong — which one it was is exactly what an attacker with a list of
/// callsigns wants told.
pub const AUTH_REFUSED: &str = "username or password not accepted";

/// What a station says when it did not judge an answer at all, because another
/// one was already being judged.
///
/// A station allows one attempt at a time, so this is the ordinary answer to
/// several radios of one station signing in together — and it is *not* a
/// refusal: nothing was compared, and the same answer offered again a moment
/// later is as good as it ever was. Named here rather than written out at each
/// end so the client can tell the two apart by identity instead of by guessing
/// at a sentence; a client that cannot (an older one, or a station that
/// phrases it differently) falls back to asking the operator, which is what it
/// did before.
pub const AUTH_BUSY: &str = "another sign-in is being checked — try again";

/// Whether a rejection reason is the "come back" above rather than a verdict.
pub fn is_auth_busy(why: &str) -> bool {
    why == AUTH_BUSY
}

/// Where a connection stands with the server's sign-in challenge.
///
/// Reported by [`RadioController::auth_phase`](crate::RadioController::auth_phase)
/// so the UI can put a dialog up. An in-process engine is always [`Open`]:
/// there is no network and nothing to prove.
///
/// [`Open`]: AuthPhase::Open
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AuthPhase {
    /// Nothing to answer — a local engine, or a server with no credentials set.
    #[default]
    Open,
    /// The server is waiting for a username and password. `Some` carries why
    /// the previous attempt was turned down, for the dialog to show.
    Prompt(Option<String>),
    /// Credentials are with the server; waiting for its verdict.
    Checking,
}

impl AuthPhase {
    /// Whether the operator is being asked to sign in, either way.
    pub fn is_pending(&self) -> bool {
        !matches!(self, AuthPhase::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(user: &str, pass: &str) -> RemoteAccess {
        RemoteAccess { username: user.into(), password: pass.into() }
    }

    /// The upgrade path: a config that has never heard of this leaves the
    /// server exactly as open as it was.
    #[test]
    fn empty_credentials_do_not_challenge_anyone() {
        assert!(!RemoteAccess::default().is_enforced());
        assert!(creds("", "").accepts("", ""), "an open server accepts anything");
    }

    /// A password with no username is a complete configuration.
    #[test]
    fn a_password_alone_is_enough_to_close_the_door() {
        let want = creds("", "hunter2");
        assert!(want.is_enforced());
        assert!(want.accepts("", "hunter2"));
        assert!(!want.accepts("", "hunter3"));
        // ...and the empty username still has to match, so a client that sends
        // one cannot be treated as having sent none.
        assert!(!want.accepts("op", "hunter2"));
    }

    /// The two answers a station gives are not the same answer, and the client
    /// acts on the difference: a refusal throws a stored password away, a
    /// "come back" must not.
    #[test]
    fn a_busy_station_is_not_a_refusal() {
        assert!(is_auth_busy(AUTH_BUSY));
        assert!(!is_auth_busy(AUTH_REFUSED));
        assert!(!is_auth_busy(""));
    }

    #[test]
    fn both_halves_have_to_match() {
        let want = creds("oe1test", "hunter2");
        assert!(want.accepts("oe1test", "hunter2"));
        assert!(!want.accepts("oe1test", "hunter"), "wrong password");
        assert!(!want.accepts("someone", "hunter2"), "wrong username");
        assert!(!want.accepts("", ""), "no credentials at all");
        // A prefix is not a match, in either direction.
        assert!(!want.accepts("oe1test", "hunter22"));
        assert!(!want.accepts("oe1tes", "hunter2"));
    }

    /// The comparison must not be fooled by anything the length arithmetic
    /// could paper over — an empty secret matching a long guess, or a guess
    /// whose length happens to XOR to zero against it.
    #[test]
    fn lengths_are_part_of_the_comparison() {
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"", b"x"));
        assert!(!ct_eq(b"x", b""));
        // "abc" vs "abc\0": equal over the overlap, and the trailing NUL is
        // exactly what `unwrap_or(0)` pads the shorter side with — only the
        // length term catches this one.
        assert!(!ct_eq(b"abc", b"abc\0"));
        assert!(ct_eq(b"abc", b"abc"));
    }

    /// What the address box produces for the shapes an operator actually
    /// types: a bare host, a host with the port in it, and a pasted URL.
    #[test]
    fn the_dialled_url_follows_what_was_typed() {
        let plain = RemoteServer { host: "shack".into(), port: 4950, tls: false };
        assert_eq!(plain.url(), "ws://shack:4950/ws");
        assert_eq!(plain.label(), "shack:4950");
        // A whole URL wins over the port box — including a `wss://` one from a
        // reverse proxy, which this could not have built.
        let url = RemoteServer { host: "wss://shack.example/ws".into(), port: 4950, tls: false };
        assert_eq!(url.url(), "wss://shack.example/ws");
        assert_eq!(url.label(), "shack.example");
        // An IPv6 literal needs brackets before a port can be appended to it.
        let v6 = RemoteServer { host: "fe80::1".into(), port: 4950, tls: false };
        assert_eq!(v6.url(), "ws://[fe80::1]:4950/ws");
        let bracketed = RemoteServer { host: "[fe80::1]".into(), port: 4950, tls: false };
        assert_eq!(bracketed.url(), "ws://[fe80::1]:4950/ws");
    }

    /// The secure switch, which is the whole of what a station behind a reverse
    /// proxy needs from this end: the scheme changes and nothing else does, so
    /// the port the operator typed is still the port dialled (issue #360).
    #[test]
    fn the_secure_switch_changes_the_scheme_and_nothing_else() {
        let tls = RemoteServer { host: "shack.example".into(), port: 443, tls: true };
        assert_eq!(tls.url(), "wss://shack.example:443/ws");
        assert_eq!(tls.label(), "shack.example:443");
        // Brackets are still the IPv6 answer, secure or not.
        let v6 = RemoteServer { host: "fe80::1".into(), port: 443, tls: true };
        assert_eq!(v6.url(), "wss://[fe80::1]:443/ws");
        // A pasted URL is used as typed, so the switch has nothing to say about
        // it — turning it on must not rewrite somebody's plain-text tunnel.
        let typed = RemoteServer { host: "ws://shack.example/ws".into(), port: 443, tls: true };
        assert_eq!(typed.url(), "ws://shack.example/ws");
    }

    fn roster(names: &[&str]) -> Users {
        Users {
            users: names.iter().map(|n| User { name: (*n).into(), ..User::default() }).collect(),
        }
    }

    /// The upgrade path again, in the other file: a station that has never
    /// written a roster is one `[remote_access]` still decides for.
    #[test]
    fn an_empty_roster_decides_nothing() {
        assert!(!Users::default().is_enforced());
        // An entry with no name is not an operator either — that is a stanza
        // somebody started and did not finish, and treating it as a roster
        // would lock everybody out of a station that had been working.
        assert!(!roster(&[""]).is_enforced());
        assert!(roster(&["oe1test"]).is_enforced());
    }

    #[test]
    fn a_name_on_the_roster_is_found_and_one_that_is_not_is_not() {
        let r = roster(&["oe1test", "dl2abc"]);
        assert_eq!(r.find("oe1test"), Some(0));
        assert_eq!(r.find("dl2abc"), Some(1));
        assert_eq!(r.find("dl9xyz"), None);
        assert_eq!(r.find(""), None, "the empty name matches nobody");
        // A prefix is not a name, in either direction.
        assert_eq!(r.find("oe1tes"), None);
        assert_eq!(r.find("oe1test1"), None);
    }

    /// A new entry may transmit, and so does one in a file that predates the
    /// setting. The two have to agree — see [`User::default`].
    #[test]
    fn a_new_operator_may_transmit() {
        assert!(User::default().may_transmit);
        let from_file: User = serde_json::from_str(r#"{"name":"oe1test"}"#).expect("parse");
        assert!(from_file.may_transmit);
        let refused: User =
            serde_json::from_str(r#"{"name":"guest","may_transmit":false}"#).expect("parse");
        assert!(!refused.may_transmit);
    }

    /// Which entries the server has to hash and write back — the only reason a
    /// plaintext password is accepted at all.
    #[test]
    fn hand_written_passwords_are_the_ones_reported() {
        let mut r = roster(&["oe1test", "dl2abc", "guest"]);
        r.users[0].password_hash = "$argon2id$vraisemblable".into();
        r.users[1].password = "hunter2".into();
        r.users[2].password = "letmein".into();
        assert_eq!(r.plaintext_entries(), vec![1, 2]);
    }

    #[test]
    fn user_settings_round_trip_through_json() {
        let mut s = UserSettings::default();
        s.volume = 0.4;
        s.ui.frame_rate_fps = 30;
        let back: UserSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let empty: UserSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.volume, 0.5);
        assert_eq!(empty.squelch_db, SQUELCH_OPEN_DB);
    }

    #[test]
    fn a_local_engine_never_prompts() {
        assert!(!AuthPhase::default().is_pending());
        assert!(AuthPhase::Prompt(None).is_pending());
        assert!(AuthPhase::Checking.is_pending());
    }
}
