//! The sign-in gate every remote connection passes through.
//!
//! Two rules, and they are the whole design:
//!
//! * **One attempt at a time, server-wide.** Guessing a password is only
//!   practical if guesses can be made in parallel, so they cannot be: every
//!   attempt on every socket queues for the same turnstile. Whoever finds it
//!   occupied is told to come back rather than being parked on it, so the queue
//!   cannot be made to grow either.
//! * **A wrong answer shuts the door for [`LOCKOUT`].** The next attempt — from
//!   anyone — waits that long before it is judged. Combined with the first rule
//!   that caps the whole server at one guess every three seconds, which turns a
//!   dictionary run into a project measured in years and makes the wall-clock
//!   cost of a wrong guess enormously larger than any timing difference inside
//!   the comparison.
//!
//! The cost is deliberate and worth naming: while an attack is running, an
//! operator signing in legitimately waits their turn too. That is the trade a
//! shared lockout makes, and for a single-radio station it is the right way
//! round — a few seconds of delay against somebody else keying the transmitter.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{info, warn};

use sdroxide_types::{AUTH_BUSY, AUTH_REFUSED, RemoteAccess, Users};

use crate::passwd;
use crate::{Access, AccessFn};

/// How long a wrong password shuts the door for.
const LOCKOUT: Duration = Duration::from_secs(3);

/// How long the operator has to get signed in, from the challenge to the last
/// attempt. Generous because a person is typing, quite possibly after going to
/// find a password manager; the turnstile is what limits guessing, not this.
const SIGN_IN_BUDGET: Duration = Duration::from_secs(300);

/// How many tries one socket gets before it is closed. Not the brute-force
/// defence — the turnstile is — but a socket that has sat here misspelling
/// things a dozen times is not going to succeed on the thirteenth, and it
/// stops a client that answers the challenge with garbage from doing so for
/// ever.
const MAX_ATTEMPTS: usize = 12;

/// Who a session belongs to.
///
/// A station with one secret cannot tell its clients apart and does not try:
/// the name is whatever was typed into the sign-in card, which on such a
/// station is commonly nothing at all. A station with a roster knows exactly
/// who this is, and that is what makes per-operator settings and a named
/// control key possible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The name the roster holds, or the one the client offered. Empty on an
    /// open station, or a `[remote_access]` one asked only for a password.
    pub(crate) name: String,
    /// Whether this operator may key the transmitter. Always true unless a
    /// roster entry says otherwise — a station with one secret has one
    /// operator, and it is theirs.
    pub(crate) may_transmit: bool,
    /// Whether the station knows who this is, rather than only that they knew
    /// the password. Only a roster sign-in does, and only then is there
    /// anywhere to keep this operator's settings.
    pub(crate) named: bool,
}

impl Identity {
    /// What to call this session in a log line or on another client's screen.
    pub(crate) fn label(&self) -> &str {
        if self.name.is_empty() { "the operator" } else { &self.name }
    }
}

/// What a check came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Ok(Identity),
    Wrong,
    /// Another attempt is being judged right now. Not a rejection — nothing was
    /// even compared — so it neither counts against the client nor extends the
    /// lockout.
    Busy,
}

/// What the server is asking for right now.
///
/// The roster wins where it has anything to say, and `[remote_access]` is what
/// a station without one uses — which is every station that predates the
/// roster, and every single-operator station that has no use for names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Demand {
    /// One secret for the whole station.
    Shared(RemoteAccess),
    /// Named operators, each with their own.
    Roster(Users),
}

impl Demand {
    /// What this server currently asks of a socket, or `None` if it asks
    /// nothing and is open to anyone who can reach the port.
    fn of(access: &Access) -> Option<Demand> {
        if access.users.is_enforced() {
            return Some(Demand::Roster(access.users.clone()));
        }
        access.shared.is_enforced().then(|| Demand::Shared(access.shared.clone()))
    }

