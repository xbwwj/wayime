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
// Adjust this constant to tune how quickly the double-tap must happen.
const CTRL_SPACE_DOUBLE_TAP_INTERVAL: Duration = Duration::from_millis(250);

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
}

impl App {
    /// Start a fresh exact-code query and load its first page.
    fn refresh_candidates(&mut self) {
        self.query = None;

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

                return;
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
                .map_or(0, |query| query.candidates().len()),
            total_candidate_count = self.query.as_ref().map_or(0, Query::total_count),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "dictionary query completed"
        );
    }

    fn preedit_text(&self) -> String {
        if let Some(state) = self.voice_state.as_ref() {
            return match state {
                VoiceState::Recording => " 语音输入 🔴 ".to_owned(),
                VoiceState::Transcribing => " 语音输入 … ".to_owned(),
                VoiceState::Result(text) => text.clone(),
            };
        }

        if let Some(indicator) = &self.mode_indicator {
            return indicator.clone();
        }
        if self.composing.is_empty() {
            return String::new();
        }

        // Render the actual input code first, then the current query page, then
        // arrows derived from the known exact-code count.
        let query = self.query.as_ref();

        let (candidates, page_index, total_count, has_next_page) =
            if let Some(candidate) = self.expanded_candidate.as_ref() {
                (std::slice::from_ref(candidate), 0, 1, false)
            } else if let Some(query) = query {
                (
                    query.candidates(),
                    query.page_index(),
                    query.total_count(),
                    query.has_next_page(),
                )
            } else {
                (&[][..], 0, 0, false)
            };

        let indicator = page_indicator(page_index, total_count, has_next_page);
        let indicator_reserve = if indicator.is_empty() {
            0
        } else {
            1 + indicator.len() // separating space plus the UTF-8 indicator bytes
        };
        let mut candidates_text = String::new();
        for (index, candidate) in candidates.iter().enumerate() {
            let item = format!(
                "{}{}{}",
                if candidates_text.is_empty() { "" } else { " " },
                superscript_number(index + 1),
                candidate,
            );

            let proposed_len = self.composing.len()
                + 1 // space between input code and candidates
                + candidates_text.len()
                + item.len()
                + indicator_reserve;
            if proposed_len > MAX_PREEDIT_BYTES {
                break;
            }
            candidates_text.push_str(&item);
        }

        if candidates_text.is_empty() {
            // No candidates: retain the code and do not show paging arrows.
            self.composing.clone()
        } else if indicator.is_empty() {
            format!("{} {}", self.composing, candidates_text)
        } else {
            format!("{} {} {}", self.composing, candidates_text, indicator)
        }
    }

    /// 停止录音并等待后台 Whisper 识别完成。
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
        // Cursor offsets are UTF-8 byte offsets. In voice mode the displayed
        // label or recognized transcript is the complete logical preedit.
        let (cursor_begin, cursor_end) = if text.is_empty() {
            (-1, -1)
        } else if self.voice_state.is_some() || self.mode_indicator.is_some() {
            (0, text.len() as i32)
        } else if self.composing.is_empty() {
            (0, text.len() as i32)
        } else {
            // Candidate labels and paging arrows are presentation-only text and
            // must not be part of the cursor range.
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
        // The virtual keyboard is only a fallback for physical key events that
        // this IME does not consume. Committed text always goes through
        // zwp_input_method_v2.commit_string(), never through synthetic key presses.
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

        // Dropping the worker requests shutdown and releases any active microphone
        // stream. Deactivation must not leave recording alive for another client.
        self.voice_state = None;
        self.speech_worker = None;

        // Send release/destroy immediately on deactivation so the compositor
        // can restore normal keyboard routing for the newly focused client.
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
        let candidate = if let Some(candidate) = self.expanded_candidate.as_ref() {
            if index != 0 {
                return false;
            }

            candidate.clone()
        } else {
            let Some(candidate) = self
                .query
                .as_ref()
                .and_then(|query| query.candidates().get(index))
                .cloned()
            else {
                return false;
            };

            candidate
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
        // Show the mode that a single tap will select while waiting for a
        // possible second tap. A double tap replaces this with the recording UI.
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

    /// Resolve the first Ctrl+Space as a normal mode toggle.
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

            // The old tap expired; apply it as a regular toggle and treat this
            // press as the first tap of a new possible double-tap.
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

        // The tentative single-tap mode indicator is replaced by either the
        // recording indicator or the restored ordinary preedit if startup fails.
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

    fn exit_voice_mode(&mut self) {
        if self.voice_state.is_none() {
            return;
        }

        // Dropping the session worker requests shutdown and releases an active
        // microphone stream. The ordinary composition remains untouched.
        self.voice_state = None;
        self.speech_worker = None;
        self.active_repeat = None;
        self.sync_preedit();
        info!("voice input ended; returned to normal input mode");
    }

    fn commit_voice_result(&mut self) {
        let Some(VoiceState::Result(text)) = self.voice_state.take() else {
            return;
        };

        self.speech_worker = None;
        self.active_repeat = None;
        if self.active {
            // Commit the recognized text and restore whichever ordinary preedit
            // (if any) was active before speech input began.
            self.input_method.commit_string(text);
            self.sync_preedit();
        }
        info!("committed voice transcript; returned to normal input mode");
    }

    /// Discard the recognized text and return to the ordinary input state.
    fn discard_voice_result(&mut self) {
        if !matches!(self.voice_state.as_ref(), Some(VoiceState::Result(_))) {
            return;
        }
        self.exit_voice_mode();
    }

    fn handle_voice_key(&mut self, time: u32, key: u32) -> bool {
        match key {
            KEY_ESC => {
                self.exit_voice_mode();
                true
            }
            KEY_BACKSPACE => {
                // Once a transcript is ready, Backspace discards the whole phrase.
                // During recording/transcription it cancels the current voice task.
                if matches!(self.voice_state.as_ref(), Some(VoiceState::Result(_))) {
                    self.discard_voice_result();
                } else {
                    self.exit_voice_mode();
                }
                true
            }
            KEY_SPACE => {
                match self.voice_state.as_ref() {
                    Some(VoiceState::Recording) => self.stop_voice_recording(),
                    Some(VoiceState::Transcribing) => {}
                    Some(VoiceState::Result(_)) => self.commit_voice_result(),
                    None => return false,
                }
                true
            }
            KEY_ENTER => {
                // Enter accepts a transcript just like Space, but does not stop
                // recording early or interfere with an in-flight transcription.
                if matches!(self.voice_state.as_ref(), Some(VoiceState::Result(_))) {
                    self.commit_voice_result();
                }
                true
            }
            _ if is_printable_evdev_key(key) => {
                // Keep accidental typing from leaking into the focused application.
                true
            }
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
        eprintln!("handle_key_press: key={key}");
        // The keyboard grab is scoped to activate/deactivate. Events from a stale
        // grab must never be injected into whichever client happens to be focused.
        if !self.active {
            return true;
        }

        // Keep consumed keys consumed until keyup. Repeat events are generated
        // locally from the protocol's repeat_info timing, not from duplicate keydowns.
        if self.consumed_keys.contains(&key) {
            return true;
        }

        // Double-tap Ctrl+Space starts recording immediately. A single tap is
        // deferred for a very short interval so it can still toggle the IME.
        let toggle_shortcut =
            (key == KEY_SPACE && self.ctrl_space_pressed()) || key == KEY_XF86_KEYBOARD;

        if toggle_shortcut && self.voice_state.is_none() {
            return self.handle_ctrl_space_tap();
        }

        // Modifier key-downs can be part of the second Ctrl+Space tap, so do not
        // resolve a pending first tap until another non-modifier key is pressed.
        if self.pending_ctrl_space.is_some() && !Self::is_modifier_key(key) {
            self.resolve_pending_ctrl_space();
        }

        // During speech input, the ordinary input state is suspended. Modifier
        // shortcuts are passed through; Space/Backspace/Enter act on the transcript.
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

        // Disabled mode is a transparent pass-through, except for Ctrl+Space above.
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

        // The unshifted '-' and '=' keys are paging controls while a composition
        // exists. Consume them even with no candidates or at a page boundary.
        // Shifted variants remain punctuation (underscore and plus).
        if !self.composing.is_empty() && !shift_pressed && key == KEY_MINUS {
            self.page_previous();
            return true;
        }
        if !self.composing.is_empty() && !shift_pressed && key == KEY_EQUAL {
            self.page_next();
            return true;
        }

        // Unshifted 1..9 select candidates 1..9; 0 selects candidate 10.
        // While composing, a digit is always owned by the IME: invalid indices
        // are ignored instead of leaking into the focused application.
        if !self.composing.is_empty() && !shift_pressed && (KEY_1..=KEY_0).contains(&key) {
            let local_index = if key == KEY_0 {
                9
            } else {
                (key - KEY_1) as usize
            };
            if local_index < self.page_size.get() {
                self.select_candidate(local_index);
            }
            return true;
        }

        if key == KEY_SPACE {
            if !self.composing.is_empty() {
                // Space commits the first candidate on the current page (or the
                // raw code if there are no candidates), without trailing space.
                let text = self
                    .expanded_candidate
                    .clone()
                    .or_else(|| {
                        self.query
                            .as_ref()
                            .and_then(|query| query.candidates().first())
                            .cloned()
                    })
                    .unwrap_or_else(|| self.composing.clone());
                self.commit_text(text);
                return true;
            }
            self.forward_key(time, key, KEY_PRESSED);
            return false;
        }

        if key == KEY_ENTER {
            if !self.composing.is_empty() {
                // Enter explicitly commits the composition code, even when it has
                // candidates, which is useful for literal-code entry.
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

        // In Chinese mode, printable punctuation is handled centrally rather than
        // being sent through as physical key events. If composition is pending,
        // commit its current first candidate (or literal code) before punctuation.
        if let Some(punctuation) = evdev_chinese_punctuation(key, shift_pressed) {
            let mut text = String::new();
            if !self.composing.is_empty() {
                text.push_str(
                    self.expanded_candidate
                        .as_deref()
                        .or_else(|| {
                            self.query
                                .as_ref()
                                .and_then(|query| query.candidates().first())
                                .map(String::as_str)
                        })
                        .unwrap_or(self.composing.as_str()),
                );
            }
            text.push_str(punctuation);
            self.commit_text(text);
            return true;
        }

        // Safety net: no printable physical key should escape while a composition
        // is active. Known punctuation is mapped above; this catches layout-specific
        // printable keys that are not yet in the hard-coded punctuation table.
        if !self.composing.is_empty() && is_printable_evdev_key(key) {
            return true;
        }

        self.forward_key(time, key, KEY_PRESSED);
        false
    }

    /// Start local repeat for code letters and Backspace after their initial press.
    /// The protocol's repeat_info event supplies the compositor-configured rate/delay.
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

    /// Timeout passed to poll(): wake for key repeat, to resolve a pending single
    /// Ctrl+Space tap, and to receive speech events while recording/transcribing.
    pub fn poll_timeout_ms(&self) -> i32 {
        let mut timeout = self.repeat_timeout_ms();

        if let Some(first_tap) = self.pending_ctrl_space {
            let remaining = CTRL_SPACE_DOUBLE_TAP_INTERVAL.saturating_sub(first_tap.elapsed());
            let millis = remaining.as_nanos().saturating_add(999_999) / 1_000_000;
            timeout = min_timeout(timeout, millis.min(i32::MAX as u128) as i32);
        }

        if self.voice_state.is_some() {
            timeout = min_timeout(timeout, SPEECH_EVENT_POLL_MS);
        }

        timeout
    }

    /// Timeout passed to poll(): wake when the currently held key is due to repeat.
    pub fn repeat_timeout_ms(&self) -> i32 {
        let Some(repeat) = &self.active_repeat else {
            return -1;
        };
        let remaining = repeat.next_at.saturating_duration_since(Instant::now());
        let millis = remaining.as_nanos().saturating_add(999_999) / 1_000_000;
        millis.min(i32::MAX as u128) as i32
    }

    /// Execute one repeat tick. Repeated letters extend the code; repeated
    /// Backspace removes one character. Candidate searches run synchronously.
    pub fn handle_repeat_tick(&mut self) {
        if self
            .pending_ctrl_space
            .is_some_and(|first_tap| first_tap.elapsed() >= CTRL_SPACE_DOUBLE_TAP_INTERVAL)
        {
            self.resolve_pending_ctrl_space();
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
            // Do not emit a burst of catch-up repeats if the event loop was busy.
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

/// Return the available paging directions for the current zero-based page.
/// A single-page candidate set has no arrows because neither direction is available.
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

/// Map common US-QWERTY punctuation keys to Chinese/full-width punctuation.
/// This intentionally uses evdev key codes without an XKB layout dependency.
fn evdev_chinese_punctuation(key: u32, shift: bool) -> Option<&'static str> {
    Some(match (key, shift) {
        // Shifted number row.
        (2, true) => "！",
        (10, true) => "（",
        (11, true) => "）",

        // Main punctuation row. Period and slash use conventional Chinese
        // punctuation (。 and 、), rather than their literal full-width forms.
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

/// Printable keys on the US-QWERTY alphanumeric section. This is used as a
/// catch-all while composing so an unmapped printable key cannot leak to the app.
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
    // Linux evdev key codes for the physical QWERTY letter keys. We normalize to
    // lowercase pinyin; this intentionally avoids depending on a GUI toolkit or XKB.
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
        // The initial registry snapshot is maintained by registry_queue_init(). This
        // minimal example does not hot-plug seats/managers after startup.
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

                    // The grab is created only once the compositor activates this
                    // input method for a focused text-input.
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
                // The protocol serial is the number of `done` events seen so far.
                // Do not resend preedit here: done reports compositor-side state,
                // it is not a request to re-emit the current candidate list.
                state.serial = state.serial.wrapping_add(1);
            }
            InputMethodEvent::Unavailable => {
                warn!("compositor reports input-method-v2 unavailable");
                state.release_active_objects();
                // The protocol says the now-inert input-method object should be
                // destroyed after deactivation has been handled.
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
            // A queued event can outlive a deactivate notification. Never forward
            // such an event after keyboard routing has returned to the compositor.
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
                    // A key that ends voice input must not start ordinary
                    // IME key-repeat after the state transition has completed.
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

                    // Keep the mode indicator visible while Space is held, then
                    // clear it immediately on keyup (no timer or polling deadline).

                    if matches!(key, KEY_SPACE | KEY_XF86_KEYBOARD)
                        && state.pending_ctrl_space.is_none()
                        && state.mode_indicator.take().is_some()
                    {
                        state.sync_preedit();
                    }
                } else {
                    // The protocol only defines pressed and released states.
                }
            }
            KeyboardGrabEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                // The virtual-keyboard protocol requires a keymap before key or
                // modifier events are sent.
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
