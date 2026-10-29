use std::{
    path::Path,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, SampleFormat, SizedSample, I24, U24,
};
use tracing::{debug, warn};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const RECORD_SECONDS: u64 = 10;
const WHISPER_SAMPLE_RATE: u32 = 16_000;
const RECORDING_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Whisper.cpp 的识别封装。
pub struct Recognizer {
    context: WhisperContext,
    language: Option<String>,
}

impl Recognizer {
    /// 加载 Whisper GGML 模型。
    ///
    /// language 使用 ISO 639-1 语言代码，例如 "zh"、"en"；
    /// 使用 "auto" 时自动识别语言。
    pub fn load(model_path: impl AsRef<Path>, language: &str) -> Result<Self> {
        let mut context_params = WhisperContextParameters::default();

        // whisper-rs 只启用了 Vulkan feature，不启用 CUDA。
        context_params.use_gpu = true;

        let context = WhisperContext::new_with_params(model_path, context_params)
            .context("failed to load Whisper model using Vulkan-enabled backend")?;

        let language = if language.eq_ignore_ascii_case("auto") {
            None
        } else {
            Some(language.to_owned())
        };

        Ok(Self { context, language })
    }

    /// 对完整的 16 kHz、单声道 f32 PCM 音频执行一次整体识别。
    pub fn transcribe(&self, audio: &[f32]) -> Result<String> {
        if audio.is_empty() {
            bail!("audio buffer is empty");
        }

        let mut state = self
            .context
            .create_state()
            .context("failed to create Whisper state")?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        params.set_language(self.language.as_deref());
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        let thread_count = std::thread::available_parallelism()
            .map(|count| count.get().min(8) as i32)
            .unwrap_or(4);

        params.set_n_threads(thread_count);

        state
            .full(params, audio)
            .context("Whisper transcription failed")?;

        let mut text = String::new();

        for segment in state.as_iter() {
            text.push_str(&segment.to_string());
        }

        Ok(text.trim().to_owned())
    }
}

/// 输入法里的语音识别事件。录音和推理都在后台线程中完成。
pub enum SpeechEvent {
    /// 一段录音已经识别完成；空字符串表示没有识别出文本。
    Transcript(String),
    /// 麦克风、模型加载或识别出错。
    Error(String),
}

enum SpeechCommand {
    StartRecording,
    StopRecording,
    Shutdown,
}

/// 语音后台工作线程的控制端。
///
/// 工作线程独占 CPAL stream 和 Whisper 模型，Wayland 事件循环只发送命令、
/// 接收结果，不会等待麦克风或 Whisper 推理完成。
pub struct SpeechWorker {
    commands: Sender<SpeechCommand>,
    events: Receiver<SpeechEvent>,
}

impl SpeechWorker {
    /// 创建空闲的后台线程。模型会在第一次录音结束后懒加载，并在本次语音
    /// 输入会话中复用，避免每个语音片段都重新加载模型。
    pub fn spawn(language: impl Into<String>) -> Result<Self> {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let language = language.into();

        thread::Builder::new()
            .name("wayime-speech".to_owned())
            .spawn(move || speech_worker_loop(commands_rx, events_tx, language))
            .context("failed to spawn speech worker")?;

        Ok(Self {
            commands: commands_tx,
            events: events_rx,
        })
    }

    pub fn start_recording(&self) -> Result<()> {
        self.commands
            .send(SpeechCommand::StartRecording)
            .context("speech worker has stopped")
    }

    pub fn stop_recording(&self) -> Result<()> {
        self.commands
            .send(SpeechCommand::StopRecording)
            .context("speech worker has stopped")
    }