    /// Judge one answer, and say who gave it.
    ///
    /// The expensive half — an Argon2 verification, a tenth of a second by
    /// design — runs for a username that is not on the roster too. Skipping it
    /// there would sort a list of callsigns into those this station knows and
    /// those it does not, from the outside, at one guess per three seconds.
    fn judge(&self, username: &str, password: &str) -> Option<Identity> {
        match self {
            Demand::Shared(want) => want.accepts(username, password).then(|| Identity {
                name: username.to_string(),
                may_transmit: true,
                named: false,
            }),
            Demand::Roster(users) => {
                let Some(i) = users.find(username) else {
                    passwd::verify_nobody(password);
                    return None;
                };
                let user = &users.users[i];
                // A password typed into the file by hand, not yet hashed. It is
                // accepted so the roster can be edited with a text editor while
                // the station is running; the next start rewrites it as a hash
                // — see `hash_hand_written_passwords` in the binary.
                let ok = if user.password.is_empty() {
                    passwd::verify(&user.password_hash, password)
                } else {
                    warn!(
                        "{}'s password is still in the clear in users.toml; it will be hashed at \
                         the next start",
                        user.name
                    );
                    user.accepts_plaintext(password)
                };
                ok.then(|| Identity {
                    name: user.name.clone(),
                    may_transmit: user.may_transmit,
                    named: true,
                })
            }
        }
    }
}

/// The turnstile, plus how to find out what the server currently demands.
#[derive(Default)]
pub(crate) struct AuthGate {
    /// Consulted once per connection and again on each attempt, rather than
    /// read at startup: it is a file on this machine, and an operator who
    /// changes their password expects the change to hold without restarting
    /// the server and dropping whoever is on it.
    access: Option<AccessFn>,
    /// Both the "one at a time" lock and, inside it, the instant before which
    /// nothing may be judged. `None` means the door is open now.
    turnstile: Mutex<Option<Instant>>,
}

impl AuthGate {
    pub(crate) fn new(access: Option<AccessFn>) -> Self {
        AuthGate { access, turnstile: Mutex::new(None) }
    }

    /// What this server demands right now, or `None` if it is open to anyone
    /// who can reach the port.
    pub(crate) fn required(&self) -> Option<Demand> {
        Demand::of(&(self.access.as_ref()?)())
    }

    /// Judge one attempt.
    pub(crate) async fn check(&self, username: &str, password: &str) -> Verdict {
        // `try_lock`, not `lock`: an occupied turnstile means somebody else is
        // mid-attempt, and the answer to that is "come back", not a queue an
        // attacker can lengthen at will with one socket per entry.
        let Ok(mut door) = self.turnstile.try_lock() else { return Verdict::Busy };
        // Serve out whatever is left of the last wrong answer's lockout before
        // judging this one, still holding the turnstile so nothing slips past
        // while we wait.
        if let Some(until) = *door {
            tokio::time::sleep_until(until).await;
        }
        // Re-read rather than reuse an earlier copy: the wait above may have
        // been three seconds long, and the operator may have spent them
        // changing the password.
        let Some(want) = self.required() else {
            return Verdict::Ok(Identity { may_transmit: true, ..Identity::default() });
        };
        // Off the runtime: a roster verification is Argon2, which is meant to
        // cost a tenth of a second of processor time. That is nothing next to
        // the lockout above, but it is far too long to spend on a thread that
        // is also carrying other clients' audio.
        let (username, password) = (username.to_string(), password.to_string());
        let judged = tokio::task::spawn_blocking(move || want.judge(&username, &password))
            .await
            .unwrap_or_else(|e| {
                warn!("sign-in check did not finish ({e}); refusing");
                None
            });
        match judged {
            Some(who) => {
                *door = None;
                Verdict::Ok(who)
            }
            None => {
                *door = Some(Instant::now() + LOCKOUT);
                Verdict::Wrong
            }
        }
    }
}

/// The frames one protocol uses to run a challenge.
///
/// `/ws` and `/solar-ws` speak different message sets but the same exchange, so
/// the loop below is written once and told how to say each thing.
pub(crate) struct Frames<'a> {
    /// Which endpoint this is, for the log.
    pub(crate) what: &'static str,
    /// The encoded "I need a username and password".
    pub(crate) required: Vec<u8>,
    /// Builds the encoded "not those, and here is why".
    pub(crate) rejected: &'a (dyn Fn(&str) -> Vec<u8> + Send + Sync),
    /// Pulls credentials out of a client frame, if that is what it is.
    pub(crate) credentials: &'a (dyn Fn(&[u8]) -> Option<(String, String)> + Send + Sync),
}

