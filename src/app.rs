use std::{
    collections::HashSet,
    num::NonZeroUsize,
    os::fd::AsFd,
    time::{Duration, Instant},
};

use tracing::{debug, error, info, warn};
use wayland_client::{
    delegate_noop,
    globals::GlobalListContents,
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_misc::{
    zwp_input_method_v2::client::{
        zwp_input_method_keyboard_grab_v2::{
            Event as KeyboardGrabEvent, ZwpInputMethodKeyboardGrabV2,
        },
        zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
        zwp_input_method_v2::{Event as InputMethodEvent, ZwpInputMethodV2},
    },
    zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    },
};

use crate::{
    config::Config,
    engine::{Engine, Query},
    keys::*,
    speech::{SpeechEvent, SpeechWorker},
};

const SPEECH_EVENT_POLL_MS: i32 = 25;

// Two Ctrl+Space presses within this interval start speech recording.
// A single press is deferred briefly so it can still toggle the IME.
const CTRL_SPACE_DOUBLE_TAP_INTERVAL: Duration = Duration::from_millis(250);

// In voice-result state, the first Space commits the transcript but defers
// leaving voice mode briefly. A second Space within this interval restarts
// recording using the existing SpeechWorker thread. Each completed recording
// launches a new whisper-cli process.
const VOICE_RESULT_SPACE_DOUBLE_TAP_INTERVAL: Duration = Duration::from_millis(250);

pub struct ActiveRepeat {
    key: u32,
    next_at: Instant,
}

/// Speech input temporarily overlays the ordinary input state. It always starts
/// directly in Recording; there is deliberately no idle voice-input state.
pub(crate) enum VoiceState {
    Recording,
    Transcribing,
    Result(String),
}

pub struct App {
    pub engine: Engine,
    pub config: Config,
    pub page_size: NonZeroUsize,
    pub seat: WlSeat,
    pub input_method: ZwpInputMethodV2,
    pub virtual_keyboard_manager: ZwpVirtualKeyboardManagerV1,

    // These objects exist only while the input method is active. In particular,
    // do not grab the physical keyboard during startup: activation is controlled
    // by the compositor's activate/deactivate events.
    pub keyboard_grab: Option<ZwpInputMethodKeyboardGrabV2>,
    pub virtual_keyboard: Option<ZwpVirtualKeyboardV1>,

    pub active: bool,
    pub unavailable: bool,
    pub serial: u32,
    pub keymap_ready: bool,

    pub composing: String,
    // Holds the current exact-code query and only its loaded page.
    pub query: Option<Query>,
    // Start in English/pass-through mode. Ctrl+Space enables the IME.
    pub enabled: bool,
    // Displayed only between Ctrl+Space keydown and Space keyup.
    pub mode_indicator: Option<String>,
    pub consumed_keys: HashSet<u32>,
    pub pressed_shortcut_modifiers: HashSet<u32>,
    pub pressed_shift_keys: HashSet<u32>,
    pub repeat_rate: u32,
    pub repeat_delay: Duration,
    pub active_repeat: Option<ActiveRepeat>,
    // XXX: should not be passed this way.
    pub expanded_candidate: Option<String>,

    pub(crate) voice_state: Option<VoiceState>,
    pub(crate) speech_worker: Option<SpeechWorker>,
    // The first Ctrl+Space is deferred briefly so a second press can trigger voice.
    pub(crate) pending_ctrl_space: Option<Instant>,
    // After committing a voice result with Space, keep the worker alive briefly.
    // A second Space restarts recording; timeout exits voice mode and drops the worker.
    pub(crate) pending_voice_exit: Option<Instant>,
}

impl App {
    /// Start a fresh exact-code query and load its first page.
    fn refresh_candidates(&mut self) {
        self.query = None;
        self.expanded_candidate = None;

        if self.composing.is_empty() || !self.active || !self.enabled {
            return;
        }

        let started = Instant::now();
        let input_len = self.composing.len();

        match self.config.expand(&self.composing) {
            Ok(Some(candidate)) => {
                self.expanded_candidate = Some(candidate);

                debug!(
                    input_len,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "JavaScript code expansion matched"
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    %error,
                    input_len,
                    "JavaScript code expansion failed; falling back to dictionary"
                );
            }
        }

        match self.engine.query(&self.composing, self.page_size) {
            Ok(query) => self.query = Some(query),
            Err(error) => warn!(%error, input_len, "dictionary query failed"),
        }

        debug!(
            input_len,
            candidate_count = self
                .query
                .as_ref()
                .map_or(0, |query| query.candidates().len())
                + usize::from(self.expanded_candidate.is_some()),
            total_candidate_count = self.query.as_ref().map_or(0, Query::total_count)
                + usize::from(self.expanded_candidate.is_some()),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "dictionary query completed"
        );
    }

