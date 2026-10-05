//! NOAA APT panel: two live channels (visible / IR), the passes already saved,
//! and the schedule of ones still to come.
//!
//! The schedule is the part that makes the mode usable. A pass is fifteen
//! minutes long, four times a day per bird, and cannot be asked to come round
//! again — so the useful question is not "record now" but "record the 20:41
//! NOAA 19 tonight", which is what the SCHEDULE window asks. The ticking is
//! sent to the engine host; nothing here has to stay open for it to happen.

use eframe::egui::{self, RichText};
use sdroxide_types::Command;

use crate::app::SdroxideApp;

/// Width the gallery strip takes, as a fraction of the panel.
const GALLERY_FRACTION: f32 = 0.24;

impl SdroxideApp {
    pub(in crate::app) fn apt_panel(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        panel_h: f32,
    ) {
        use crate::theme;

        let st = self.apt.status;
        let ctx = ui.ctx().clone();
        crate::repaint::after_ms(&ctx, 200);
        // The pictures are the engine's; list its store once when the panel
        // first opens. Passes that arrive afterwards are announced one at a
        // time. The schedule is asked for on the same terms — a client that
        // attached before the engine last reported one would otherwise show an
        // empty table for up to a minute.
        if !self.apt.listed {
            self.apt.listed = true;
            self.apt.page_pending = true;
            cmds.push(Command::ImageList {
                kind: sdroxide_types::ImageKind::Apt,
                offset: 0,
                count: sdroxide_types::IMAGE_PAGE_MAX,
            });
            cmds.push(Command::RefreshWxSched);
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("APT").size(12.0).strong().color(theme::CYAN()));
            let (face, hint) = if st.receiving {
                (" ■ STOP ", "End the pass now and save the picture")
            } else {
                (" ● START ", "Start a picture now, without waiting for sync")
            };
            if crate::chrome::chip_accent(
                ui,
                st.receiving,
                RichText::new(face).strong(),
                if st.receiving { theme::PINK() } else { theme::GREEN() },
                theme::INK_ON_CYAN(),
            )
            .on_hover_text(hint)
            .clicked()
            {
                cmds.push(if st.receiving { Command::AptStop } else { Command::AptStart });
            }

            // The scheduler's own chip, and what it is up to, because the
            // window it opens is where the automatic half of this mode lives.
            let armed = self.apt.sched.passes.iter().filter(|p| p.armed).count();
            let busy = self.apt.sched.recording.is_some();
            let face = match (busy, armed) {
                (true, _) => " SCHEDULE · recording ".to_string(),
                (false, 0) => " SCHEDULE ".to_string(),
                (false, n) => format!(" SCHEDULE · {n} armed "),
            };
            if crate::chrome::chip_accent(
                ui,
                self.apt.sched_open || busy,
                RichText::new(face).size(11.0),
                if busy { theme::PINK() } else { theme::CYAN() },
                theme::INK_ON_CYAN(),
            )
            .on_hover_text(
                "Upcoming passes, and which of them the station records on its own.\n\
                 The ticks live on the radio's machine — this window does not have to stay open.",
            )
            .clicked()
            {
                self.apt.sched_open = !self.apt.sched_open;
            }

            let hint = if st.receiving {
                format!("A {} · B {} lines", st.lines_a, st.lines_b)
            } else {
                "waiting for NOAA APT sync".to_string()
            };
            ui.label(RichText::new(hint).size(11.0).color(theme::CYAN_DIM()));
        });

        ui.add_space(6.0);
        let avail = ui.available_width();
        let img_h = (panel_h - 48.0).max(80.0);
        let pane = self.phone_pane(ui, self.state.rx[0].mode);
        let gap = ui.spacing().item_spacing.x;
        // On a phone the two halves take turns; on a screen the gallery takes a
        // strip down the side, as it does for the charts.
        let (live_w, gallery_w) = match pane {
            Some(0) => (avail, 0.0),
            Some(_) => (0.0, avail),
            None => {
                let g = (avail * GALLERY_FRACTION).clamp(120.0, (avail - 240.0).max(120.0));
                ((avail - g - gap).max(160.0), g)
            }
        };
        ui.horizontal_top(|ui| {
            if pane.is_none_or(|p| p == 0) {
                ui.allocate_ui_with_layout(
                    egui::vec2(live_w, img_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let (tex_a, tex_b) = self.apt.textures(ui.ctx());
                        let col_w = ((live_w - 8.0) * 0.5).max(80.0);
                        ui.horizontal(|ui| {
                            channel_col(ui, "A visible", tex_a.as_ref(), col_w, img_h);
                            ui.add_space(8.0);
                            channel_col(ui, "B IR", tex_b.as_ref(), col_w, img_h);
                        });
                    },
                );
            }
            if pane.is_none_or(|p| p != 0) {
                ui.allocate_ui_with_layout(
                    egui::vec2(gallery_w, img_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_max_width(gallery_w);
                        self.apt_gallery(ui, gallery_w, cmds);
                    },
                );
            }
        });

        self.apt_schedule_window(&ctx, cmds);
        self.apt_viewer(&ctx, cmds);
    }

    /// The gallery of saved passes: a thumbnail each, labelled with the bird and
    /// the time, and clickable to open it full size.
    ///
    /// The label carries the work: a night of NOAA passes is a strip of
    /// near-identical grey ribbons, and telling last night's 19 from this
    /// morning's 15 without a name on it means opening them one at a time.
    fn apt_gallery(&mut self, ui: &mut egui::Ui, width: f32, cmds: &mut Vec<Command>) {
        use crate::theme;

        let dir = self.apt.dir.clone();
        let where_ = self.store_where(&dir);
        ui.horizontal(|ui| {
            ui.label(RichText::new("SAVED").color(theme::CYAN_DIM()).size(9.5).strong());
            if self.apt.total > 0 {
                ui.label(
                    RichText::new(format!("{}", self.apt.total)).color(theme::LINE_LIT()).size(9.5),
                );
            }
            if !dir.is_empty()
                && crate::chrome::chip(ui, false, RichText::new("PATH").size(9.5))
                    .on_hover_text(format!("Passes are saved {where_}\n\nClick to copy the path"))
                    .clicked()
            {
                ui.ctx().copy_text(dir.clone());
            }
        });

        if self.apt.gallery.is_empty() {
            ui.label(
                RichText::new(if self.apt.page_pending {
                    "Reading the radio's pictures…".to_string()
                } else if dir.is_empty() {
                    "Recorded passes collect here.".to_string()
                } else {
                    format!("Recorded passes are saved {where_} and collect here.")
                })
                .color(theme::LINE_LIT())
                .size(10.0),
            );
            return;
        }

        let thumb_w = (width - 20.0).max(60.0);
        // A pass is about twice as tall as it is wide — two 909-pixel channels
        // side by side against a quarter of an hour of lines — so a thumbnail
        // that keeps its shape is roughly twice its width. Capped, because a
        // full fifteen-minute pass would otherwise be one card per screen.
        let thumb_h = (thumb_w * 1.4).min(200.0);
        let mut open = None;
        let mut more = false;
        // By name, not by index: the answer comes back asynchronously and a
        // pass may have arrived at the front of the gallery by then.
        let mut delete: Option<String> = None;
        egui::ScrollArea::vertical().id_salt("apt-gallery").auto_shrink([false, false]).show(
            ui,
            |ui| {
                for (i, c) in self.apt.gallery.iter().enumerate() {
                    let selected = self.apt.viewing == Some(i);
                    let card = ui.scope_builder(
                        egui::UiBuilder::new().sense(egui::Sense::click()),
                        |ui| {
                            ui.set_width(thumb_w);
                            ui.add(
                                egui::Image::new(&c.texture)
                                    .fit_to_exact_size(egui::vec2(thumb_w, thumb_h))
                                    .maintain_aspect_ratio(true),
                            );
                            ui.label(
                                RichText::new(hhmm_day(c.unix))
                                    .color(if selected { theme::CYAN() } else { theme::TEXT() })
                                    .size(10.0)
                                    .monospace(),
                            );
                            // A picture started by hand carries no bird in its
                            // name; its size is at least true.
                            ui.label(
                                RichText::new(
                                    c.bird()
                                        .unwrap_or_else(|| format!("{} × {}", c.size.0, c.size.1)),
                                )
                                .color(theme::CYAN_DIM())
                                .size(9.5),
                            );
                        },
                    );
                    let resp = card.response;
                    if selected || resp.hovered() {
                        ui.painter().rect_stroke(
                            resp.rect.expand(2.0),
                            2.0,
                            egui::Stroke::new(
                                1.0,
                                if selected { theme::CYAN() } else { theme::LINE_LIT() },
                            ),
                            egui::StrokeKind::Inside,
                        );
                    }
                    let resp = resp.on_hover_text(format!(
                        "{}\n{} × {} pixels\n{}\n\nRight-click to delete",
                        hhmm_day(c.unix),
                        c.size.0,
                        c.size.1,
                        c.name
                    ));
                    if resp.clicked() {
                        open = Some(i);
                    }
                    // A menu rather than a button on the card: the whole card is
                    // the target for opening it, and a delete sharing that
                    // target would be pressed by accident.
                    resp.context_menu(|ui| {
                        ui.label(RichText::new(&c.name).color(theme::CYAN_DIM()).size(10.0));
                        if ui.button("Delete this pass").clicked() {
                            delete = Some(c.name.clone());
                            ui.close();
                        }
                    });
                    ui.add_space(6.0);
                }
                let older = self.apt.total.saturating_sub(self.apt.gallery.len() as u32);
                if older > 0 && !self.apt.can_page() {
                    // Not a collection that has been lost — the store still has
                    // them and this says how many. This client simply stops
                    // holding thumbnails somewhere.
                    ui.label(
                        RichText::new(format!("{older} older in the store"))
                            .color(theme::LINE_LIT())
                            .size(9.5),
                    );
                } else if older > 0 {
                    let label = if self.apt.page_pending {
                        "loading…".to_string()
                    } else {
                        format!("{older} older — load more")
                    };
                    if crate::chrome::chip(ui, false, RichText::new(label).size(9.5)).clicked()
                        && !self.apt.page_pending
                    {
                        more = true;
                    }
                }
            },
        );
        if open.is_some() {
            self.apt.viewing = open;
        }
        if more {
            self.apt.page_pending = true;
            cmds.push(Command::ImageList {
                kind: sdroxide_types::ImageKind::Apt,
                offset: self.apt.gallery.len() as u32,
                count: sdroxide_types::IMAGE_PAGE_MAX,
            });
        }
        // The card goes when the engine says the file has, not here: the store
        // is on the radio's machine, and a thumbnail that vanished from a delete
        // that then failed would be a lie.
        if let Some(name) = delete {
            cmds.push(Command::ImageDelete { kind: sdroxide_types::ImageKind::Apt, name });
        }
    }

    /// A saved pass, full size, in its own window, with both artefacts to hand.
    ///
    /// The WAV is the one that matters for real work: WXtoImg wants the
    /// discriminator audio, not a decoded picture, and without it the pass
    /// cannot be reprocessed — no map overlay, no false colour, no temperature
    /// calibration. The PNG is the fast look that says whether it is worth it.
    fn apt_viewer(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let Some(i) = self.apt.viewing else { return };
        let n = self.apt.gallery.len();
        let Some(pass) = self.apt.gallery.get(i) else {
            self.apt.viewing = None;
            return;
        };
        // Two megapixels of picture live in the engine's store; the thumbnail
        // stands in until the real one arrives, so the window opens on something
        // rather than on nothing. Asked once per pass, whatever the answer — a
        // fetch that fails must not become a request every frame.
        if pass.full.is_none() && self.apt.full_asked.as_deref() != Some(&pass.name) {
            self.apt.full_asked = Some(pass.name.clone());
            self.apt.full_gone = false;
            cmds.push(Command::ImageGet {
                kind: sdroxide_types::ImageKind::Apt,
                name: pass.name.clone(),
            });
        }
        let pass = &self.apt.gallery[i];
        let (name, size) = (pass.name.clone(), pass.size);
        let title = match pass.bird() {
            Some(bird) => format!("{bird} · {}", hhmm_day(pass.unix)),
            None => hhmm_day(pass.unix),
        };
        let wav_name = pass.wav_name();
        let loaded = pass.full.is_some();
        let tex = pass.full.clone().unwrap_or_else(|| pass.texture.clone());
        let savable = self.apt.full_png.as_ref().is_some_and(|(n, _)| *n == name);
        let gone = self.apt.full_gone;
        let armed = self.apt.confirm_delete.as_deref() == Some(name.as_str());
        let del_hint = if self.apt.dir.is_empty() {
            "Delete this pass from the store".to_string()
        } else {
            format!("Delete this pass {}", self.store_where(&self.apt.dir))
        };
        // Which of the three states the audio is in for *this* pass: not asked
        // for, waiting, or here. The engine answers an ask that found nothing
        // with an empty file, so "waiting" cannot be left to time out.
        let wav_here = match (&wav_name, &self.apt.wav) {
            (Some(w), Some((n, _))) => n == w,
            _ => false,
        };
        let wav_waiting = !wav_here
            && !self.apt.wav_gone
            && match (&wav_name, &self.apt.wav_asked) {
                (Some(w), Some(a)) => a == w,
                _ => false,
            };
        let wav_gone = self.apt.wav_gone && self.apt.wav_asked.as_deref() == wav_name.as_deref();

        let mut open = true;
        let mut save_png = false;
        let mut want_wav = false;
        let mut save_wav = false;
        let mut pressed_delete = false;
        let mut step = 0i32;
        let resp = egui::Window::new(format!("{title}  ·  {}×{}", size.0, size.1))
            .id(crate::layout::salted_id(ctx, "apt-viewer"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .default_size([
                crate::layout::window_w(ctx, 760.0),
                crate::layout::window_h(ctx, 700.0),
            ])
            .show(ctx, |ui| {
                use crate::theme;
                crate::chrome::window_body_bg(ui);
                ui.horizontal_wrapped(|ui| {
                    if crate::chrome::chip(ui, false, "◀ NEWER")
                        .on_hover_text("The pass received after this one")
                        .clicked()
                    {
                        step = -1;
                    }
                    ui.label(
                        RichText::new(format!("{} of {n}", i + 1))
                            .color(theme::CYAN_DIM())
                            .size(10.0),
                    );
                    if crate::chrome::chip(ui, false, "OLDER ▶")
                        .on_hover_text("The pass received before this one")
                        .clicked()
                    {
                        step = 1;
                    }
                    ui.add_space(8.0);
                    if gone {
                        ui.label(
                            RichText::new("no longer in the store")
                                .color(theme::YELLOW())
                                .size(10.0),
                        );
                    } else if !loaded {
                        ui.label(
                            RichText::new("loading full size…").color(theme::CYAN_DIM()).size(10.0),
                        );
                    } else if savable
                        && crate::chrome::chip(ui, false, RichText::new("PNG").size(9.5))
                            .on_hover_text("Save the decoded picture on this computer")
                            .clicked()
                    {
                        save_png = true;
                    }

                    // The audio. Worth a sentence of hover text, because "why
                    // would I want a WAV of a satellite" is the whole question
                    // and the answer is the reason the scheduler keeps them.
                    if wav_name.is_some() {
                        if wav_here {
                            if crate::chrome::chip_accent(
                                ui,
                                true,
                                RichText::new("WAV").size(9.5),
                                theme::GREEN(),
                                theme::INK_ON_CYAN(),
                            )
                            .on_hover_text(
                                "Save the recorded audio — this is what WXtoImg reads.\n\
                                 11025 Hz mono, the FM discriminator output.",
                            )
                            .clicked()
                            {
                                save_wav = true;
                            }
                        } else if wav_waiting {
                            ui.label(
                                RichText::new("fetching audio…")
                                    .color(theme::CYAN_DIM())
                                    .size(10.0),
                            );
                        } else if wav_gone {
                            ui.label(
                                RichText::new("no audio kept for this pass")
                                    .color(theme::LINE_LIT())
                                    .size(10.0),
                            )
                            .on_hover_text(
                                "Either the pass was started by hand, or the scheduler was \
                                 set not to keep the audio.",
                            );
                        } else if crate::chrome::chip(ui, false, RichText::new("WAV…").size(9.5))
                            .on_hover_text(
                                "Fetch the recorded audio for WXtoImg.\n\
                                 About twenty megabytes for a full pass.",
                            )
                            .clicked()
                        {
                            want_wav = true;
                        }
                    }

                    // Two presses, the second one red: the file goes from the
                    // radio's disk and a pass cannot be asked to come back.
                    let del = if armed {
                        crate::chrome::chip_accent(
                            ui,
                            true,
                            RichText::new("SURE?").size(9.5),
                            theme::PINK(),
                            theme::INK_ON_CYAN(),
                        )
                        .on_hover_text("Click again to delete this pass for good")
                    } else {
                        crate::chrome::chip(ui, false, RichText::new("DELETE").size(9.5))
                            .on_hover_text(&del_hint)
                    };
                    if del.clicked() {
                        pressed_delete = true;
                    }
                    ui.label(RichText::new(&name).color(theme::LINE_LIT()).size(10.0))
                        .on_hover_text("The file this pass was saved as");
                });
                ui.add_space(4.0);
                egui::ScrollArea::both()
                    .id_salt("apt-viewer-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Sized from the picture, not the texture: a thumbnail
                        // standing in must not shrink the window and then jump.
                        let native = egui::vec2(f32::from(size.0.max(1)), f32::from(size.1.max(1)));
                        ui.add(egui::Image::new(&tex).fit_to_exact_size(native));
                    });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        if save_png {
            if let Some((name, png)) = &self.apt.full_png {
                crate::download::save_as(name, png, crate::download::Mime::Png);
            }
        }
        if save_wav {
            if let Some((name, wav)) = &self.apt.wav {
                crate::download::save_as(name, wav, crate::download::Mime::Wav);
            }
        }
        if want_wav {
            if let Some(w) = wav_name {
                self.apt.wav = None;
                self.apt.wav_gone = false;
                self.apt.wav_asked = Some(w.clone());
                cmds.push(Command::WxAudioGet(w));
            }
        }
        // First press arms the chip, second sends it. The card stays until the
        // engine confirms the file is gone.
        if pressed_delete {
            if armed {
                self.apt.confirm_delete = None;
                cmds.push(Command::ImageDelete {
                    kind: sdroxide_types::ImageKind::Apt,
                    name: name.clone(),
                });
                // The audio goes with the picture. Leaving it would collect
                // twenty megabytes a pass of recordings whose preview the
                // operator has just thrown away, with nothing left in the
                // gallery to delete them from.
                if let Some(w) = self.apt.gallery.get(i).and_then(|p| p.wav_name()) {
                    cmds.push(Command::WxAudioDelete(w));
                }
            } else {
                self.apt.confirm_delete = Some(name.clone());
            }
        }
        if step != 0 && n > 0 {
            self.apt.viewing = Some((i as i32 + step).clamp(0, n as i32 - 1) as usize);
            // Stepping to another pass means the outstanding fetches, if any,
            // are for one nobody is looking at any more — and an armed DELETE is
            // for a pass nobody is looking at any more either.
            self.apt.full_asked = None;
            self.apt.full_gone = false;
            self.apt.confirm_delete = None;
            self.apt.wav = None;
            self.apt.wav_asked = None;
            self.apt.wav_gone = false;
        }
        if !open {
            self.apt.viewing = None;
            self.apt.full_asked = None;
            self.apt.full_gone = false;
            self.apt.confirm_delete = None;
            self.apt.wav = None;
            self.apt.wav_asked = None;
            self.apt.wav_gone = false;
        }
    }

    /// The scheduler: what to look for, and which of the passes found are to be
    /// recorded.
    fn apt_schedule_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        if !self.apt.sched_open {
            return;
        }
        use crate::theme;
        let now = crate::time::now_unix();
        let mut open = true;
        let mut apply = false;
        let mut refresh = false;
        // Collected rather than sent inside the closure: the table borrows the
        // pass list to draw, and a command that changed it mid-draw would be a
        // borrow of `self` inside a borrow of `self`.
        let mut arm: Option<(u64, i64, bool)> = None;
        let mut open_png: Option<String> = None;

        let resp = egui::Window::new("APT schedule")
            .id(crate::layout::salted_id(ctx, "apt-schedule"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .default_size([
                crate::layout::window_w(ctx, 680.0),
                crate::layout::window_h(ctx, 560.0),
            ])
            .show(ctx, |ui| {
                crate::chrome::window_body_bg(ui);
                if !self.apt.sched_seen {
                    ui.label(
                        RichText::new("Asking the station for its schedule…")
                            .color(theme::CYAN_DIM())
                            .size(11.0),
                    );
                    return;
                }

                // ── What to look for ──
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("NEXT").color(theme::CYAN_DIM()).size(9.5).strong());
                    ui.add(
                        egui::DragValue::new(&mut self.apt.edit.horizon_h)
                            .range(
                                sdroxide_types::WX_HORIZON_MIN_H..=sdroxide_types::WX_HORIZON_MAX_H,
                            )
                            .speed(1.0)
                            .suffix(" h"),
                    )
                    .on_hover_text(
                        "How far ahead to list passes. Beyond a week the element sets the \
                         prediction rests on are older than the answer they give.",
                    );
                    ui.add_space(8.0);
                    ui.label(RichText::new("ABOVE").color(theme::CYAN_DIM()).size(9.5).strong());
                    ui.add(
                        egui::DragValue::new(&mut self.apt.edit.min_max_el)
                            .range(0.0..=85.0)
                            .speed(1.0)
                            .suffix("°"),
                    )
                    .on_hover_text(
                        "Only passes that climb at least this high. A pass that barely clears \
                         the horizon is streaks of noise: the signal is in the ground clutter \
                         for most of it.",
                    );
                    ui.add_space(8.0);
                    if crate::chrome::chip(ui, false, RichText::new("REFRESH").size(9.5))
                        .on_hover_text("Work the list out again from the current element sets")
                        .clicked()
                    {
                        refresh = true;
                    }
                });

                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("BIRDS").color(theme::CYAN_DIM()).size(9.5).strong());
                    for (id, name) in sdroxide_types::WX_APT_BIRDS {
                        let on = self.apt.edit.sats.contains(id);
                        if crate::chrome::chip(ui, on, RichText::new(*name).size(10.0)).clicked() {
                            if on {
                                self.apt.edit.sats.retain(|s| s != id);
                            } else {
                                self.apt.edit.sats.push(*id);
                            }
                        }
                    }
                });

                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("PAD").color(theme::CYAN_DIM()).size(9.5).strong());
                    ui.add(
                        egui::DragValue::new(&mut self.apt.edit.lead_s)
                            .range(0..=sdroxide_types::WX_PAD_MAX_S)
                            .speed(5.0)
                            .prefix("−")
                            .suffix(" s"),
                    )
                    .on_hover_text(
                        "Start this long before the bird is due over the horizon, so the radio \
                         is tuned and the decoder hunting before the first sync arrives.",
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.apt.edit.trail_s)
                            .range(0..=sdroxide_types::WX_PAD_MAX_S)
                            .speed(5.0)
                            .prefix("+")
                            .suffix(" s"),
                    )
                    .on_hover_text("Keep recording this long after it sets.");
                    ui.add_space(8.0);
                    if crate::chrome::chip(
                        ui,
                        self.apt.edit.keep_wav,
                        RichText::new("KEEP WAV").size(10.0),
                    )
                    .on_hover_text(
                        "Keep the FM discriminator audio alongside the picture. This is the \
                         only thing WXtoImg can reprocess — map overlays, false colour and \
                         temperature calibration all come from the audio, not the PNG.\n\
                         About twenty megabytes a pass.",
                    )
                    .clicked()
                    {
                        self.apt.edit.keep_wav = !self.apt.edit.keep_wav;
                    }
                    ui.add_space(8.0);
                    if self.apt.sched_edited()
                        && crate::chrome::chip_accent(
                            ui,
                            true,
                            RichText::new(" APPLY ").size(10.0).strong(),
                            theme::GREEN(),
                            theme::INK_ON_CYAN(),
                        )
                        .on_hover_text("Send this to the station and list the passes again")
                        .clicked()
                    {
                        apply = true;
                    }
                });

                if !self.apt.sched.note.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(&self.apt.sched.note).color(theme::YELLOW()).size(10.0));
                }

                ui.add_space(6.0);
                ui.separator();

                // ── The passes ──
                if self.apt.sched.passes.is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "No passes in that window. Widen it, lower the minimum height, or \
                             check that the station has element sets for these birds.",
                        )
                        .color(theme::LINE_LIT())
                        .size(10.5),
                    );
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("apt-schedule-list")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for p in &self.apt.sched.passes {
                            let running =
                                self.apt.sched.recording == Some((p.norad_id, p.aos_unix));
                            ui.horizontal(|ui| {
                                // The tick. Disabled for a pass whose downlink
                                // is unknown, because arming it would be a
                                // promise the engine cannot keep: without a
                                // frequency there is nothing to tune to.
                                let can = p.is_tunable();
                                let mut ticked = p.armed;
                                let tick = ui.add_enabled(
                                    can && !matches!(p.state, sdroxide_types::WxJobState::Done),
                                    egui::Checkbox::without_text(&mut ticked),
                                );
                                let tick = if can {
                                    tick.on_hover_text(if p.armed {
                                        "Armed — the station records this on its own"
                                    } else {
                                        "Record this pass"
                                    })
                                } else {
                                    tick.on_hover_text(
                                        "No downlink frequency known for this bird. Set one in \
                                         the satellite settings, or let the station fetch the \
                                         SatNOGS list.",
                                    )
                                };
                                if tick.clicked() {
                                    arm = Some((p.norad_id, p.aos_unix, !p.armed));
                                }

                                let ink = if running {
                                    theme::PINK()
                                } else if p.armed {
                                    theme::GREEN()
                                } else {
                                    theme::TEXT()
                                };
                                ui.label(
                                    RichText::new(format!("{:<8}", p.name))
                                        .color(ink)
                                        .size(10.5)
                                        .monospace(),
                                );
                                ui.label(
                                    RichText::new(hhmm_day(p.aos_unix))
                                        .color(ink)
                                        .size(10.5)
                                        .monospace(),
                                )
                                .on_hover_text(format!(
                                    "Rises {} at {:.0}°, sets {} at {:.0}°\n{}",
                                    sdroxide_solar::timefmt::ymd_hm(p.aos_unix),
                                    p.rise_az,
                                    sdroxide_solar::timefmt::ymd_hm(p.los_unix),
                                    p.set_az,
                                    if p.downlink_hz > 0.0 {
                                        format!("{:.3} MHz", p.downlink_hz / 1e6)
                                    } else {
                                        "no downlink frequency known".to_string()
                                    }
                                ));
                                ui.label(
                                    RichText::new(format!(
                                        "{:>3} min  {:>2.0}°",
                                        p.duration_s() / 60,
                                        p.max_el
                                    ))
                                    .color(theme::CYAN_DIM())
                                    .size(10.0)
                                    .monospace(),
                                );
                                // Where it goes across the sky, which is what
                                // decides whether the antenna can see it.
                                ui.label(
                                    RichText::new(format!(
                                        "{} → {}",
                                        compass(p.rise_az),
                                        compass(p.set_az)
                                    ))
                                    .color(theme::LINE_LIT())
                                    .size(9.5)
                                    .monospace(),
                                );
                                // State, or how long until it starts.
                                let (face, colour) = match p.state {
                                    sdroxide_types::WxJobState::Recording => {
                                        ("recording".to_string(), theme::PINK())
                                    }
                                    sdroxide_types::WxJobState::Done => {
                                        ("saved".to_string(), theme::GREEN())
                                    }
                                    sdroxide_types::WxJobState::Failed => {
                                        ("failed".to_string(), theme::YELLOW())
                                    }
                                    sdroxide_types::WxJobState::Planned if p.armed => (
                                        format!(
                                            "in {}",
                                            sdroxide_solar::timefmt::age(p.aos_unix - now)
                                        ),
                                        theme::CYAN_DIM(),
                                    ),
                                    sdroxide_types::WxJobState::Planned => {
                                        (String::new(), theme::LINE_LIT())
                                    }
                                };
                                if !face.is_empty() {
                                    let l = ui.label(RichText::new(face).color(colour).size(10.0));
                                    if !p.note.is_empty() {
                                        l.on_hover_text(&p.note);
                                    }
                                }
                                // Straight to the picture it produced, so a
                                // finished row is not a dead end.
                                if !p.png.is_empty()
                                    && crate::chrome::chip(
                                        ui,
                                        false,
                                        RichText::new("OPEN").size(9.0),
                                    )
                                    .on_hover_text(p.png.as_str())
                                    .clicked()
                                {
                                    open_png = Some(p.png.clone());
                                }
                            });
                        }
                    });
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        if !open {
            self.apt.sched_open = false;
        }
        if apply {
            cmds.push(Command::SetWxSchedConfig(self.apt.edit.clone()));
        }
        if refresh {
            cmds.push(Command::RefreshWxSched);
        }
        if let Some((norad_id, aos_unix, on)) = arm {
            cmds.push(Command::ArmWxPass { norad_id, aos_unix, on });
        }
        // A finished row opens the picture it produced, if this client is
        // holding its card. One that is not — a pass older than the thumbnails
        // paged in — leaves the schedule alone rather than opening something
        // else; the gallery's own LOAD MORE is the way to it.
        if let Some(png) = open_png {
            if let Some(at) = self.apt.gallery.iter().position(|c| c.name == png) {
                self.apt.viewing = Some(at);
                self.apt.full_asked = None;
                self.apt.full_gone = false;
                self.apt.wav = None;
                self.apt.wav_asked = None;
                self.apt.wav_gone = false;
            }
        }
    }
}

/// `05 Oct 20:41Z` — a pass is identified by the day and the minute, and the
/// day cannot be left off: a list a week long has four rows an evening.
fn hhmm_day(unix: i64) -> String {
    let (_, _, d, h, mi, _) = sdroxide_types::utc_ymd_hms(unix);
    format!("{d:02} {h:02}:{mi:02}Z")
}

/// A bearing as a point of the compass, which is how an antenna is aimed.
fn compass(az_deg: f32) -> &'static str {
    const POINTS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let i = ((az_deg.rem_euclid(360.0) + 22.5) / 45.0) as usize;
    POINTS[i % 8]
}

fn channel_col(ui: &mut egui::Ui, title: &str, tex: Option<&egui::TextureHandle>, w: f32, h: f32) {
    ui.vertical(|ui| {
        ui.label(RichText::new(title).size(10.0).color(crate::theme::CYAN_DIM()));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, crate::theme::BG_DEEP());
        if let Some(tex) = tex {
            let size = tex.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let draw = size * scale;
            let pos = egui::pos2(rect.center().x - draw.x * 0.5, rect.top());
            ui.painter().image(
                tex.id(),
                egui::Rect::from_min_size(pos, draw),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    });
}
