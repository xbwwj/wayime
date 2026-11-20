use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
        OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, SampleFormat, SizedSample, I24, U24,
};
use ferrous_opencc::{config::BuiltinConfig, OpenCC};
use regex::Regex;
use tracing::{debug, warn};

const RECORD_SECONDS: u64 = 10;
const WHISPER_SAMPLE_RATE: u32 = 16_000;
const RECORDING_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// whisper.cpp CLI executable environment variable.
const WHISPER_CLI_ENV: &str = "WAYIME_WHISPER_CLI";

/// 输入法里的语音识别事件。
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
/// 工作线程独占 CPAL stream，并负责启动 whisper-cli 子进程。
/// wayime 自身不加载 Whisper 模型，也不持有 Whisper GPU 资源。
pub struct SpeechWorker {
    commands: Sender<SpeechCommand>,
    events: Receiver<SpeechEvent>,
}

impl SpeechWorker {
    /// 创建后台录音/识别协调线程。
    ///
    /// Whisper 模型仅由 whisper-cli 在执行识别时加载。
    /// 同一语音会话可以复用此工作线程，但每段音频都会启动独立的 CLI 进程。
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
        // If recording or transcribing, the worker receives Shutdown and stops
        // the microphone stream or terminates the whisper-cli child process.
        let _ = self.commands.send(SpeechCommand::Shutdown);
    }
}

fn speech_worker_loop(
    commands: Receiver<SpeechCommand>,
    events: Sender<SpeechEvent>,
    language: String,
) {
    let mut recording: Option<StreamingRecording> = None;

    // Resolve the model path once per voice session. This only locates the model
    // file; whisper-cli is responsible for loading and releasing the model.
    let mut model_path: Option<PathBuf> = None;

    loop {
        // CPAL independently submits samples. Drain the channel frequently while
        // recording to prevent unbounded channel growth.
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
                    if model_path.is_none() {
                        model_path = Some(
                            crate::model::downloaded_path()
                                .context("Whisper model is not available locally")?,
                        );
                    }

                    transcribe_with_cli(
                        model_path
                            .as_deref()
                            .expect("model path was initialized above"),
                        &audio,
                        &language,
                        Some(&commands),
                    )
                });

                match result {
                    Ok(Some(text)) => {
                        debug!(
                            transcript_bytes = text.len(),
                            "voice transcription completed"
                        );

                        if events.send(SpeechEvent::Transcript(text)).is_err() {
                            break;
                        }
                    }

                    // None means the worker received Shutdown and terminated
                    // the whisper-cli child process.
                    Ok(None) => break,

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

    // Dropping recording also drops the CPAL stream and stops microphone capture.
    drop(recording);
}

/// 将 16 kHz 单声道 f32 音频写为标准 PCM16 WAV 文件。
///
/// whisper.cpp CLI 可以直接读取 WAV，不需要额外音频容器库。
fn write_wav(path: &Path, audio: &[f32]) -> Result<()> {
    if audio.is_empty() {
        bail!("audio buffer is empty");
    }

    let data_size = audio
        .len()
        .checked_mul(std::mem::size_of::<i16>())
        .and_then(|size| u32::try_from(size).ok())
        .context("recorded audio is too large for a WAV file")?;

    let riff_size = 36_u32
        .checked_add(data_size)
        .context("WAV file is too large")?;

    let byte_rate = WHISPER_SAMPLE_RATE * 2;

    let file = File::create(path)
        .with_context(|| format!("failed to create temporary WAV file: {}", path.display()))?;

    let mut writer = BufWriter::new(file);

    // RIFF header.
    writer.write_all(b"RIFF")?;
    writer.write_all(&riff_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;

    // fmt chunk: PCM, mono, 16 kHz, 16-bit.
    writer.write_all(b"fmt ")?;
    writer.write_all(&16_u32.to_le_bytes())?;
    writer.write_all(&1_u16.to_le_bytes())?;
    writer.write_all(&1_u16.to_le_bytes())?;
    writer.write_all(&WHISPER_SAMPLE_RATE.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&2_u16.to_le_bytes())?;
    writer.write_all(&16_u16.to_le_bytes())?;

    // data chunk.
    writer.write_all(b"data")?;
    writer.write_all(&data_size.to_le_bytes())?;

    for &sample in audio {
        // Avoid writing NaN/Inf into the WAV file.
        let sample = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };

        let pcm = if sample <= -1.0 {
            i16::MIN
        } else {
            (sample * i16::MAX as f32).round() as i16
        };

        writer.write_all(&pcm.to_le_bytes())?;
    }

    writer.flush()?;

    Ok(())
}