    fn preedit_text(&self) -> String {
        if let Some(state) = self.voice_state.as_ref() {
            return match state {
                VoiceState::Recording => " 🎤 录音中... ".to_owned(),
                VoiceState::Transcribing => " ✍️ 识别中... ".to_owned(),
                VoiceState::Result(text) => text.clone(),
            };
        }

        if let Some(indicator) = &self.mode_indicator {
            return indicator.clone();
        }
        if self.composing.is_empty() {
            return String::new();
        }

        // Render the expansion result first on the first page, followed by the
        // dictionary candidates. Later pages contain dictionary candidates only.
        let query = self.query.as_ref();
        let page_index = query.map_or(0, Query::page_index);
        let expanded_visible = self.expanded_candidate_is_visible();

        let expanded_candidate = if expanded_visible {
            self.expanded_candidate.as_deref()
        } else {
            None
        };
        let candidates = expanded_candidate.into_iter().chain(query.into_iter().flat_map(
            |query| query.candidates().iter().map(String::as_str),
        ));

        let total_count = query.map_or(0, Query::total_count)
            + usize::from(self.expanded_candidate.is_some());
        let has_next_page = query.is_some_and(Query::has_next_page);

        let indicator = page_indicator(page_index, total_count, has_next_page);
        let indicator_reserve = if indicator.is_empty() {
            0
        } else {
            1 + indicator.len()
        };
        let mut candidates_text = String::new();
        for (index, candidate) in candidates.enumerate() {
            let item = format!(
                "{}{}{}",
                if candidates_text.is_empty() { "" } else { " " },
                superscript_number(index + 1),
                candidate,
            );

            let proposed_len = self.composing.len()
                + 1
                + candidates_text.len()
                + item.len()
                + indicator_reserve;
            if proposed_len > MAX_PREEDIT_BYTES {
                break;
            }
            candidates_text.push_str(&item);
        }

        if candidates_text.is_empty() {
            self.composing.clone()
        } else if indicator.is_empty() {
            format!("{} {}", self.composing, candidates_text)
        } else {
            format!("{} {} {}", self.composing, candidates_text, indicator)
        }
    }

    fn expanded_candidate_is_visible(&self) -> bool {
        self.expanded_candidate.is_some()
            && self
                .query
                .as_ref()
                .map_or(true, |query| query.page_index() == 0)
    }

    fn first_candidate(&self) -> Option<&str> {
        if self.expanded_candidate_is_visible() {
            self.expanded_candidate.as_deref()
        } else {
            self.query
                .as_ref()
                .and_then(|query| query.candidates().first())
                .map(String::as_str)
        }
    }

    /// Stop recording and wait for the background whisper-cli transcription.
    fn stop_voice_recording(&mut self) {
        if !matches!(self.voice_state.as_ref(), Some(VoiceState::Recording)) {
            return;
        }

        let result = self
            .speech_worker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("speech worker is not available"))
            .and_then(|worker| worker.stop_recording());

        if let Err(error) = result {
            warn!(%error, "could not stop voice recording");
            self.exit_voice_mode();
            return;
        }

        self.voice_state = Some(VoiceState::Transcribing);
        self.active_repeat = None;
        self.sync_preedit();

