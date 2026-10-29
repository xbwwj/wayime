use std::{
    collections::HashSet,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use engine::Engine;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use wayland_client::{globals::registry_queue_init, protocol::wl_seat::WlSeat, Connection};
use wayland_protocols_misc::{
    zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
};

use crate::{
    app::{App, VoiceState},
    cli::{Args, Command, ModelCommand},
    config::Config,
    utils::{default_cache_path, default_dict_path},
};

mod app;
mod cli;
mod config;
mod engine;
mod error;
mod keys;
mod model;
mod speech;
mod utils;

fn main() {
    init_logging();

    if let Err(error) = run() {
        let chain = format!("{error:#}");
        error!(error = %chain, "fatal error");
        std::process::exit(1);
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_names(true)
        .compact()
        .init();
}

fn run() -> Result<()> {
    let args = Args::parse();

    match args.command.as_ref() {
        Some(Command::Model {
            command: ModelCommand::Show,
        }) => {
            return model::show();
        }
        Some(Command::Model {
            command: ModelCommand::Download,
        }) => {
            return model::download();
        }
        Some(Command::Transcribe { language }) => {
            let model_path = model::downloaded_path()?;
            return speech::run(&model_path, language);
        }
        None => {}
    }

    let config = Config::load_default().context("failed to load JavaScript config")?;

    let dict_path = match args.dict {
        Some(path) => path,
        None => default_dict_path()?,
    };

    let cache_path = default_cache_path()?;
    let page_size = args.page_size;

    let load_started = Instant::now();
    let engine = Engine::load(&dict_path, &cache_path).with_context(|| {
        format!(
            "failed to load dictionary {} using cache {}",
            dict_path.display(),
            cache_path.display()
        )
    })?;

    info!(
        dict = %dict_path.display(),
        cache = %cache_path.display(),
        elapsed_ms = load_started.elapsed().as_millis() as u64,
        "dictionary loaded"
    );

    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .context("no wl_seat global; this example expects one seat")?;

    let input_method_manager: ZwpInputMethodManagerV2 = globals
        .bind(&qh, 1..=1, ())
        .context("compositor does not expose zwp_input_method_manager_v2")?;

    let virtual_keyboard_manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context(
            "compositor does not expose zwp_virtual_keyboard_manager_v1; forwarding unhandled keys needs it",
        )?;

    let input_method = input_method_manager.get_input_method(&seat, &qh, ());

    let mut app = App {
        engine,
        config,
        page_size,
        seat,
        input_method,
        virtual_keyboard_manager,
        keyboard_grab: None,
        virtual_keyboard: None,
        active: false,
        unavailable: false,
        serial: 0,
        keymap_ready: false,
        composing: String::new(),
        query: None,
        enabled: false,
        mode_indicator: None,
        consumed_keys: HashSet::new(),
        pressed_shortcut_modifiers: HashSet::new(),
        pressed_shift_keys: HashSet::new(),
        repeat_rate: 0,
        repeat_delay: Duration::ZERO,
        active_repeat: None,
        expanded_candidate: None,
        voice_state: None::<VoiceState>,
        speech_worker: None,
        pending_ctrl_space: None,
    };

    info!(
        dict = %dict_path.display(),
        page_size = app.page_size,
        repeat_support = true,
        "wayime started; single Ctrl+Space toggles IME, double Ctrl+Space starts voice input, '-'/'=' change candidate page"
    );

    while !app.unavailable {
        event_queue.dispatch_pending(&mut app)?;
        app.poll_speech_events();
        event_queue.flush()?;

        let Some(read_guard) = event_queue.prepare_read() else {
            // Events arrived between dispatch_pending and prepare_read.
            continue;
        };

        let mut descriptor = libc::pollfd {
            fd: read_guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        let poll_result = unsafe { libc::poll(&mut descriptor, 1, app.poll_timeout_ms()) };

        if poll_result > 0 {
            let wayland_ready = descriptor.revents
                & (libc::POLLIN | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
                != 0;

            if wayland_ready {
                read_guard.read()?;
            } else {
                drop(read_guard);
            }
        } else {
            drop(read_guard);

            if poll_result < 0 {
                let error = std::io::Error::last_os_error();

                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }

                return Err(error).context("polling Wayland connection");
            }

            // poll() timed out because a held key reached its repeat deadline, or
            // because the app is polling for a speech worker result.
            app.handle_repeat_tick();
        }
    }

    Ok(())
}
