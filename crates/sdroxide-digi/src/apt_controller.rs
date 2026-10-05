//! `AptController` — NOAA APT pictures, receive only.

use std::time::SystemTime;

use sdroxide_dsp::apt::{AptChannel, AptEvent, AptRx, CHANNEL_PIXELS};
use sdroxide_types::{AptStatus, DigiConfig, DigiStatus, Mode, QsoStep, TranscriptLine};

use crate::DigiEngine;
use crate::controller::DigiAction;

const GROW_ROWS: usize = 256;

pub struct AptController {
    cfg: DigiConfig,
    rx: AptRx,
    scratch: Vec<AptEvent>,
    image_a: Vec<u8>,
    image_b: Vec<u8>,
    height_a: u16,
    height_b: u16,
    image_id: u32,
    saved: u32,
    queued: Vec<DigiAction>,
    status_dirty: bool,
    last_status: Option<SystemTime>,
}

impl AptController {
    pub fn new(cfg: DigiConfig, tap_rate: f64) -> Self {
        AptController {
            cfg,
            rx: AptRx::new(tap_rate),
            scratch: Vec::new(),
            image_a: Vec::new(),
            image_b: Vec::new(),
            height_a: 0,
            height_b: 0,
            image_id: 0,
            saved: 0,
            queued: Vec::new(),
            status_dirty: true,
            last_status: None,
        }
    }

    fn apt_status(&self) -> AptStatus {
        AptStatus {
            receiving: self.rx.receiving(),
            lines_a: self.height_a,
            lines_b: self.height_b,
            signal: self.rx.level(),
            saved: self.saved,
        }
    }

    fn begin(&mut self) {
        self.image_id = self.image_id.wrapping_add(1);
        self.height_a = 0;
        self.height_b = 0;
        self.image_a.clear();
        self.image_b.clear();
        self.image_a.reserve(CHANNEL_PIXELS * GROW_ROWS);
        self.image_b.reserve(CHANNEL_PIXELS * GROW_ROWS);
    }

    fn push_line(&mut self, channel: AptChannel, y: u16, gray: Vec<u8>) {
        let (buf, h) = match channel {
            AptChannel::A => (&mut self.image_a, &mut self.height_a),
            AptChannel::B => (&mut self.image_b, &mut self.height_b),
        };
        if gray.len() != CHANNEL_PIXELS || y as usize != *h as usize {
            return;
        }
        if buf.capacity() < buf.len() + CHANNEL_PIXELS {
            buf.reserve(CHANNEL_PIXELS * GROW_ROWS);
        }
        buf.extend_from_slice(&gray);
        *h = h.saturating_add(1);
        self.queued.push(DigiAction::AptLine {
            image_id: self.image_id,
            channel: match channel {
                AptChannel::A => 0,
                AptChannel::B => 1,
            },
            y,
            gray,
        });
    }

    fn emit_image(&mut self) {
        const MIN_ROWS: u16 = 16;
        let h = self.height_a.max(self.height_b);
        if h < MIN_ROWS {
            self.image_a.clear();
            self.image_b.clear();
            self.height_a = 0;
            self.height_b = 0;
            return;
        }
        let w = (CHANNEL_PIXELS * 2) as u16;
        let mut gray = vec![0u8; w as usize * h as usize];
        for y in 0..self.height_a as usize {
            let src = y * CHANNEL_PIXELS;
            let dst = y * w as usize;
            gray[dst..dst + CHANNEL_PIXELS]
                .copy_from_slice(&self.image_a[src..src + CHANNEL_PIXELS]);
        }
        for y in 0..self.height_b as usize {
            let src = y * CHANNEL_PIXELS;
            let dst = y * w as usize + CHANNEL_PIXELS;
            gray[dst..dst + CHANNEL_PIXELS]
                .copy_from_slice(&self.image_b[src..src + CHANNEL_PIXELS]);
        }
        self.saved = self.saved.wrapping_add(1);
        self.queued.push(DigiAction::AptImage {
            image_id: self.image_id,
            w,
            h,
            gray,
        });
        self.image_a.clear();
        self.image_b.clear();
        self.height_a = 0;
        self.height_b = 0;
    }

    fn digi_status(&self) -> DigiStatus {
        DigiStatus {
            mode: Mode::Apt,
            step: QsoStep::Idle,
            dx_call: None,
            dx_grid: None,
            tx_next: false,
            tx_pending_msg: None,
            audio_hz: sdroxide_dsp::apt::CARRIER_HZ as f32,
            tx_even: false,
            transmitting: false,
            tx_watchdog: false,
            tx_refused: None,
            transcript: Vec::<TranscriptLine>::new(),
            config: self.cfg.clone(),
            text_rx: String::new(),
            tx_sent: 0,
            fsq_heard: Vec::new(),
            fsq_messages: Vec::new(),
            rade: None,
            packet: None,
            navtex: None,
            acars: None,
            aprs: None,
            js8: None,
            atchat: None,
            fox_queue: Vec::new(),
            call_queue: Vec::new(),
            clock_offset_s: None,
            cw: None,
            wspr: None,
            pi4: None,
            qso: None,
        }
    }
}

impl DigiEngine for AptController {
    fn mode(&self) -> Mode {
        Mode::Apt
    }

    fn on_rx_audio(&mut self, tap: &[f32]) {
        let mut events = std::mem::take(&mut self.scratch);
        events.clear();
        self.rx.process(tap, &mut events);
        for e in events.drain(..) {
            match e {
                AptEvent::Start => {
                    self.begin();
                    self.status_dirty = true;
                }
                AptEvent::Line { channel, y, gray } => {
                    self.push_line(channel, y, gray);
                    self.status_dirty = true;
                }
                AptEvent::Complete => {
                    self.emit_image();
                    self.status_dirty = true;
                }
            }
        }
        self.scratch = events;
    }

    fn poll(&mut self, _now: SystemTime, _dial_hz: f64) -> Vec<DigiAction> {
        let mut actions = std::mem::take(&mut self.queued);
        let now = SystemTime::now();
        let due = self
            .last_status
            .map(|t| now.duration_since(t).map(|d| d.as_secs_f32() > 0.2).unwrap_or(true))
            .unwrap_or(true);
        if self.status_dirty || due {
            self.status_dirty = false;
            self.last_status = Some(now);
            actions.push(DigiAction::AptStatus(self.apt_status()));
            actions.push(DigiAction::Status(self.digi_status()));
        }
        actions
    }

    fn tx_burst_active(&self) -> bool {
        false
    }

    fn fill_tx_block(&mut self, _out: &mut [f32]) -> bool {
        false
    }

    fn on_burst_done(&mut self) {}

    fn abort(&mut self) {
        self.rx.reset();
        self.image_a.clear();
        self.image_b.clear();
        self.height_a = 0;
        self.height_b = 0;
        self.status_dirty = true;
    }

    fn abort_tx(&mut self) {}

    fn set_audio_hz(&mut self, _hz: f32) {}

    fn audio_hz(&self) -> f32 {
        sdroxide_dsp::apt::CARRIER_HZ as f32
    }

    fn status(&self) -> DigiStatus {
        self.digi_status()
    }

    fn set_config(&mut self, cfg: DigiConfig) {
        self.cfg = cfg;
        self.status_dirty = true;
    }

    fn apt_start(&mut self) {
        self.rx.force_start();
        if self.height_a == 0 && self.height_b == 0 {
            self.begin();
        }
        self.status_dirty = true;
    }

    fn apt_stop(&mut self) {
        self.emit_image();
        self.rx.reset();
        self.status_dirty = true;
    }
}
