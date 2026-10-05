//! Client-side state for the NOAA APT panel: the pass being received, the
//! passes already on the disk, and the schedule of ones still to come.
//!
//! The schedule is not computed here. It arrives as a
//! [`sdroxide_types::WxSchedStatus`] from the engine host, because that is the
//! machine that owns the element sets, the clock and the receiver — and because
//! a schedule that only ran while a browser tab was open would not be a
//! schedule. This holds the answer and the tick the operator is about to send.

use eframe::egui;
use sdroxide_types::{AptStatus, WxSchedConfig, WxSchedStatus};

const CHANNEL_PIXELS: usize = 909;
const GROW_ROWS: usize = 256;

/// Passes this client will hold thumbnails for.
///
/// Not a limit on the collection — the engine counts the whole store and the
/// panel says how much more there is. Three birds four times a day is a
/// thousand pictures a year, and a thumbnail is a few tens of kilobytes where
/// the pass behind it is two megapixels.
pub const GALLERY_MAX: usize = 240;

/// A saved pass in the gallery.
///
/// What is held is the thumbnail; the picture itself lives in the engine's
/// store and is fetched only when one is opened.
pub struct SavedPass {
    /// Thumbnail, as the engine rendered it out of the stored file.
    pub texture: egui::TextureHandle,
    /// The pass at full size, once fetched.
    pub full: Option<egui::TextureHandle>,
    /// File name it was saved under, which carries its date and its bird.
    pub name: String,
    /// Size of the picture itself, not of the thumbnail.
    pub size: (u16, u16),
    /// When it was received, as the engine ordered the store by.
    pub unix: i64,
}

impl SavedPass {
    /// The bird's name, read back out of the file name, or the name itself.
    ///
    /// `apt-<millis>-NOAA_19.png` is what a scheduled pass is written as — see
    /// [`sdroxide_types::pass_stem`] — and the label is the only thing in it
    /// that says which of three identical-looking pictures this is.
    pub fn bird(&self) -> Option<String> {
        let rest = self.name.strip_prefix("apt-")?;
        let stem = rest.strip_suffix(".png").or_else(|| rest.strip_suffix(".PNG"))?;
        let (_, label) = stem.split_once('-')?;
        // A second picture from the same pass carries a counter; the bird is
        // what comes before it.
        let label = label.rsplit_once('-').map_or(label, |(bird, n)| {
            if n.chars().all(|c| c.is_ascii_digit()) { bird } else { label }
        });
        let label = label.replace('_', " ");
        (!label.trim().is_empty()).then_some(label)
    }

    /// The audio file that would have been recorded alongside, if any was.
    pub fn wav_name(&self) -> Option<String> {
        let stem = self.name.strip_suffix(".png").or_else(|| self.name.strip_suffix(".PNG"))?;
        Some(format!("{stem}.wav"))
    }
}

pub struct AptUi {
    pub status: AptStatus,
    image_id: u32,
    a: Vec<u8>,
    b: Vec<u8>,
    h_a: u16,
    h_b: u16,
    tex_a: Option<egui::TextureHandle>,
    tex_b: Option<egui::TextureHandle>,
    dirty: bool,

    // ── The store ──
    /// Saved passes, newest first.
    pub gallery: Vec<SavedPass>,
    /// Whether the engine's store has been listed this session.
    pub listed: bool,
    /// How many passes the store holds altogether.
    pub total: u32,
    /// True while a listing page is outstanding, so a click cannot spray
    /// requests down the one reliable lane.
    pub page_pending: bool,
    /// Name of the pass whose full size has been asked for. Records the *ask*,
    /// so a picture that fails to come back is asked for once rather than once
    /// a frame.
    pub full_asked: Option<String>,
    /// That ask came back empty — the store no longer has it.
    pub full_gone: bool,
    /// Bytes of the picture last fetched, and its name: what the PNG button
    /// writes out. One at a time, because a pass is two megapixels.
    pub full_png: Option<(String, Vec<u8>)>,
    /// Where passes are being saved, for the panel to show.
    pub dir: String,
    /// Which gallery entry is open full-size, if any.
    pub viewing: Option<usize>,
    /// Name of the picture whose DELETE has been pressed once and is waiting to
    /// be pressed again. Two presses rather than a modal, because the file goes
    /// for good and a pass cannot be asked to come round again.
    pub confirm_delete: Option<String>,