/// Combine the per-segment lines generated by whisper-cli.
///
/// CJK text does not need an inserted separator. For adjacent ASCII word
/// characters, insert a space because whisper.cpp's text-file writer strips
/// the leading whitespace from each segment.
fn join_transcript_segments(output: &str) -> String {
    let mut joined = String::new();

    for segment in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let previous = joined.chars().next_back();
        let next = segment.chars().next();

        if previous.is_some_and(|ch| ch.is_ascii_alphanumeric())
            && next.is_some_and(|ch| ch.is_ascii_alphanumeric())
        {
            joined.push(' ');
        }

        joined.push_str(segment);
    }

    joined
}

/// 规范化语音识别文本中的中英文混排格式。
///
/// 规则：
/// 1. 两个汉字之间的英文逗号替换为中文逗号。
/// 2. 汉字和 ASCII 英文字母相邻时插入一个空格。
///
/// 已经存在空格的地方不会重复插入。
fn normalize_transcript(text: &str) -> String {
    static CHINESE_COMMA: OnceLock<Regex> = OnceLock::new();
    static HAN_LATIN: OnceLock<Regex> = OnceLock::new();
    static LATIN_HAN: OnceLock<Regex> = OnceLock::new();

    let chinese_comma = CHINESE_COMMA
        .get_or_init(|| Regex::new(r"(\p{Han}),(\p{Han})").expect("valid Chinese comma regex"));

    let han_latin = HAN_LATIN.get_or_init(|| {
        Regex::new(r"(\p{Han})([A-Za-z0-9])")
            .expect("valid Han-to-ASCII-alphanumeric spacing regex")
    });

    let latin_han = LATIN_HAN.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9])(\p{Han})")
            .expect("valid ASCII-alphanumeric-to-Han spacing regex")
    });

    // 先处理中文逗号，再处理汉字与英文字母之间的空格。
    let text = chinese_comma.replace_all(text, "$1，$2");
    let text = han_latin.replace_all(&text, "$1 $2");

    latin_han.replace_all(&text, "$1 $2").into_owned()
}