/// Run the challenge to a verdict.
///
/// `Some` is a socket that may go on, and says who is on it — which is what
/// makes the roster worth having: the answer to "whose settings" and "who is on
/// the transmitter" is decided here and nowhere else. `None` is a socket that is
/// finished with. The caller has already read `Hello` and accepted its protocol
/// version, so a client on the wrong protocol is told *that* rather than being
/// asked to sign in to a server it could not have talked to anyway.
pub(crate) async fn challenge(
    socket: &mut WebSocket,
    gate: &AuthGate,
    frames: Frames<'_>,
) -> Option<Identity> {
    if gate.required().is_none() {
        // Nobody is asked, so nobody is named. An open station has no roster to
        // look anybody up in, which is exactly why it has no per-operator
        // settings either.
        return Some(Identity { may_transmit: true, ..Identity::default() });
    }
    if socket.send(Message::Binary(frames.required.into())).await.is_err() {
        return None;
    }

    // Bounded two ways: `deadline` caps how long the socket may sit here at all,
    // and `attempts` caps how many guesses it gets within that. Keepalives count
    // against the first but not the second.
    let deadline = Instant::now() + SIGN_IN_BUDGET;
    let mut attempts = 0;
    while attempts < MAX_ATTEMPTS {
        let bytes = match tokio::time::timeout_at(deadline, socket.recv()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => bytes,
            // Keepalives. Worth handling here and not in the `Hello` read
            // above, because this socket may be open for minutes on end — a
            // person is typing, quite possibly after going to find a password
            // manager — which is exactly long enough for a browser, a proxy or
            // a load balancer to ping it. The transport answers them; all that
            // is needed here is not to mistake one for a failed sign-in.
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            // Timed out, closed, errored, or a text frame, which this protocol
            // never sends. Nothing here is worth answering.
            _ => return None,
        };
        attempts += 1;
        // Anything that is not an answer to the challenge is either a client
        // that does not know about it or one trying to walk around it, and
        // both get the same treatment: ignored, and charged an attempt.
        let Some((username, password)) = (frames.credentials)(&bytes) else {
            warn!("{}: expected sign-in credentials", frames.what);
            continue;
        };
        let why = match gate.check(&username, &password).await {
            Verdict::Ok(who) => {
                info!("{}: signed in as {username:?}", frames.what);
                return Some(who);
            }
            Verdict::Wrong => {
                // Deliberately the same message whether the username or the
                // password was wrong — see [`AUTH_REFUSED`].
                warn!("{}: sign-in refused for {username:?}", frames.what);
                AUTH_REFUSED
            }
            // Not a refusal, and the wording is a shared constant because the
            // client has to be able to tell it apart from one: an answer that
            // was never compared must not cost a remembered password, and a
            // tab nobody is looking at has to be free to offer it again.
            Verdict::Busy => AUTH_BUSY,
        };
        if socket.send(Message::Binary((frames.rejected)(why).into())).await.is_err() {
            return None;
        }
    }
    warn!("{}: too many sign-in attempts; closing", frames.what);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use sdroxide_types::User;

    fn gate(user: &str, pass: &str) -> AuthGate {
        let want = Access {
            shared: RemoteAccess { username: user.into(), password: pass.into() },
            users: Users::default(),
        };
        AuthGate::new(Some(Box::new(move || want.clone())))
    }

    /// A station whose roster holds these operators, passwords hashed as the
    /// file would hold them.
    fn roster_gate(entries: &[(&str, &str, bool)]) -> AuthGate {
        let users = Users {
            users: entries
                .iter()
                .map(|(name, pass, tx)| User {
                    name: (*name).into(),
                    password_hash: passwd::hash(pass).expect("hash"),
                    password: String::new(),
                    may_transmit: *tx,
                })
                .collect(),
        };
        let want = Access { shared: RemoteAccess::default(), users };
        AuthGate::new(Some(Box::new(move || want.clone())))
    }

    /// Assert a verdict was a sign-in, and hand back who it says signed in.
    fn who(v: Verdict) -> Identity {
        match v {
            Verdict::Ok(id) => id,
            other => panic!("expected a sign-in, got {other:?}"),
        }
    }

    /// A server told nothing, and a server told empty strings, are both open —
    /// which is what every installation that predates this feature is.
    #[tokio::test]
    async fn a_server_with_no_credentials_challenges_nobody() {
        assert!(AuthGate::default().required().is_none());
        assert!(gate("", "").required().is_none());
    }

    #[tokio::test]
    async fn the_right_password_gets_in_and_the_wrong_one_does_not() {
        let g = gate("oe1test", "hunter2");
        assert!(g.required().is_some());
        who(g.check("oe1test", "hunter2").await);
        assert_eq!(g.check("oe1test", "wrong").await, Verdict::Wrong);
    }

    /// A station with one secret cannot say who is on it, and does not pretend
    /// to: there is no roster to look the name up in, so nothing is named and
    /// there is nowhere to keep this operator's settings.
    #[tokio::test]
    async fn one_shared_secret_names_nobody() {
        let id = who(gate("oe1test", "hunter2").check("oe1test", "hunter2").await);
        assert!(!id.named);
        assert!(id.may_transmit);
        // An open station names nobody either, and asks nobody.
        let open = who(AuthGate::default().check("", "").await);
        assert!(!open.named);
        assert!(open.may_transmit, "an open station is the operator's own");
    }

    /// The roster: each operator has their own password, and the station knows
    /// which of them signed in.
    #[tokio::test]
    async fn a_roster_tells_its_operators_apart() {
        let g = roster_gate(&[("oe1test", "hunter2", true), ("dl2abc", "correct-horse", true)]);
        assert!(g.required().is_some());

        let first = who(g.check("oe1test", "hunter2").await);
        assert_eq!(first.name, "oe1test");
        assert!(first.named, "a roster sign-in is a named one");

        let second = who(g.check("dl2abc", "correct-horse").await);
        assert_eq!(second.name, "dl2abc");

        // One operator's password is not another's, and a name that is not on
        // the roster is refused like a wrong password — see `AUTH_REFUSED`.
        assert_eq!(g.check("oe1test", "correct-horse").await, Verdict::Wrong);
        assert_eq!(g.check("dl9xyz", "hunter2").await, Verdict::Wrong);
        assert_eq!(g.check("", "hunter2").await, Verdict::Wrong);
    }

    /// The receive-only operator: signed in, known by name, and not allowed
    /// near the transmitter. What the session does with that is
    /// `crate::session`'s business; this is where it is decided.
    #[tokio::test]
    async fn an_operator_can_be_admitted_without_the_transmitter() {
        let g = roster_gate(&[("guest", "listen-only", false)]);
        let id = who(g.check("guest", "listen-only").await);
        assert_eq!(id.name, "guest");
        assert!(id.named);
        assert!(!id.may_transmit);
    }

    /// A password written into the file by hand still works, so the roster can
    /// be edited with a text editor. It is hashed at the next start.
    #[tokio::test]
    async fn a_hand_written_password_is_accepted_as_it_stands() {
        let users = Users {
            users: vec![User {
                name: "oe1test".into(),
                password: "hunter2".into(),
                ..User::default()
            }],
        };
        let want = Access { shared: RemoteAccess::default(), users };
        let g = AuthGate::new(Some(Box::new(move || want.clone())));
        assert_eq!(who(g.check("oe1test", "hunter2").await).name, "oe1test");
        assert_eq!(g.check("oe1test", "hunter3").await, Verdict::Wrong);
    }

    /// Precedence: a roster is what a station with one is asked against, and
    /// `[remote_access]` is what a station without one uses. Both configured is
    /// not ambiguous — the roster is the more specific statement, and the one
    /// the operator wrote most recently.
    #[tokio::test]
    async fn the_roster_wins_over_the_shared_secret() {
        let users = Users {
            users: vec![User {
                name: "dl2abc".into(),
                password_hash: passwd::hash("correct-horse").expect("hash"),
                ..User::default()
            }],
        };
        let want = Access {
            shared: RemoteAccess { username: "oe1test".into(), password: "hunter2".into() },
            users,
        };
        let g = AuthGate::new(Some(Box::new(move || want.clone())));
        assert_eq!(who(g.check("dl2abc", "correct-horse").await).name, "dl2abc");
        assert_eq!(
            g.check("oe1test", "hunter2").await,
            Verdict::Wrong,
            "the shared secret is not a way around the roster"
        );
    }

    /// The lockout: after a wrong answer the *next* attempt — right or wrong,
    /// and from whoever — waits out the full three seconds before it is judged.
    #[tokio::test(start_paused = true)]
    async fn a_wrong_answer_shuts_the_door_for_three_seconds() {
        let g = gate("oe1test", "hunter2");
        assert_eq!(g.check("oe1test", "wrong").await, Verdict::Wrong);

        let started = Instant::now();
        who(g.check("oe1test", "hunter2").await);
        assert!(
            started.elapsed() >= LOCKOUT,
            "the next attempt was judged after {:?}, not the full lockout",
            started.elapsed()
        );

        // ...and a correct answer opens it again, so an operator who mistypes
        // once is not made to wait a second time.
        let started = Instant::now();
        who(g.check("oe1test", "hunter2").await);
        assert!(started.elapsed() < LOCKOUT);
    }

    /// The parallelism rule: while one attempt is being judged — which, after a
    /// wrong answer, takes three seconds — every other attempt is turned away
    /// rather than run alongside it or queued behind it.
    #[tokio::test(start_paused = true)]
    async fn attempts_cannot_be_run_side_by_side() {
        let g = std::sync::Arc::new(gate("oe1test", "hunter2"));
        assert_eq!(g.check("oe1test", "wrong").await, Verdict::Wrong);

        // One guesser sits in the lockout holding the turnstile...
        let held = std::sync::Arc::clone(&g);
        let slow = tokio::spawn(async move { held.check("oe1test", "guess-1").await });
        tokio::task::yield_now().await;

        // ...and everybody else is refused outright, having been compared
        // against nothing.
        for _ in 0..50 {
            assert_eq!(g.check("oe1test", "guess-n").await, Verdict::Busy);
        }
        assert_eq!(slow.await.unwrap(), Verdict::Wrong);
    }

    /// A username that is not on the roster costs the same work as one that is.
    /// Without that, the station sorts a list of callsigns into those it knows
    /// and those it does not, from the outside — see [`Demand::judge`].
    #[tokio::test]
    async fn an_unknown_name_is_not_cheaper_than_a_known_one() {
        let demand = match roster_gate(&[("oe1test", "hunter2", true)]).required() {
            Some(d) => d,
            None => panic!("the roster should be enforced"),
        };
        // Measured on the judgement itself, not through `check`, whose lockout
        // would swamp the difference this is about.
        let known = std::time::Instant::now();
        assert!(demand.judge("oe1test", "wrong").is_none());
        let known = known.elapsed();

        let unknown = std::time::Instant::now();
        assert!(demand.judge("dl9xyz", "wrong").is_none());
        let unknown = unknown.elapsed();

        // Argon2 is deliberately slow — tens of milliseconds — so a case that
        // ran one is nowhere near a millisecond, and a case that skipped it is
        // a string comparison. Asserted as a floor on both rather than as a
        // ratio between them, which is the same statement without depending on
        // how loaded the machine running the tests is.
        let floor = Duration::from_millis(1);
        assert!(known >= floor, "a known name was judged in {known:?}: no verification ran");
        assert!(unknown >= floor, "an unknown name was judged in {unknown:?}: none ran");
    }

    /// A password changed while the server is running applies to the next
    /// sign-in; nobody has to restart it and drop whoever is connected.
    #[tokio::test]
    async fn the_credentials_are_re_read_for_every_attempt() {
        let live = std::sync::Arc::new(std::sync::Mutex::new(Access {
            shared: RemoteAccess { username: "oe1test".into(), password: "hunter2".into() },
            users: Users::default(),
        }));
        let seen = std::sync::Arc::clone(&live);
        let g = AuthGate::new(Some(Box::new(move || seen.lock().unwrap().clone())));

        who(g.check("oe1test", "hunter2").await);
        live.lock().unwrap().shared.password = "correct-horse".into();
        who(g.check("oe1test", "correct-horse").await);
        // And clearing it out reopens the server, without a restart.
        *live.lock().unwrap() = Access::default();
        assert!(g.required().is_none());
    }

    /// The same promise for the roster, which needs it more: a station several
    /// people work is one where somebody is added, or has their password
    /// changed, while everybody else is connected.
    #[tokio::test]
    async fn an_operator_can_be_added_while_the_server_runs() {
        let live = std::sync::Arc::new(std::sync::Mutex::new(Access::default()));
        let seen = std::sync::Arc::clone(&live);
        let g = AuthGate::new(Some(Box::new(move || seen.lock().unwrap().clone())));

        // Nothing configured at all: the station is open, as it always was.
        assert!(g.required().is_none());

        live.lock().unwrap().users.users.push(User {
            name: "dl2abc".into(),
            password_hash: passwd::hash("correct-horse").expect("hash"),
            ..User::default()
        });
        assert!(g.required().is_some(), "one entry closes the door");
        assert_eq!(who(g.check("dl2abc", "correct-horse").await).name, "dl2abc");

        // And taking them off again reopens it, with nobody dropped.
        live.lock().unwrap().users.users.clear();
        assert!(g.required().is_none());
    }
}