        info!("stopped voice recording; waiting for transcription");
    }

    fn sync_preedit(&self) {
        if !self.active {
            return;
        }
        let text = self.preedit_text();

        let (cursor_begin, cursor_end) = if text.is_empty() {
            (-1, -1)
        } else if self.voice_state.is_some() || self.mode_indicator.is_some() {
            (0, text.len() as i32)
        } else if self.composing.is_empty() {
            (0, text.len() as i32)
        } else {
            (0, self.composing.len() as i32)
        };

        self.input_method
            .set_preedit_string(text, cursor_begin, cursor_end);
        self.input_method.commit(self.serial);
    }

    fn clear_composition(&mut self) {
        self.composing.clear();
        self.query = None;
        self.expanded_candidate = None;
    }

    fn commit_text(&mut self, text: String) {
        self.clear_composition();
        if self.active {
            self.input_method.commit_string(text);
            self.input_method.set_preedit_string(String::new(), -1, -1);
            self.input_method.commit(self.serial);
        }
    }

    fn forward_key(&self, time: u32, key: u32, state: u32) {
        if self.active && self.keymap_ready {
            if let Some(virtual_keyboard) = &self.virtual_keyboard {
                virtual_keyboard.key(time, key, state);
            }
        }
    }

    fn release_active_objects(&mut self) {
        self.active = false;
        self.keymap_ready = false;
        self.consumed_keys.clear();
        self.pressed_shortcut_modifiers.clear();
        self.pressed_shift_keys.clear();
        self.active_repeat = None;
        self.repeat_rate = 0;
        self.enabled = false;
        self.pending_ctrl_space = None;
        self.pending_voice_exit = None;

        // Dropping the worker requests shutdown, releases any active microphone
        // stream, and terminates an active whisper-cli child process.
        self.voice_state = None;
        self.speech_worker = None;

        if let Some(keyboard_grab) = self.keyboard_grab.take() {
            keyboard_grab.release();
        }
        if let Some(virtual_keyboard) = self.virtual_keyboard.take() {
            virtual_keyboard.destroy();
        }
        self.mode_indicator = None;
        self.clear_composition();
    }

    fn select_candidate(&mut self, index: usize) -> bool {
        let expanded_visible = self.expanded_candidate_is_visible();
        let candidate = if expanded_visible && index == 0 {
            self.expanded_candidate.clone()
        } else {
            let query_index = index.saturating_sub(usize::from(expanded_visible));
            self.query
                .as_ref()
                .and_then(|query| query.candidates().get(query_index))
                .cloned()
        };

        let Some(candidate) = candidate else {
            return false;
        };

        self.commit_text(candidate);
        true
    }

    fn ctrl_space_pressed(&self) -> bool {
        let control = self.pressed_shortcut_modifiers.contains(&KEY_LEFT_CTRL)
            || self.pressed_shortcut_modifiers.contains(&KEY_RIGHT_CTRL);
        let other_modifier = self.pressed_shortcut_modifiers.iter().any(|key| {
            matches!(
                *key,
                KEY_LEFT_ALT | KEY_RIGHT_ALT | KEY_LEFT_META | KEY_RIGHT_META
            )
        });
        control && !other_modifier && !self.shift_pressed()
    }

    fn is_modifier_key(key: u32) -> bool {
        is_shortcut_modifier(key) || matches!(key, KEY_LEFT_SHIFT | KEY_RIGHT_SHIFT)
    }

    fn show_pending_ctrl_space_indicator(&mut self) {
        self.mode_indicator = Some((if self.enabled { " En " } else { " 中 " }).to_owned());
        self.sync_preedit();
    }

    fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        self.active_repeat = None;
        self.clear_composition();
        self.mode_indicator = None;
        self.sync_preedit();

        info!(
            enabled = self.enabled,
            mode = if self.enabled { "zh" } else { "en" },
            "input mode changed"
        );
    }

    fn resolve_pending_ctrl_space(&mut self) {
        if self.pending_ctrl_space.take().is_some() {
            self.toggle_enabled();
        }
    }

    fn handle_ctrl_space_tap(&mut self) -> bool {
        let now = Instant::now();

        if let Some(first_tap) = self.pending_ctrl_space.take() {
            if now.saturating_duration_since(first_tap) <= CTRL_SPACE_DOUBLE_TAP_INTERVAL {
                self.active_repeat = None;
                self.enter_voice_recording();
                return true;
            }

            self.toggle_enabled();
        }

        self.active_repeat = None;
        self.pending_ctrl_space = Some(now);
        self.show_pending_ctrl_space_indicator();
        true
    }

    fn enter_voice_recording(&mut self) {
        if self.voice_state.is_some() {
            return;
        }

        self.pending_voice_exit = None;
        self.mode_indicator = None;
        let worker = match SpeechWorker::spawn("auto") {
            Ok(worker) => worker,
            Err(error) => {
                error!(%error, "could not create speech worker");
                self.sync_preedit();
                return;
            }
        };

        if let Err(error) = worker.start_recording() {
            error!(%error, "could not start voice recording");
            drop(worker);
            self.sync_preedit();
            return;
        }

        self.mode_indicator = None;
        self.active_repeat = None;
        self.speech_worker = Some(worker);
        self.voice_state = Some(VoiceState::Recording);
        self.sync_preedit();
        info!("started voice recording via double Ctrl+Space");
    }

    /// Restart recording without dropping SpeechWorker.
    ///
    /// The same worker thread is reused, but the next completed recording starts
    /// a fresh whisper-cli process. No Whisper model remains resident in wayime.
    fn start_voice_recording_with_existing_worker(&mut self) {
        if self.voice_state.is_some() {
            return;
        }

        let result = self
            .speech_worker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("speech worker is not available"))
            .and_then(|worker| worker.start_recording());

        if let Err(error) = result {
            error!(%error, "could not restart voice recording");
            self.exit_voice_mode();
            return;
        }

        self.pending_voice_exit = None;
        self.mode_indicator = None;
        self.active_repeat = None;
        self.voice_state = Some(VoiceState::Recording);
        self.sync_preedit();
        info!("restarted voice recording with the existing speech worker");
    }

    /// Backspace on a voice transcript discards that transcript and immediately
    /// starts a new recording using the existing worker thread.
    fn restart_voice_recording_from_result(&mut self) {
        if !matches!(self.voice_state.as_ref(), Some(VoiceState::Result(_))) {
            return;
        }

        self.voice_state = None;
        self.pending_voice_exit = None;
        self.active_repeat = None;
        self.start_voice_recording_with_existing_worker();
    }

    fn exit_voice_mode(&mut self) {
        if self.voice_state.is_none() && self.speech_worker.is_none() {
            self.pending_voice_exit = None;
            return;
        }

        // Dropping the worker requests shutdown and releases an active microphone
        // stream. A running whisper-cli child is also terminated by the worker.
        self.voice_state = None;
        self.speech_worker = None;
        self.pending_voice_exit = None;
        self.active_repeat = None;
        self.sync_preedit();
        info!("voice input ended; returned to normal input mode");
    }

    /// Commit the transcript and leave voice mode immediately. Used by Enter.
    fn commit_voice_result_and_exit(&mut self) {
        let Some(VoiceState::Result(text)) = self.voice_state.take() else {
            return;
        };

        self.pending_voice_exit = None;
        self.speech_worker = None;
        self.active_repeat = None;
        if self.active {
            self.input_method.commit_string(text);
            self.sync_preedit();
        }
        info!("committed voice transcript; exited voice input mode");
    }

    /// Commit the transcript but keep the worker alive briefly.
    /// A second Space within VOICE_RESULT_SPACE_DOUBLE_TAP_INTERVAL restarts
    /// recording; timeout exits voice mode and drops the worker.
    fn commit_voice_result_deferred_exit(&mut self) {
        let Some(VoiceState::Result(text)) = self.voice_state.take() else {
            return;
        };

        self.active_repeat = None;
        if self.active {
            self.input_method.commit_string(text);
            self.sync_preedit();
        }
        self.pending_voice_exit = Some(Instant::now());
        info!("committed voice transcript; waiting briefly for a double Space restart");
    }

    fn handle_voice_key(&mut self, time: u32, key: u32) -> bool {
        match key {
            KEY_ESC => {
                self.exit_voice_mode();
                true
            }

            KEY_BACKSPACE => {
                if matches!(self.voice_state.as_ref(), Some(VoiceState::Result(_))) {
                    self.restart_voice_recording_from_result();
                } else {
                    self.exit_voice_mode();
                }
                true
            }

            KEY_SPACE => {
                match self.voice_state.as_ref() {
                    Some(VoiceState::Recording) => self.stop_voice_recording(),
                    Some(VoiceState::Transcribing) => {}
                    Some(VoiceState::Result(_)) => self.commit_voice_result_deferred_exit(),
                    None => return false,
                }
                true
            }

            KEY_ENTER => {
                if matches!(self.voice_state.as_ref(), Some(VoiceState::Result(_))) {
                    self.commit_voice_result_and_exit();
                }
                true
            }

            _ if is_printable_evdev_key(key) => true,

            _ => {
                self.forward_key(time, key, KEY_PRESSED);
                false
            }
        }
    }

    /// Drain speech events without blocking the Wayland dispatch loop.
    pub fn poll_speech_events(&mut self) {
        let mut pending = Vec::new();

        if let Some(worker) = self.speech_worker.as_ref() {
            loop {
                match worker.try_recv() {
                    Ok(event) => pending.push(event),

                    Err(std::sync::mpsc::TryRecvError::Empty)
                    | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        for event in pending {
            match event {
                SpeechEvent::Transcript(text) => {
                    if !matches!(self.voice_state.as_ref(), Some(VoiceState::Transcribing)) {
                        continue;
                    }

                    if text.trim().is_empty() {
                        warn!("voice recognition returned no text");
                        self.exit_voice_mode();
                    } else {
                        self.voice_state = Some(VoiceState::Result(text));
                        self.sync_preedit();
                    }
                }

                SpeechEvent::Error(message) => {
                    warn!(error = %message, "voice input failed");
                    if self.voice_state.is_some() {
                        self.exit_voice_mode();
                    }
                }
            }
        }
    }

    fn page_previous(&mut self) {
        let Some(page_index) = self.query.as_ref().and_then(|query| {
            query
                .has_previous_page()
                .then_some(query.page_index().saturating_sub(1))
        }) else {
            return;
        };

        if let Some(query) = self.query.as_mut() {
            if let Err(error) = self.engine.load_page(query, page_index) {
                warn!(%error, page_index, "candidate page query failed");
                return;
            }
        }

        self.sync_preedit();
    }

    fn page_next(&mut self) {
        let Some(page_index) = self.query.as_ref().and_then(|query| {
            query
                .has_next_page()
                .then_some(query.page_index().saturating_add(1))
        }) else {
            return;
        };

        if let Some(query) = self.query.as_mut() {
            if let Err(error) = self.engine.load_page(query, page_index) {
                warn!(%error, page_index, "candidate page query failed");
                return;
            }
        }

        self.sync_preedit();
    }

    /// Handle one keyboard event. Returns true if this physical key press is consumed
    /// by the input method; its release will be consumed as well.
    fn handle_key_press(&mut self, time: u32, key: u32) -> bool {
        if !self.active {
            return true;
        }

        if self.consumed_keys.contains(&key) {
            return true;
        }

        // A committed voice transcript may be waiting for a possible second
        // Space. Intercept that before ordinary shortcut handling.
        if self.pending_voice_exit.is_some() {
            let plain_space = key == KEY_SPACE
                && self.pressed_shortcut_modifiers.is_empty()
                && !self.shift_pressed();

            if plain_space {
                self.pending_voice_exit = None;
                self.active_repeat = None;
                self.start_voice_recording_with_existing_worker();
                return true;
            }

            if !Self::is_modifier_key(key) {
                self.exit_voice_mode();
            }
        }

        // Double-tap Ctrl+Space starts recording immediately. A single tap is
        // deferred for a very short interval so it can still toggle the IME.
        let toggle_shortcut =
            (key == KEY_SPACE && self.ctrl_space_pressed()) || key == KEY_XF86_KEYBOARD;

        if toggle_shortcut && self.voice_state.is_none() {
            return self.handle_ctrl_space_tap();
        }

        if self.pending_ctrl_space.is_some() && !Self::is_modifier_key(key) {
            self.resolve_pending_ctrl_space();
        }

        if self.voice_state.is_some() {
            if !self.pressed_shortcut_modifiers.is_empty() {
                self.forward_key(time, key, KEY_PRESSED);
                return false;
            }
            return self.handle_voice_key(time, key);
        }

        if !self.pressed_shortcut_modifiers.is_empty() {
            self.forward_key(time, key, KEY_PRESSED);
            return false;
        }

        if !self.enabled {
            self.forward_key(time, key, KEY_PRESSED);
            return false;
        }

        if let Some(letter) = evdev_letter(key) {
            self.composing.push(letter);
            self.refresh_candidates();
            self.sync_preedit();
            return true;
        }

        if key == KEY_BACKSPACE {
            if !self.composing.is_empty() {
                self.composing.pop();
                self.refresh_candidates();
                self.sync_preedit();
                return true;
            }
            self.forward_key(time, key, KEY_PRESSED);
            return false;
        }

        let shift_pressed = self.shift_pressed();

        if !self.composing.is_empty() && !shift_pressed && key == KEY_MINUS {
            self.page_previous();
            return true;
        }

        if !self.composing.is_empty() && !shift_pressed && key == KEY_EQUAL {
            self.page_next();
            return true;
        }

        if !self.composing.is_empty() && !shift_pressed && (KEY_1..=KEY_0).contains(&key) {
            let local_index = if key == KEY_0 {
                9
            } else {
                (key - KEY_1) as usize
            };

            let candidate_count = self
                .query
                .as_ref()
                .map_or(0, |query| query.candidates().len())
                + usize::from(self.expanded_candidate_is_visible());

            if local_index < candidate_count {
                self.select_candidate(local_index);
            }

            return true;
        }

        if key == KEY_SPACE {
            if !self.composing.is_empty() {
                let text = self
                    .first_candidate()
                    .map(str::to_owned)
                    .unwrap_or_else(|| self.composing.clone());

                self.commit_text(text);
                return true;
            }

            self.forward_key(time, key, KEY_PRESSED);
            return false;
        }

        if key == KEY_ENTER {
            if !self.composing.is_empty() {
                self.commit_text(self.composing.clone());
                return true;
            }

            self.forward_key(time, key, KEY_PRESSED);
            return false;
        }

        if key == KEY_ESC && !self.composing.is_empty() {
            self.clear_composition();
            self.sync_preedit();
            return true;
        }

        if let Some(punctuation) = evdev_chinese_punctuation(key, shift_pressed) {
            let mut text = String::new();

            if !self.composing.is_empty() {
                text.push_str(self.first_candidate().unwrap_or(self.composing.as_str()));
            }

            text.push_str(punctuation);
            self.commit_text(text);
            return true;
        }

        if !self.composing.is_empty() && is_printable_evdev_key(key) {
            return true;
        }

        self.forward_key(time, key, KEY_PRESSED);
        false
    }

    /// Start local repeat for code letters and Backspace after their initial press.
    fn start_key_repeat(&mut self, key: u32, consumed: bool) {
        if !consumed
            || self.repeat_rate == 0
            || !self.enabled
            || self.mode_indicator.is_some()
            || self.voice_state.is_some()
            || !(evdev_letter(key).is_some() || key == KEY_BACKSPACE)
        {
            return;
        }

        self.active_repeat = Some(ActiveRepeat {
            key,
            next_at: Instant::now() + self.repeat_delay,
        });

        debug!(
            key,
            delay_ms = self.repeat_delay.as_millis() as u64,
            rate = self.repeat_rate,
            "started key repeat"
        );
    }

    fn update_repeat_info(&mut self, rate: i32, delay: i32) {
        if rate < 0 || delay < 0 {
            warn!(rate, delay, "ignoring invalid keyboard repeat settings");
            return;
        }

        self.repeat_rate = rate as u32;
        self.repeat_delay = Duration::from_millis(delay as u64);

        if self.repeat_rate == 0 {
            self.active_repeat = None;
        } else if let Some(repeat) = &mut self.active_repeat {
            repeat.next_at = Instant::now() + self.repeat_delay;
        }

        info!(
            rate = self.repeat_rate,
            delay_ms = delay,
            "keyboard repeat settings updated"
        );
    }

    /// Timeout passed to poll(): wake for key repeat, pending shortcuts,
    /// deferred voice exit, and speech worker events.
    pub fn poll_timeout_ms(&self) -> i32 {
        let mut timeout = self.repeat_timeout_ms();

        if let Some(first_tap) = self.pending_ctrl_space {
            let remaining = CTRL_SPACE_DOUBLE_TAP_INTERVAL.saturating_sub(first_tap.elapsed());
            let millis = remaining.as_nanos().saturating_add(999_999) / 1_000_000;
            timeout = min_timeout(timeout, millis.min(i32::MAX as u128) as i32);
        }

        if let Some(first_tap) = self.pending_voice_exit {
            let remaining =
                VOICE_RESULT_SPACE_DOUBLE_TAP_INTERVAL.saturating_sub(first_tap.elapsed());
            let millis = remaining.as_nanos().saturating_add(999_999) / 1_000_000;
            timeout = min_timeout(timeout, millis.min(i32::MAX as u128) as i32);
        }

        if self.voice_state.is_some() {
            timeout = min_timeout(timeout, SPEECH_EVENT_POLL_MS);
        }

        timeout
    }

    pub fn repeat_timeout_ms(&self) -> i32 {
        let Some(repeat) = &self.active_repeat else {
            return -1;
        };

        let remaining = repeat.next_at.saturating_duration_since(Instant::now());
        let millis = remaining.as_nanos().saturating_add(999_999) / 1_000_000;

        millis.min(i32::MAX as u128) as i32
    }

    /// Execute one repeat tick.
    pub fn handle_repeat_tick(&mut self) {
        if self
            .pending_ctrl_space
            .is_some_and(|first_tap| first_tap.elapsed() >= CTRL_SPACE_DOUBLE_TAP_INTERVAL)
        {
            self.resolve_pending_ctrl_space();
        }

        if self
            .pending_voice_exit
            .is_some_and(|first_tap| {
                first_tap.elapsed() >= VOICE_RESULT_SPACE_DOUBLE_TAP_INTERVAL
            })
        {
            self.exit_voice_mode();
        }

        let now = Instant::now();

        let key = {
            let Some(repeat) = &mut self.active_repeat else {
                return;
            };

            if now < repeat.next_at || self.repeat_rate == 0 {
                return;
            }

            let interval =
                Duration::from_nanos((1_000_000_000_u64 / u64::from(self.repeat_rate)).max(1));

            repeat.next_at = now + interval;
            repeat.key
        };

        if !self.active
            || !self.enabled
            || self.mode_indicator.is_some()
            || self.voice_state.is_some()
            || !self.pressed_shortcut_modifiers.is_empty()
        {
            self.active_repeat = None;
            return;
        }

        if let Some(letter) = evdev_letter(key) {
            self.composing.push(letter);
            self.refresh_candidates();
            self.sync_preedit();

            debug!(
                key,
                composition_len = self.composing.len(),
                "repeated composition key"
            );
        } else if key == KEY_BACKSPACE {
            if self.composing.is_empty() {
                self.active_repeat = None;
                return;
            }

            self.composing.pop();
            self.refresh_candidates();
            self.sync_preedit();

            debug!(composition_len = self.composing.len(), "repeated Backspace");
        } else {
            self.active_repeat = None;
        }
    }

    fn shift_pressed(&self) -> bool {
        !self.pressed_shift_keys.is_empty()
    }
}

fn min_timeout(current: i32, candidate: i32) -> i32 {
    if current < 0 {
        candidate
    } else {
        current.min(candidate)
    }
}

fn page_indicator(page_index: usize, total_count: usize, has_next_page: bool) -> &'static str {
    if total_count == 0 {
        ""
    } else if page_index == 0 {
        if has_next_page {
            "↓"
        } else {
            ""
        }
    } else if has_next_page {
        "⇅"
    } else {
        "↑"
    }
}

fn evdev_chinese_punctuation(key: u32, shift: bool) -> Option<&'static str> {
    Some(match (key, shift) {
        (2, true) => "！",
        (10, true) => "（",
        (11, true) => "）",

        (26, false) => "「",
        (27, false) => "」",
        (26, true) => "『",
        (27, true) => "』",
        (39, false) => "；",
        (39, true) => "：",
        (40, false) => "＇",
        (40, true) => "＂",
        (41, false) => "｀",
        (41, true) => "～",
        (43, false) => "、",
        (43, true) => "｜",
        (51, false) => "，",
        (51, true) => "《",
        (52, false) => "。",
        (52, true) => "》",
        (53, true) => "？",
        _ => return None,
    })
}