/// Write audio to a temporary WAV file and invoke whisper.cpp CLI.
///
/// The optional command receiver allows the worker to terminate the child
/// process when the input method is deactivated.
fn transcribe_with_cli(
    model_path: &Path,
    audio: &[f32],
    language: &str,
    shutdown: Option<&Receiver<SpeechCommand>>,
) -> Result<Option<String>> {
    if !model_path.is_file() {
        bail!(
            "Whisper model file does not exist: {}",
            model_path.display()
        );
    }

    if audio.is_empty() {
        bail!("audio buffer is empty");
    }

    // The directory is removed automatically on success, failure, or cancellation.
    let temp_dir = tempfile::tempdir().context("failed to create temporary speech directory")?;

    let audio_path = temp_dir.path().join("recording.wav");
    let output_base = temp_dir.path().join("transcript");
    let output_path = output_base.with_extension("txt");
    let stderr_path = temp_dir.path().join("whisper.stderr");

    write_wav(&audio_path, audio)?;

    // Default to whisper-cli in PATH. An absolute or relative executable path
    // can be supplied through WAYIME_WHISPER_CLI.
    let cli_path = std::env::var_os(WHISPER_CLI_ENV).unwrap_or_else(|| "whisper-cli".into());
    let cli_display = cli_path.to_string_lossy().into_owned();

    let thread_count = thread::available_parallelism()
        .map(|count| count.get().min(8))
        .unwrap_or(4)
        .max(1);

    let stderr_file =
        File::create(&stderr_path).context("failed to create temporary whisper-cli stderr file")?;

    let mut child = Command::new(&cli_path)
        .arg("-m")
        .arg(model_path)
        .arg("-f")
        .arg(&audio_path)
        .arg("-l")
        .arg(language)
        .arg("-t")
        .arg(thread_count.to_string())
        .arg("-nt")
        .arg("-np")
        .arg("-otxt")
        .arg("-of")
        .arg(&output_base)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .with_context(|| {
            format!("failed to start {cli_display}; install whisper.cpp or set {WHISPER_CLI_ENV}")
        })?;

    // Poll instead of blocking in Child::wait(). This lets the speech worker
    // respond to Shutdown while Whisper is using the GPU.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,

            Ok(None) => {}

            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("failed to wait for whisper-cli");
            }
        }

        if let Some(commands) = shutdown {
            match commands.try_recv() {
                Ok(SpeechCommand::Shutdown) | Err(TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None);
                }

                // Start/stop commands are not expected while the UI is in
                // Transcribing state. The caller owns that state transition.
                Ok(SpeechCommand::StartRecording | SpeechCommand::StopRecording) => {}

                Err(TryRecvError::Empty) => {}
            }
        }

        thread::sleep(RECORDING_POLL_INTERVAL);
    };

    if !status.success() {
        let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
        let stderr = stderr.trim();

        if stderr.is_empty() {
            bail!("whisper-cli exited with status {status}");
        }

        bail!("whisper-cli exited with status {status}: {stderr}");
    }

    let raw_text = fs::read_to_string(&output_path)
        .with_context(|| format!("whisper-cli did not produce {}", output_path.display()))?;

    let transcript = join_transcript_segments(&raw_text);

    // Preserve the existing traditional-to-simplified conversion behavior.
    let opencc = OpenCC::from_config(BuiltinConfig::T2s)
        .context("failed to initialize OpenCC T2s converter")?;

    let simplified = opencc.convert(&transcript);

    // Apply punctuation and Chinese-English spacing normalization after OpenCC.
    Ok(Some(normalize_transcript(&simplified)))
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

        SampleFormat::I8 => {
            build_input_stream::<i8>(device, config, channels, samples_tx, errors_tx)
        }

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

        SampleFormat::U8 => {
            build_input_stream::<u8>(device, config, channels, samples_tx, errors_tx)
        }

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

/// CLI 一次性识别流程：录音 10 秒，然后写入临时 WAV 并调用 whisper-cli。
pub fn run(model_path: &Path, language: &str) -> Result<()> {
    if !model_path.is_file() {
        bail!(
            "Whisper model file does not exist: {}",
            model_path.display()
        );
    }

    let audio = record_audio()?;

    eprintln!("Transcribing with whisper-cli...");

    let transcript = transcribe_with_cli(model_path, &audio, language, None)?
        .context("Whisper CLI transcription was cancelled")?;

    println!("{transcript}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_transcript;

    #[test]
    fn replaces_english_comma_between_chinese_characters() {
        assert_eq!(normalize_transcript("你好,世界"), "你好，世界");
        assert_eq!(normalize_transcript("我,爱Rust"), "我，爱 Rust");
    }

    #[test]
    fn inserts_spaces_between_chinese_and_english() {
        assert_eq!(normalize_transcript("学习Rust语言"), "学习 Rust 语言");
        assert_eq!(
            normalize_transcript("使用Wayland开发输入法"),
            "使用 Wayland 开发输入法"
        );
        assert_eq!(normalize_transcript("Rust语言"), "Rust 语言");
        assert_eq!(normalize_transcript("中文English"), "中文 English");
    }

    #[test]
    fn preserves_english_commas_and_existing_spaces() {
        assert_eq!(normalize_transcript("Hello, world"), "Hello, world");
        assert_eq!(normalize_transcript("中文, English"), "中文, English");
        assert_eq!(normalize_transcript("中文 English"), "中文 English");
    }

    #[test]
    fn inserts_spaces_between_chinese_and_ascii_digits() {
        assert_eq!(normalize_transcript("2026年"), "2026 年");
        assert_eq!(normalize_transcript("第3章"), "第 3 章");
        assert_eq!(normalize_transcript("中文123"), "中文 123");
        assert_eq!(normalize_transcript("123中文"), "123 中文");
        assert_eq!(normalize_transcript("版本2Release"), "版本 2Release");
        assert_eq!(normalize_transcript("Rust2026年"), "Rust2026 年");
    }
}