    // ── The schedule ──
    /// The scheduler as the engine last reported it.
    pub sched: WxSchedStatus,
    /// Whether that has arrived at all. Until it has, the panel must not show
    /// the editor: the defaults it would seed from are not the station's, and
    /// applying them would write them over the real ones.
    pub sched_seen: bool,
    /// The filter being edited. Separate from `sched.cfg` so a slider the
    /// operator is dragging is not yanked back by the next status.
    pub edit: WxSchedConfig,
    /// Name of the recorded audio asked for, and the answer once it lands.
    pub wav_asked: Option<String>,
    pub wav: Option<(String, Vec<u8>)>,
    /// The audio ask came back empty — nothing was recorded, or it has gone.
    pub wav_gone: bool,
    /// Whether the schedule window is open. Not persisted: it is a window the
    /// operator opens to plan a night and closes again, not a layout choice.
    pub sched_open: bool,
}

impl Default for AptUi {
    fn default() -> Self {
        AptUi {
            status: AptStatus::default(),
            image_id: 0,
            a: Vec::new(),
            b: Vec::new(),
            h_a: 0,
            h_b: 0,
            tex_a: None,
            tex_b: None,
            dirty: false,
            gallery: Vec::new(),
            listed: false,
            total: 0,
            page_pending: false,
            full_asked: None,
            full_gone: false,
            full_png: None,
            dir: String::new(),
            viewing: None,
            confirm_delete: None,
            sched: WxSchedStatus::default(),
            sched_seen: false,
            edit: WxSchedConfig::default(),
            wav_asked: None,
            wav: None,
            wav_gone: false,
            sched_open: false,
        }
    }
}

impl AptUi {
    pub fn on_line(&mut self, image_id: u32, channel: u8, y: u16, gray: Vec<u8>) {
        if image_id != self.image_id {
            self.image_id = image_id;
            self.a.clear();
            self.b.clear();
            self.h_a = 0;
            self.h_b = 0;
        }
        if gray.len() != CHANNEL_PIXELS {
            return;
        }
        let (buf, h) =
            if channel == 0 { (&mut self.a, &mut self.h_a) } else { (&mut self.b, &mut self.h_b) };
        if y as usize != *h as usize {
            return;
        }
        if buf.capacity() < buf.len() + CHANNEL_PIXELS {
            buf.reserve(CHANNEL_PIXELS * GROW_ROWS);
        }
        buf.extend_from_slice(&gray);
        *h = h.saturating_add(1);
        self.dirty = true;
    }

    pub fn on_image(&mut self) {
        self.dirty = true;
    }

    pub fn textures(
        &mut self,
        ctx: &egui::Context,
    ) -> (Option<egui::TextureHandle>, Option<egui::TextureHandle>) {
        if self.dirty {
            self.tex_a = make_tex(ctx, "apt-a", &self.a, self.h_a);
            self.tex_b = make_tex(ctx, "apt-b", &self.b, self.h_b);
            self.dirty = false;
        }
        (self.tex_a.clone(), self.tex_b.clone())
    }

    // ── The store ──

    /// One page of the engine's pass store.
    pub fn on_listing(&mut self, listing: sdroxide_types::ImageListing, ctx: &egui::Context) {
        self.page_pending = false;
        self.total = listing.total;
        self.dir = listing.dir;
        for entry in listing.entries {
            self.insert_entry(entry, ctx);
        }
    }

    /// Whether more of the store can still be paged in.
    pub fn can_page(&self) -> bool {
        self.gallery.len() < GALLERY_MAX
    }

    /// A pass the engine has just stored.
    pub fn on_saved(&mut self, entry: sdroxide_types::ImageEntry, ctx: &egui::Context) {
        self.total += 1;
        self.insert_entry(entry, ctx);
    }

    /// A pass is no longer in the store: this screen's delete, or another's.
    ///
    /// The viewer is an index into the gallery, so it moves with it. Deleting
    /// the picture on screen leaves the viewer on the next-older one, which is
    /// what makes culling a night of blank passes a sequence of clicks rather
    /// than a reopen after each.
    pub fn on_deleted(&mut self, name: &str) {
        let Some(at) = self.gallery.iter().position(|c| c.name == name) else { return };
        self.gallery.remove(at);
        // `total` counts the whole store, of which this gallery is a window. It
        // comes down for a picture that was in the window, because that is the
        // case where this client knows the store really shrank.
        self.total = self.total.saturating_sub(1);
        if self.full_png.as_ref().is_some_and(|(n, _)| n == name) {
            self.full_png = None;
        }
        if self.confirm_delete.as_deref() == Some(name) {
            self.confirm_delete = None;
        }
        if let Some(v) = self.viewing {
            let next = if at < v { v - 1 } else { v };
            self.viewing = (next < self.gallery.len()).then_some(next);
            // Only a delete of the picture being viewed puts a different one in
            // the window; anything above it just shifted the same one up.
            if at == v {
                self.full_asked = None;
                self.full_gone = false;
                self.wav_asked = None;
                self.wav = None;
                self.wav_gone = false;
            }
        }
    }