fn is_printable_evdev_key(key: u32) -> bool {
    matches!(key, 2..=13 | 16..=27 | 30..=41 | 43..=53 | 86)
}

fn superscript_number(number: usize) -> String {
    number
        .to_string()
        .chars()
        .map(|digit| match digit {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            _ => digit,
        })
        .collect()
}

fn is_shortcut_modifier(key: u32) -> bool {
    matches!(
        key,
        KEY_LEFT_CTRL
            | KEY_RIGHT_CTRL
            | KEY_LEFT_ALT
            | KEY_RIGHT_ALT
            | KEY_LEFT_META
            | KEY_RIGHT_META
    )
}

fn evdev_letter(key: u32) -> Option<char> {
    Some(match key {
        16 => 'q',
        17 => 'w',
        18 => 'e',
        19 => 'r',
        20 => 't',
        21 => 'y',
        22 => 'u',
        23 => 'i',
        24 => 'o',
        25 => 'p',
        30 => 'a',
        31 => 's',
        32 => 'd',
        33 => 'f',
        34 => 'g',
        35 => 'h',
        36 => 'j',
        37 => 'k',
        38 => 'l',
        44 => 'z',
        45 => 'x',
        46 => 'c',
        47 => 'v',
        48 => 'b',
        49 => 'n',
        50 => 'm',
        _ => return None,
    })
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The initial registry snapshot is maintained by registry_queue_init().
    }
}