    pub fn try_recv(&self) -> std::result::Result<SpeechEvent, TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for SpeechWorker {
    fn drop(&mut self) {
        // If recording, the worker drops the CPAL stream when it receives Shutdown.
        // If Whisper is currently running, it exits immediately after that inference.
        let _ = self.commands.send(SpeechCommand::Shutdown);
    }
}

fn speech_worker_loop(
    commands: Receiver<SpeechCommand>,
    events: Sender<SpeechEvent>,
    language: String,
) {
    let mut recording: Option<StreamingRecording> = None;
    let mut recognizer: Option<Recognizer> = None;

    loop {
        // CPAL delivers samples independently. Drain its channel frequently so it
        // does not grow without bound while the user holds Space to record.
        let recording_error = recording
            .as_mut()
            .and_then(|current| current.collect_available().err());
        if let Some(error) = recording_error {
            let _ = events.send(SpeechEvent::Error(format!(
                "microphone recording failed: {error:#}"
            )));
            recording = None;
        }

        let command = if recording.is_some() {
            match commands.recv_timeout(RECORDING_POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            }
        };

        let Some(command) = command else {
            continue;
        };

        match command {
            SpeechCommand::StartRecording => {
                if recording.is_some() {
                    continue;
                }

                match StreamingRecording::start() {
                    Ok(started) => {
                        debug!("voice recording started");
                        recording = Some(started);
                    }
                    Err(error) => {
                        let _ = events.send(SpeechEvent::Error(format!(
                            "could not start microphone recording: {error:#}"
                        )));
                    }
                }
            }
            SpeechCommand::StopRecording => {
                let Some(current) = recording.take() else {
                    continue;
                };

                let result = current.finish().and_then(|audio| {
                    if recognizer.is_none() {
                        let model_path = crate::model::downloaded_path()
                            .context("Whisper model is not available locally")?;
                        recognizer = Some(Recognizer::load(model_path, &language)?);
                    }

                    recognizer
                        .as_ref()
                        .expect("recognizer was initialized above")
                        .transcribe(&audio)
                });

                match result {
                    Ok(text) => {
                        debug!(transcript_bytes = text.len(), "voice transcription completed");
                        if events.send(SpeechEvent::Transcript(text)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        warn!(%error, "voice transcription failed");
                        if events
                            .send(SpeechEvent::Error(format!("{error:#}")))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            SpeechCommand::Shutdown => break,
        }
    }

    // Dropping a recording also drops its CPAL stream and stops microphone capture.
    drop(recording);
}

/// 一个由后台线程独占的、可被键盘命令随时停止的录音。
struct StreamingRecording {
    stream: Option<cpal::Stream>,
    samples_rx: Receiver<Vec<f32>>,
    errors_rx: Receiver<String>,
    sample_rate: u32,
    audio: Vec<f32>,
}

impl StreamingRecording {
    fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default microphone/input device found")?;
        let supported_config = device
            .default_input_config()
            .context("failed to get default microphone configuration")?;

        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();
        let sample_rate = config.sample_rate;
        let channels = usize::from(config.channels);

        if sample_rate == 0 || channels == 0 {
            bail!("microphone reported an invalid sample rate or channel count");
        }

        let (samples_tx, samples_rx) = mpsc::channel::<Vec<f32>>();
        let (errors_tx, errors_rx) = mpsc::channel::<String>();
        let stream = build_device_stream(
            &device,
            &config,
            channels,
            sample_format,
            samples_tx,
            errors_tx,
        )?;
        stream
            .play()
            .context("failed to start microphone recording")?;

        Ok(Self {
            stream: Some(stream),
            samples_rx,
            errors_rx,
            sample_rate,
            audio: Vec::new(),
        })
    }

    fn collect_available(&mut self) -> Result<()> {
        if let Ok(error) = self.errors_rx.try_recv() {
            bail!("microphone stream failed: {error}");
        }

        loop {
            match self.samples_rx.try_recv() {
                Ok(chunk) => self.audio.extend(chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) if self.stream.is_some() => {
                    bail!("microphone input stream stopped unexpectedly");
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }

        Ok(())
    }

    fn finish(mut self) -> Result<Vec<f32>> {
        // Stop the callback before draining the final queued chunks.
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }

        for chunk in self.samples_rx.try_iter() {
            self.audio.extend(chunk);
        }

        if let Ok(error) = self.errors_rx.try_recv() {
            bail!("microphone stream failed: {error}");
        }

        let audio = resample_linear(&self.audio, self.sample_rate, WHISPER_SAMPLE_RATE);
        if audio.is_empty() {
            bail!("recorded audio is empty");
        }

        Ok(audio)
    }
}

/// 建立一个输入流，将设备样本格式统一转换为单声道 f32。
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    samples_tx: Sender<Vec<f32>>,
    errors_tx: Sender<String>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let stream = device
        .build_input_stream::<T, _, _>(
            *config,
            move |input: &[T], _| {
                let mono: Vec<f32> = input
                    .chunks_exact(channels)
                    .map(|frame| {
                        frame
                            .iter()
                            .copied()
                            .map(|sample| sample.to_sample::<f32>())
                            .sum::<f32>()
                            / channels as f32
                    })
                    .collect();

                if !mono.is_empty() {
                    // 音频回调只提交采样块，不执行模型推理。
                    let _ = samples_tx.send(mono);
                }
            },
            move |error| {
                let _ = errors_tx.send(error.to_string());
            },
            None,
        )
        .context("failed to create microphone input stream")?;

    Ok(stream)
}

fn build_device_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    sample_format: SampleFormat,
    samples_tx: Sender<Vec<f32>>,
    errors_tx: Sender<String>,
) -> Result<cpal::Stream> {
    Ok(match sample_format {
        SampleFormat::F32 => {
            build_input_stream::<f32>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::F64 => {
            build_input_stream::<f64>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::I8 => build_input_stream::<i8>(device, config, channels, samples_tx, errors_tx),
        SampleFormat::I16 => {
            build_input_stream::<i16>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::I24 => {
            build_input_stream::<I24>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::I32 => {
            build_input_stream::<i32>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::I64 => {
            build_input_stream::<i64>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::U8 => build_input_stream::<u8>(device, config, channels, samples_tx, errors_tx),
        SampleFormat::U16 => {
            build_input_stream::<u16>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::U24 => {
            build_input_stream::<U24>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::U32 => {
            build_input_stream::<u32>(device, config, channels, samples_tx, errors_tx)
        }
        SampleFormat::U64 => {
            build_input_stream::<u64>(device, config, channels, samples_tx, errors_tx)
        }
        format => bail!("unsupported microphone sample format: {format}"),
    }?)
}

/// 录制默认输入设备，直到取得 10 秒的单声道采样数据。
fn record_audio() -> Result<Vec<f32>> {
    let host = cpal::default_host();

    let device = host
        .default_input_device()
        .context("no default microphone/input device found")?;

    let supported_config = device
        .default_input_config()
        .context("failed to get default microphone configuration")?;

    let sample_format = supported_config.sample_format();
    let config: cpal::StreamConfig = supported_config.into();

    let sample_rate = config.sample_rate;
    let channels = usize::from(config.channels);

    if sample_rate == 0 || channels == 0 {
        bail!("microphone reported an invalid sample rate or channel count");
    }

    let (samples_tx, samples_rx) = mpsc::channel::<Vec<f32>>();
    let (errors_tx, errors_rx) = mpsc::channel::<String>();
    let stream = build_device_stream(
        &device,
        &config,
        channels,
        sample_format,
        samples_tx,
        errors_tx,
    )?;

    let target_samples = (sample_rate as usize).saturating_mul(RECORD_SECONDS as usize);
    let mut audio = Vec::with_capacity(target_samples);

    eprintln!("Recording for {RECORD_SECONDS} seconds. Speak now...");

    stream
        .play()
        .context("failed to start microphone recording")?;

    // 收够准确的采样点数，同时为设备初始化/丢失数据留出超时保护。
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECONDS + 5);

    while audio.len() < target_samples {
        if let Ok(error) = errors_rx.try_recv() {
            bail!("microphone stream failed: {error}");
        }

        let now = Instant::now();

        if now >= deadline {
            bail!(
                "timed out recording audio: received {} of {} samples",
                audio.len(),
                target_samples
            );
        }

        let timeout = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));

        match samples_rx.recv_timeout(timeout) {
            Ok(chunk) => audio.extend(chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                bail!("microphone input stream stopped unexpectedly");
            }
        }
    }

    // 最后一个回调可能多送了几个采样点。
    audio.truncate(target_samples);
    drop(stream);

    let audio = resample_linear(&audio, sample_rate, WHISPER_SAMPLE_RATE);

    if audio.is_empty() {
        bail!("recorded audio is empty");
    }

    Ok(audio)
}

/// 线性重采样。第一版无需额外的音频容器或重采样库。
fn resample_linear(input: &[f32], input_rate: u32, output_rate: u32) -> Vec<f32> {
    if input.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }

    if input_rate == output_rate {
        return input.to_vec();
    }

    let output_len = (input.len() as u64 * output_rate as u64) / input_rate as u64;

    let mut output = Vec::with_capacity(output_len as usize);

    for i in 0..output_len as usize {
        let source_position = i as f64 * input_rate as f64 / output_rate as f64;

        let left = (source_position.floor() as usize).min(input.len() - 1);

        let right = (left + 1).min(input.len() - 1);
        let fraction = (source_position - left as f64) as f32;

        output.push(input[left] + (input[right] - input[left]) * fraction);
    }

    output
}

/// CLI 一次性识别流程：录音 10 秒，然后整体转写并将文字写到 stdout。
pub fn run(model_path: &Path, language: &str) -> Result<()> {
    if !model_path.is_file() {
        bail!(
            "Whisper model file does not exist: {}",
            model_path.display()
        );
    }

    let audio = record_audio()?;

    eprintln!("Loading Whisper model...");
    let recognizer = Recognizer::load(model_path, language)?;

    eprintln!("Transcribing...");
    let transcript = recognizer.transcribe(&audio)?;

    println!("{transcript}");

    Ok(())
}