    /// A fetched full-size pass. An empty answer means the store no longer has
    /// it — the file moved or was deleted between listing and opening.
    pub fn on_file(&mut self, name: &str, png: &[u8], ctx: &egui::Context) {
        let Some((gray, w, h)) = crate::wefax::decode_gray(png) else {
            if self.full_asked.as_deref() == Some(name) {
                self.full_gone = true;
            }
            return;
        };
        let tex = ctx.load_texture(
            format!("apt-{name}"),
            crate::wefax::gray_image(&gray, w, h),
            egui::TextureOptions::LINEAR,
        );
        if let Some(c) = self.gallery.iter_mut().find(|c| c.name == name) {
            c.full = Some(tex);
        }
        self.full_png = Some((name.to_string(), png.to_vec()));
    }

    fn insert_entry(&mut self, entry: sdroxide_types::ImageEntry, ctx: &egui::Context) {
        if self.gallery.iter().any(|c| c.name == entry.name) {
            return;
        }
        let Some((gray, tw, th)) = crate::wefax::decode_gray(&entry.thumb) else { return };
        let texture = ctx.load_texture(
            format!("apt-thumb-{}", entry.name),
            crate::wefax::gray_image(&gray, tw, th),
            egui::TextureOptions::LINEAR,
        );
        let at = self.gallery.partition_point(|c| c.unix > entry.unix);
        self.gallery.insert(
            at,
            SavedPass {
                texture,
                full: None,
                size: (entry.width, entry.height),
                unix: entry.unix,
                name: entry.name,
            },
        );
        // The entry the viewer is on has just moved down if this went above it.
        if let Some(v) = self.viewing.as_mut() {
            if at <= *v {
                *v += 1;
            }
        }
        self.gallery.truncate(GALLERY_MAX);
    }

    // ── The schedule ──

    /// Adopt a schedule from the engine.
    ///
    /// The filter is copied into the editor only the first time, for the reason
    /// every other engine-owned editor in this program does it: later edits are
    /// the operator's, and overwriting them on every status would make a slider
    /// impossible to drag.
    pub fn on_sched(&mut self, status: WxSchedStatus) {
        if !self.sched_seen {
            self.edit = status.cfg.clone();
            self.sched_seen = true;
        }
        self.sched = status;
    }

    /// A fetched pass recording. Empty means nothing was kept, or it has gone.
    pub fn on_wav(&mut self, name: &str, wav: Vec<u8>) {
        if self.wav_asked.as_deref() != Some(name) {
            return;
        }
        if wav.is_empty() {
            self.wav_gone = true;
            return;
        }
        self.wav_gone = false;
        self.wav = Some((name.to_string(), wav));
    }

    /// Whether the editor differs from what the engine is running on.
    pub fn sched_edited(&self) -> bool {
        self.sched_seen && self.edit != self.sched.cfg
    }
}