delegate_noop!(App: ignore WlSeat);
delegate_noop!(App: ignore ZwpInputMethodManagerV2);
delegate_noop!(App: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(App: ignore ZwpVirtualKeyboardV1);

impl Dispatch<ZwpInputMethodV2, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ZwpInputMethodV2,
        event: InputMethodEvent,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            InputMethodEvent::Activate => {
                if !state.active {
                    state.active = true;
                    state.clear_composition();
                    state.consumed_keys.clear();
                    state.keymap_ready = false;
                    state.enabled = false;
                    state.voice_state = None;
                    state.speech_worker = None;
                    state.pending_ctrl_space = None;
                    state.pending_voice_exit = None;

                    state.keyboard_grab = Some(state.input_method.grab_keyboard(qh, ()));
                    state.virtual_keyboard = Some(
                        state
                            .virtual_keyboard_manager
                            .create_virtual_keyboard(&state.seat, qh, ()),
                    );
                }
            }

            InputMethodEvent::Deactivate => {
                state.release_active_objects();
            }

            InputMethodEvent::Done => {
                state.serial = state.serial.wrapping_add(1);
            }

            InputMethodEvent::Unavailable => {
                warn!("compositor reports input-method-v2 unavailable");
                state.release_active_objects();
                state.input_method.destroy();
                state.unavailable = true;
            }

            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ZwpInputMethodKeyboardGrabV2,
        event: KeyboardGrabEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if !state.active {
            return;
        }

        match event {
            KeyboardGrabEvent::Keymap { format, fd, size } => {
                let format: u32 = format.into();

                if format != KEYMAP_XKB_V1 {
                    warn!(format, "unsupported keyboard keymap format");
                    return;
                }

                match fd.as_fd().try_clone_to_owned() {
                    Ok(fd_copy) => {
                        if let Some(virtual_keyboard) = &state.virtual_keyboard {
                            virtual_keyboard.keymap(format, fd_copy.as_fd(), size);
                            state.keymap_ready = true;
                        }
                    }

                    Err(error) => error!(%error, "cannot duplicate keyboard keymap fd"),
                }
            }

            KeyboardGrabEvent::Key {
                time,
                key,
                state: key_state,
                ..
            } => {
                let key_state: u32 = key_state.into();

                if key_state == KEY_PRESSED {
                    if is_shortcut_modifier(key) {
                        state.pressed_shortcut_modifiers.insert(key);
                    }

                    if matches!(key, KEY_LEFT_SHIFT | KEY_RIGHT_SHIFT) {
                        state.pressed_shift_keys.insert(key);
                    }

                    let was_down = state.consumed_keys.contains(&key);
                    let was_voice_mode = state.voice_state.is_some();
                    let consumed = state.handle_key_press(time, key);

                    if consumed {
                        state.consumed_keys.insert(key);
                    }

                    if !was_down && !was_voice_mode {
                        state.start_key_repeat(key, consumed);
                    }
                } else if key_state == KEY_RELEASED {
                    if !state.consumed_keys.remove(&key) {
                        state.forward_key(time, key, KEY_RELEASED);
                    }

                    state.pressed_shortcut_modifiers.remove(&key);
                    state.pressed_shift_keys.remove(&key);

                    if state
                        .active_repeat
                        .as_ref()
                        .is_some_and(|repeat| repeat.key == key)
                    {
                        state.active_repeat = None;
                        debug!(key, "stopped key repeat");
                    }

                    if matches!(key, KEY_SPACE | KEY_XF86_KEYBOARD)
                        && state.pending_ctrl_space.is_none()
                        && state.mode_indicator.take().is_some()
                    {
                        state.sync_preedit();
                    }
                }
            }

            KeyboardGrabEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if state.keymap_ready {
                    if let Some(virtual_keyboard) = &state.virtual_keyboard {
                        virtual_keyboard.modifiers(
                            mods_depressed,
                            mods_latched,
                            mods_locked,
                            group,
                        );
                    }
                }
            }

            KeyboardGrabEvent::RepeatInfo { rate, delay } => {
                state.update_repeat_info(rate, delay);
            }

            _ => {}
        }
    }
}
