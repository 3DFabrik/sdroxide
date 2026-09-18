//! Who is working this radio, and how it changes hands.
//!
//! Only ever drawn for a station that says it has more than one client on the
//! radio. A station with one operator on it — which is every station this
//! program has ever connected to until now, and most of them still — looks
//! exactly as it did: no chip, no dialog, nothing to read.
//!
//! Kept out of `chrome.rs` on purpose. That file is the widget vocabulary the
//! whole interface is drawn from; this is one feature's worth of policy, and it
//! belongs beside [`crate::login`], which is the other thing on the screen that
//! is about who you are rather than about the radio.

use eframe::egui::{self, RichText};
use sdroxide_types::{ClientInfo, ControlStatus};

use crate::chrome;
use crate::theme;

/// What the operator did to the sharing, for the caller to pass on to the
/// controller.
///
/// Returned rather than acted on here so this module needs nothing but the
/// status: it draws what is true and reports what was pressed, which is what
/// makes it testable and what keeps the controller out of the paint code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlAction {
    /// Ask for the radio.
    Request,
    /// Give it up.
    Release,
    /// Let the client in `slot` have it.
    Grant(u64),
    /// Turn that client down.
    Deny(u64),
}

/// The chip that says who is at the radio, and the buttons that move it.
///
/// `None` when there is nothing to say — a station with one client, or one too
/// old to have an opinion. The caller draws nothing at all in that case, rather
/// than a chip reading "you", which would be a new thing on screen that tells
/// the operator something they already knew.
pub(crate) fn chip(ui: &mut egui::Ui, status: Option<&ControlStatus>) -> Option<ControlAction> {
    let status = status?;
    if !status.shared() {
        return None;
    }
    let mut action = None;
    ui.horizontal(|ui| {
        if status.i_hold() {
            // The accent is the same one the transmitter uses, because it means
            // the same thing: what happens here goes out on the air.
            let held = chip::accent(ui, "AT THE RADIO");
            if held.clicked() {
                action = Some(ControlAction::Release);
            }
            held.on_hover_text("You are working this radio. Click to hand it over.");
        } else {
            let at_the_radio = match &status.holder {
                Some(h) => h.label(),
                // Nobody is working it, which on a shared station is the usual
                // state: everybody listening, the key free for the asking.
                None => "nobody".to_string(),
            };
            let waiting = status.i_am_waiting();
            let label = if waiting { "ASKED".to_string() } else { "LISTENING".to_string() };
            let ask = chip::plain(ui, waiting, label);
            if ask.clicked() && !waiting {
                action = Some(ControlAction::Request);
            }
            ask.on_hover_text(if waiting {
                format!("{at_the_radio} has been asked for the radio.")
            } else {
                format!("{at_the_radio} is working this radio. Click to ask for it.")
            });
        }

        // Everybody else on the radio, named. Worth the room: an operator who
        // can see that two other people are listening reads a quiet band
        // differently from one who thinks they are alone with it.
        let others: Vec<String> =
            status.clients.iter().filter(|c| c.slot != status.me).map(|c| c.label()).collect();
        if !others.is_empty() {
            ui.label(
                RichText::new(format!("also here: {}", others.join(", ")))
                    .color(theme::TEXT())
                    .size(11.0),
            );
        }
    });

    // The holder's side of a handover: who is asking, and yes or no. Below the
    // chip rather than in a modal, because the operator may well be in the
    // middle of a contact and a dialog over the panadapter is the last thing
    // they need — the request is not urgent and does not expire.
    if status.i_hold() {
        for asker in status.waiting.iter() {
            if let Some(a) = request_row(ui, asker) {
                action = Some(a);
            }
        }
    }
    action
}

/// One "DL2ABC would like the radio" line, with its two answers.
fn request_row(ui: &mut egui::Ui, asker: &ClientInfo) -> Option<ControlAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} would like the radio", asker.label())).size(11.0));
        if chip::plain(ui, false, "HAND OVER").clicked() {
            action = Some(ControlAction::Grant(asker.slot));
        }
        if chip::plain(ui, false, "NOT NOW").clicked() {
            action = Some(ControlAction::Deny(asker.slot));
        }
    });
    action
}

/// The two chip shapes this module uses, named so the paint code above reads as
/// what it means rather than as a call with four positional arguments.
mod chip {
    use super::*;

    pub(super) fn plain(
        ui: &mut egui::Ui,
        selected: bool,
        text: impl Into<RichText>,
    ) -> egui::Response {
        chrome::chip(ui, selected, text)
    }

    /// The hazard accent, not the interface cyan: this chip says the
    /// transmitter answers to *this* screen, which is the same warning the PTT
    /// controls carry.
    pub(super) fn accent(ui: &mut egui::Ui, text: impl Into<RichText>) -> egui::Response {
        chrome::chip_accent(ui, true, text, theme::HAZARD(), theme::INK_ON_BRIGHT())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(slot: u64, name: &str) -> ClientInfo {
        ClientInfo { slot, name: name.into(), may_transmit: true }
    }

    /// A station with one client on it says nothing at all — the case that
    /// covers every station that predates sharing, and most that do not.
    #[test]
    fn one_client_is_not_worth_a_chip() {
        let alone = ControlStatus {
            me: 1,
            holder: Some(client(1, "oe1test")),
            clients: vec![client(1, "oe1test")],
            waiting: Vec::new(),
        };
        assert!(!alone.shared());
        assert!(alone.i_hold());
        assert!(alone.may_transmit());
    }

    #[test]
    fn a_listener_knows_it_is_listening() {
        let listening = ControlStatus {
            me: 2,
            holder: Some(client(1, "oe1test")),
            clients: vec![client(1, "oe1test"), client(2, "dl2abc")],
            waiting: Vec::new(),
        };
        assert!(listening.shared());
        assert!(!listening.i_hold());
        assert!(!listening.may_transmit(), "a listener must not be offered a PTT button");
        assert!(!listening.i_am_waiting());
    }

    /// Holding the key is not permission to transmit. A receive-only sign-in can
    /// work the radio and must still not be given a PTT button.
    #[test]
    fn the_key_is_not_a_licence() {
        let rx_only = ControlStatus {
            me: 1,
            holder: Some(ClientInfo { slot: 1, name: "guest".into(), may_transmit: false }),
            clients: vec![
                ClientInfo { slot: 1, name: "guest".into(), may_transmit: false },
                client(2, "oe1test"),
            ],
            waiting: Vec::new(),
        };
        assert!(rx_only.i_hold());
        assert!(!rx_only.may_transmit());
    }

    #[test]
    fn an_asker_sees_its_own_request() {
        let asked = ControlStatus {
            me: 2,
            holder: Some(client(1, "oe1test")),
            clients: vec![client(1, "oe1test"), client(2, "dl2abc")],
            waiting: vec![client(2, "dl2abc")],
        };
        assert!(asked.i_am_waiting());
        assert!(!asked.i_hold());
    }

    /// A station with one shared password cannot tell its clients apart, and the
    /// interface says so rather than inventing a name.
    #[test]
    fn a_nameless_client_is_still_something_to_point_at() {
        assert_eq!(client(3, "").label(), "another client");
        assert_eq!(client(3, "oe1test").label(), "oe1test");
    }
}