fn make_tex(ctx: &egui::Context, name: &str, gray: &[u8], h: u16) -> Option<egui::TextureHandle> {
    if h == 0 || gray.len() < CHANNEL_PIXELS {
        return None;
    }
    let w = CHANNEL_PIXELS;
    let mut rgba = vec![0u8; w * h as usize * 4];
    for (i, &g) in gray.iter().take(w * h as usize).enumerate() {
        let o = i * 4;
        rgba[o] = g;
        rgba[o + 1] = g;
        rgba[o + 2] = g;
        rgba[o + 3] = 255;
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([w, h as usize], &rgba);
    Some(ctx.load_texture(name, img, egui::TextureOptions::NEAREST))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot_png() -> Vec<u8> {
        let img = image::GrayImage::from_pixel(2, 2, image::Luma([128]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode");
        buf.into_inner()
    }

    fn entry(name: &str, unix: i64) -> sdroxide_types::ImageEntry {
        sdroxide_types::ImageEntry {
            kind: sdroxide_types::ImageKind::Apt,
            name: name.to_string(),
            unix,
            width: 1818,
            height: 800,
            bytes: 64,
            thumb: dot_png(),
            rifp: None,
        }
    }

    /// The file name is the only thing that says which bird a picture is of —
    /// three NOAA passes look identical in a thumbnail strip.
    #[test]
    fn a_stored_name_says_which_bird_and_which_audio_goes_with_it() {
        let p = SavedPass {
            texture: egui::Context::default().load_texture(
                "t",
                egui::ColorImage::filled([1, 1], egui::Color32::BLACK),
                Default::default(),
            ),
            full: None,
            name: "apt-1753795200000-NOAA_19.png".into(),
            size: (1818, 800),
            unix: 1_753_795_200,
        };
        assert_eq!(p.bird().as_deref(), Some("NOAA 19"));
        assert_eq!(p.wav_name().as_deref(), Some("apt-1753795200000-NOAA_19.wav"));

        // The second picture of a pass carries a counter, which is not the bird.
        let second = SavedPass { name: "apt-1753795200000-NOAA_19-2.png".into(), ..p };
        assert_eq!(second.bird().as_deref(), Some("NOAA 19"));

        // A hand-pressed picture has no bird in its name, and says so rather
        // than inventing one.
        let plain = SavedPass { name: "apt-1753795200000.png".into(), ..second };
        assert_eq!(plain.bird(), None);
    }

    /// The viewer is an index into the gallery, so a deletion has to move it.
    /// An index left where it was means the operator deletes one pass and the
    /// window jumps to a different one — or the next DELETE is aimed at
    /// whatever slid into the gap.
    #[test]
    fn deleting_a_pass_carries_the_viewer_to_the_next_one() {
        let ctx = egui::Context::default();
        let mut ui = AptUi::default();
        for i in 0..4i64 {
            ui.on_saved(entry(&format!("apt-{}000-NOAA_19.png", 1_700_000 + i), 1_000 + i), &ctx);
        }
        assert_eq!((ui.gallery.len(), ui.total), (4, 4));
        let names: Vec<String> = ui.gallery.iter().map(|c| c.name.clone()).collect();

        // Deleting above the viewer shifts the same pass up under it.
        ui.viewing = Some(2);
        ui.on_deleted(&names[0]);
        assert_eq!(ui.viewing, Some(1));
        assert_eq!(ui.gallery[1].name, names[2], "the viewer must stay on its pass");
        assert_eq!(ui.total, 3);

        // Deleting the pass being viewed lands on the next-older one.
        ui.on_deleted(&names[2]);
        assert_eq!(ui.viewing, Some(1));
        assert_eq!(ui.gallery[1].name, names[3]);

        // Deleting the last one has nowhere to go, so the window closes.
        ui.on_deleted(&names[3]);
        assert_eq!(ui.viewing, None);

        // A name this client is not holding changes nothing, `total` included.
        ui.total = 40;
        ui.on_deleted("apt-99990101.png");
        assert_eq!((ui.gallery.len(), ui.total), (1, 40));
    }

    /// The bug this guards: the filter arrives from the engine several times a
    /// minute. Seeding the editor from every one of them would snap a slider
    /// back under the operator's finger.
    #[test]
    fn the_filter_editor_is_seeded_once_and_then_left_alone() {
        let mut ui = AptUi::default();
        assert!(!ui.sched_seen);
        ui.on_sched(WxSchedStatus {
            cfg: WxSchedConfig { horizon_h: 48, min_max_el: 30.0, ..Default::default() },
            ..Default::default()
        });
        assert!(ui.sched_seen);
        assert_eq!(ui.edit.horizon_h, 48, "seeded from the station, not from a default");
        assert!(!ui.sched_edited());

        ui.edit.min_max_el = 15.0;
        assert!(ui.sched_edited());
        // A fresh status while the operator is mid-edit leaves the editor be.
        ui.on_sched(WxSchedStatus {
            cfg: WxSchedConfig { horizon_h: 48, min_max_el: 30.0, ..Default::default() },
            ..Default::default()
        });
        assert_eq!(ui.edit.min_max_el, 15.0);
        assert!(ui.sched_edited());
    }

    /// An audio answer for something else must not land in the slot the panel
    /// is showing, and an empty one is "there is none" rather than silence.
    #[test]
    fn a_pass_recording_only_lands_where_it_was_asked_for() {
        let mut ui = AptUi::default();
        ui.wav_asked = Some("apt-1.wav".into());

        ui.on_wav("apt-2.wav", vec![1, 2, 3]);
        assert!(ui.wav.is_none(), "an answer to somebody else's ask");
        assert!(!ui.wav_gone);

        ui.on_wav("apt-1.wav", Vec::new());
        assert!(ui.wav_gone, "an empty answer has to stop the waiting");
        assert!(ui.wav.is_none());

        ui.on_wav("apt-1.wav", vec![4, 5]);
        assert!(!ui.wav_gone);
        assert_eq!(ui.wav.as_ref().map(|(n, w)| (n.as_str(), w.len())), Some(("apt-1.wav", 2)));
    }
}
