//! Local Whisper ASR through the `whisper-rs` binding (whisper.cpp).
//!
//! Whisper is an offline, non-streaming model, so we approximate realtime
//! subtitles with a sliding-window scheme driven by the VAD:
//!
//! * While voice is active we accumulate 16 kHz mono audio for the current
//!   utterance and re-decode the buffered window every `STEP_SECONDS`, emitting
//!   the transcript as a `Partial`.
//! * When voice activity ends we decode once more and emit the transcript as a
//!   `Final`, then drop the committed audio so the next utterance starts clean.
//!
//! The subtitle stabilizer de-duplicates and keeps the on-screen block stable.
//! The first decode also compiles any GPU kernels, so it is slower than the rest.
//!
//! Language: when the hint is `zh` (and no explicit prompt is configured) each
//! decode is seeded with a Simplified-Chinese initial prompt, because whisper's
//! raw zh output skews Traditional — see `resolve_initial_prompt`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use super::{AsrOptions, RecognitionEvent, SpeechRecognizer};
use crate::error::{CoreError, Result};

#[cfg(any(all(feature = "cuda", feature = "vulkan"), all(feature = "cuda", feature = "blas"), all(feature = "vulkan", feature = "blas")))]
compile_error!("Build each Whisper backend separately with scripts/build-engines.ps1");

const SAMPLE_RATE: u32 = 16_000;
/// Maximum audio kept for one utterance (seconds). Whisper's context window is
/// ~30 s, 12 s is a comfortable, low-latency default.
const WINDOW_SECONDS: f32 = 12.0;
/// Re-decode cadence while speaking (seconds of new audio since last run).
const STEP_SECONDS: f32 = 2.0;
/// Skip decoding utterances shorter than this (seconds).
const MIN_UTTERANCE_SECONDS: f32 = 0.5;

/// Simplified-Chinese bias prompt for `zh` decodes.
///
/// Whisper treats Simplified and Traditional as one language (`zh`) and its
/// training data skews Traditional, so raw zh output comes out Traditional.
/// This prompt steers the decoder to Simplified; whisper.cpp re-injects it on
/// every decode even with `no_context`, which matches our per-window scheme.
const ZH_INITIAL_PROMPT: &str = "以下是普通话的句子。";

/// Effective initial prompt for a recognizer instance.
///
/// An explicitly configured prompt always wins. Otherwise `zh`-family language
/// hints get the default Simplified-Chinese bias prompt; auto-detect (empty
/// language) gets none, because a Chinese prompt would poison transcripts of
/// non-Chinese audio.
fn resolve_initial_prompt(language: &str, configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    if language.to_ascii_lowercase().starts_with("zh") {
        return ZH_INITIAL_PROMPT.to_string();
    }
    String::new()
}

#[allow(dead_code)]
pub struct WhisperAsr {
    /// Owns the model weights; shared, never mutated after load.
    ctx: WhisperContext,
    /// Per-call inference state. Behind a `Mutex` so the struct stays `Sync`.
    state: Mutex<WhisperState>,
    /// Params template (language hint, initial prompt, decode settings), built
    /// once and cloned per decode. The setters store the strings as leaked C
    /// pointers inside `FullParams`, so building once also means each string is
    /// leaked exactly once instead of on every decode.
    base_params: FullParams<'static, 'static>,
    /// Monotonic audio of the current utterance (16 kHz mono f32).
    audio: Vec<f32>,
    /// Absolute session time (us) of `audio[0]`.
    audio_start_us: u64,
    /// Absolute session time up to which we last ran inference.
    last_infer_end_us: u64,
    speaking: bool,
    device: String,
}

impl WhisperAsr {
    pub async fn new(opts: AsrOptions) -> Result<Self> {
        validate_engine(&opts.engine)?;
        if opts.model.contains(".en") && !opts.language.is_empty() && opts.language != "en" {
            return Err(CoreError::Model("English-only models require English or auto language".into()));
        }
        let model_path = resolve_model(&opts.model, &opts.model_directory).await?;

        let (ctx, device) = load_context(&model_path)?;
        let state = ctx
            .create_state()
            .map_err(|e| CoreError::Model(format!("whisper state init failed: {e}")))?;

        let initial_prompt = resolve_initial_prompt(&opts.language, &opts.initial_prompt);

        tracing::info!(
            "whisper asr ready: model={} device={} language={} prompt={}",
            model_path.display(),
            device,
            if opts.language.is_empty() {
                "auto"
            } else {
                &opts.language
            },
            if initial_prompt.is_empty() {
                "none"
            } else {
                initial_prompt.as_str()
            }
        );
        println!(
            "[translator-core] whisper device: {device} (model={})",
            model_path.display()
        );

        let base_params = build_base_params(
            // Leak the language hint and prompt into 'static storage: the
            // whisper-rs setters already leak the CStrings they build, so the
            // only choice is one copy per instance vs one per decode.
            if opts.language.is_empty() {
                None
            } else {
                Some(Box::leak(opts.language.clone().into_boxed_str()))
            },
            if initial_prompt.is_empty() {
                None
            } else {
                Some(Box::leak(initial_prompt.into_boxed_str()))
            },
        );

        Ok(Self {
            ctx,
            state: Mutex::new(state),
            base_params,
            audio: Vec::new(),
            audio_start_us: 0,
            last_infer_end_us: 0,
            speaking: false,
            device,
        })
    }

    /// Params for one decode: a cheap clone of the shared template.
    fn build_params(&self) -> FullParams<'static, 'static> {
        self.base_params.clone()
    }

    /// Decode the current utterance buffer and return `(text, start_us, end_us)`
    /// for each segment in absolute session time.
    fn decode(&mut self) -> Result<Vec<(String, u64, u64)>> {
        let min_samples = (MIN_UTTERANCE_SECONDS * SAMPLE_RATE as f32) as usize;
        if self.audio.len() < min_samples {
            return Ok(Vec::new());
        }

        let params = self.build_params();
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoreError::Internal("whisper state mutex poisoned".into()))?;
        state
            .full(params, &self.audio)
            .map_err(|e| CoreError::Provider(format!("whisper inference failed: {e}")))?;

        let n = state
            .full_n_segments()
            .map_err(|e| CoreError::Provider(format!("whisper segment count: {e}")))?
            as i32;

        let mut segs = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let text = state
                .full_get_segment_text(i)
                .map_err(|e| CoreError::Provider(format!("whisper segment text: {e}")))?;
            let t0 = state
                .full_get_segment_t0(i)
                .map_err(|e| CoreError::Provider(format!("whisper t0: {e}")))?;
            let t1 = state
                .full_get_segment_t1(i)
                .map_err(|e| CoreError::Provider(format!("whisper t1: {e}")))?;
            let start_us = self.audio_start_us.saturating_add(t0 as u64 * 10_000);
            let end_us = self.audio_start_us.saturating_add(t1 as u64 * 10_000);
            segs.push((text.trim().to_string(), start_us, end_us));
        }
        Ok(segs)
    }
}

#[async_trait]
impl SpeechRecognizer for WhisperAsr {
    fn name(&self) -> &str {
        "whisper"
    }

    async fn push_audio(
        &mut self,
        samples: &[f32],
        speech: bool,
        end_us: u64,
    ) -> Result<Vec<RecognitionEvent>> {
        let mut events = Vec::new();

        // Append and keep at most one utterance's worth of audio.
        self.audio.extend_from_slice(samples);
        let cap = (WINDOW_SECONDS * SAMPLE_RATE as f32) as usize;
        if self.audio.len() > cap {
            let drop = self.audio.len() - cap;
            self.audio.drain(0..drop);
            self.audio_start_us += drop as u64 * 1_000_000 / SAMPLE_RATE as u64;
        }

        if speech {
            if !self.speaking {
                self.speaking = true;
            }
            let since = end_us.saturating_sub(self.last_infer_end_us);
            if since >= (STEP_SECONDS * 1_000_000.0) as u64 {
                self.last_infer_end_us = end_us;
                let segs = self.decode()?;
                let text = join_segments(&segs);
                if !text.is_empty() {
                    events.push(RecognitionEvent::Partial {
                        text,
                        start_us: self.audio_start_us,
                        end_us,
                    });
                }
            }
        } else if self.speaking {
            self.speaking = false;
            self.last_infer_end_us = end_us;
            let segs = self.decode()?;
            let text = join_segments(&segs);
            if !text.is_empty() {
                let start = segs.first().map(|s| s.1).unwrap_or(self.audio_start_us);
                events.push(RecognitionEvent::Final {
                    text,
                    start_us: start,
                    end_us,
                    confidence: 1.0,
                });
            }
            // Commit: discard the processed audio so the next utterance is fresh.
            self.audio.clear();
            self.audio_start_us = end_us;
        }

        Ok(events)
    }

    async fn flush(&mut self) -> Result<Vec<RecognitionEvent>> {
        if !self.speaking && self.audio.is_empty() {
            return Ok(Vec::new());
        }
        self.speaking = false;
        self.last_infer_end_us = self.audio_end_us();
        let segs = self.decode()?;
        let text = join_segments(&segs);
        let mut events = Vec::new();
        if !text.is_empty() {
            let start = segs.first().map(|s| s.1).unwrap_or(self.audio_start_us);
            events.push(RecognitionEvent::Final {
                text,
                start_us: start,
                end_us: self.audio_end_us(),
                confidence: 1.0,
            });
        }
        self.audio.clear();
        Ok(events)
    }
}

impl WhisperAsr {
    fn audio_end_us(&self) -> u64 {
        self.audio_start_us + self.audio.len() as u64 * 1_000_000 / SAMPLE_RATE as u64
    }
}

fn join_segments(segs: &[(String, u64, u64)]) -> String {
    segs.iter()
        .map(|(t, _, _)| t.as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Locate (downloading if necessary) the requested Whisper model file.
const MODELS: &[&str] = &["tiny", "tiny.en", "base", "base.en", "small", "small.en",
    "medium", "medium.en", "large", "large-v1", "large-v2", "large-v3"];

fn model_filename(name: &str) -> Result<String> {
    let id = name.strip_prefix("ggml-").unwrap_or(name);
    let id = id.strip_suffix(".bin").unwrap_or(id);
    let id = if id.is_empty() { "tiny" } else if id == "large" { "large-v3" } else { id };
    if !MODELS.contains(&id) { return Err(CoreError::Model(format!("Unsupported Whisper model: {name}"))); }
    Ok(format!("ggml-{id}.bin"))
}

async fn resolve_model(name: &str, directory: &str) -> Result<PathBuf> {
    let filename = model_filename(name)?;
    // Explicit UI selection takes precedence over legacy environment overrides.
    if directory.is_empty() {
        if let Ok(explicit) = std::env::var("TRANSLATOR_WHISPER_MODEL") {
            let path = PathBuf::from(explicit);
            if path.is_file() { return Ok(path); }
            return Err(CoreError::Model(format!("Model does not exist: {path:?}")));
        }
    }
    let dir = if directory.is_empty() { models_dir() } else { PathBuf::from(directory) };
    std::fs::create_dir_all(&dir).map_err(|e| CoreError::Model(format!("Cannot create model directory: {e}")))?;
    let dest = dir.join(filename);
    if dest.is_file() && std::fs::metadata(&dest).map(|m| m.len() > 4).unwrap_or(false) { return Ok(dest); }
    let base = std::env::var("TRANSLATOR_MODEL_BASE_URL")
        .unwrap_or_else(|_| "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/".into());
    download(&format!("{}/{}", base.trim_end_matches('/'), dest.file_name().unwrap().to_string_lossy()), &dest).await?;
    Ok(dest)
}

fn models_dir() -> PathBuf {
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    dir.push("models");
    dir
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    use std::io::Write;
    let mut resp = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(30))
        .build().map_err(|e| CoreError::Model(e.to_string()))?
        .get(url).send().await.map_err(|e| CoreError::Model(format!("Download failed: {e}")))?
        .error_for_status().map_err(|e| CoreError::Model(e.to_string()))?;
    let total = resp.content_length();
    let tmp = dest.with_extension(format!("{}.part", std::process::id()));
    let result = async {
        let mut file = std::fs::File::create(&tmp).map_err(|e| CoreError::Model(e.to_string()))?;
        let mut received = 0u64;
        while let Some(chunk) = resp.chunk().await.map_err(|e| CoreError::Model(e.to_string()))? {
            file.write_all(&chunk).map_err(|e| CoreError::Model(e.to_string()))?;
            received += chunk.len() as u64;
        }
        if received < 4 || total.is_some_and(|n| n != received) {
            return Err(CoreError::Model("Incomplete model download".into()));
        }
        file.sync_all().map_err(|e| CoreError::Model(e.to_string()))?;
        drop(file);
        let magic = { use std::io::Read; let mut f = std::fs::File::open(&tmp).map_err(|e| CoreError::Model(e.to_string()))?;
            let mut magic = [0; 4]; f.read_exact(&mut magic).map_err(|e| CoreError::Model(e.to_string()))?; magic };
        if magic != *b"lmgg" { return Err(CoreError::Model("Downloaded file is not a Whisper GGML model".into())); }
        std::fs::rename(&tmp, dest).map_err(|e| CoreError::Model(e.to_string()))
    }.await;
    if result.is_err() { let _ = std::fs::remove_file(tmp); }
    result
}

#[cfg(test)]
mod model_selection_tests {
    use super::*;
    #[test]
    fn aliases_and_paths() {
        assert_eq!(model_filename("large").unwrap(), "ggml-large-v3.bin");
        assert_eq!(model_filename("ggml-tiny.en.bin").unwrap(), "ggml-tiny.en.bin");
        assert!(model_filename("../other.bin").is_err());
        assert!(model_filename("https://example.org/model").is_err());
    }
    #[test]
    fn engine_mismatch_is_not_silent_fallback() {
        assert!(validate_engine(engine_id()).is_ok());
        assert!(validate_engine("unknown").is_err());
    }
}

/// `WhisperContextParameters` with the GPU explicitly on or off.
/// `'static` is fine: with default DTW parameters nothing is borrowed.
fn context_params(use_gpu: bool) -> WhisperContextParameters<'static> {
    WhisperContextParameters {
        use_gpu,
        ..Default::default()
    }
}

/// Decode settings shared by every window of a recognizer instance.
///
/// `language` / `initial_prompt` may be `None`; passed-in strings must be
/// `&'static` because `FullParams` stores them as leaked C pointers.
fn build_base_params(
    language: Option<&'static str>,
    initial_prompt: Option<&'static str>,
) -> FullParams<'static, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(language);
    params.set_translate(false);
    params.set_token_timestamps(true);
    params.set_print_progress(false);
    params.set_suppress_blank(true);
    // Each window is an independent utterance; do not carry context across.
    // The initial prompt (if any) still applies — whisper.cpp injects it after
    // clearing the carried-over context.
    params.set_no_context(true);
    params.set_max_len(0);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    params.set_n_threads(threads as i32);
    if let Some(prompt) = initial_prompt {
        params.set_initial_prompt(prompt);
    }
    params
}

/// Each shipped engine has an isolated executable and native dependencies.
pub fn engine_id() -> &'static str {
    if cfg!(feature = "cuda") { "cuda" }
    else if cfg!(feature = "vulkan") { "vulkan" }
    else if cfg!(feature = "blas") { "blas" }
    else { "cpu" }
}

fn validate_engine(requested: &str) -> Result<()> {
    if !requested.is_empty() && requested != engine_id() {
        return Err(CoreError::Unsupported(format!(
            "Requested {requested} engine, but this core contains {}. Install the matching engine package.", engine_id())));
    }
    Ok(())
}

fn load_context(path: &Path) -> Result<(WhisperContext, String)> {
    #[cfg(feature = "cuda")]
    if !cuda_device_available() {
        return Err(CoreError::Model("CUDA engine requires a usable NVIDIA GPU and driver; select CPU explicitly to continue".into()));
    }
    #[cfg(feature = "vulkan")]
    if whisper_rs::vulkan::list_devices().is_empty() {
        return Err(CoreError::Model("Vulkan engine found no compatible GPU; update the driver or select CPU".into()));
    }
    let gpu = cfg!(any(feature = "cuda", feature = "vulkan"));
    let ctx = WhisperContext::new_with_params(path.to_str().unwrap_or_default(), context_params(gpu))
        .map_err(|e| CoreError::Model(format!("{} engine initialization failed: {e}", engine_id())))?;
    Ok((ctx, engine_id().to_uppercase()))
}

/// True when an NVIDIA driver with at least one usable GPU is present.
///
/// Probed by loading `nvcuda.dll` (installed with the driver, not the toolkit)
/// and calling `cuInit` + `cuDeviceGetCount`. This mirrors the check whisper.cpp
/// does internally, minus its silent CPU fallback.
#[cfg(all(windows, feature = "cuda"))]
fn cuda_device_available() -> bool {
    use core::ffi::{c_int, c_uint, c_void};

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    let dll_name: Vec<u16> = "nvcuda.dll\0".encode_utf16().collect();
    let dll = unsafe { LoadLibraryW(dll_name.as_ptr()) };
    if dll.is_null() {
        return false;
    }

    unsafe {
        let cu_init_ptr = GetProcAddress(dll, b"cuInit\0".as_ptr());
        let cu_count_ptr = GetProcAddress(dll, b"cuDeviceGetCount\0".as_ptr());
        if cu_init_ptr.is_null() || cu_count_ptr.is_null() {
            return false;
        }
        // CUDA driver API is stdcall on Windows; identical to C on x64.
        let cu_init: unsafe extern "system" fn(c_uint) -> c_int = core::mem::transmute(cu_init_ptr);
        let cu_count: unsafe extern "system" fn(*mut c_int) -> c_int =
            core::mem::transmute(cu_count_ptr);

        if cu_init(0) != 0 {
            return false;
        }
        let mut count: c_int = 0;
        cu_count(&mut count) == 0 && count > 0
    }
}

/// Non-Windows CUDA build: no cheap probe, attempt GPU and let whisper.cpp decide.
#[cfg(all(not(windows), feature = "cuda"))]
fn cuda_device_available() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::resolve_initial_prompt;

    #[test]
    fn zh_language_gets_the_simplified_bias_prompt() {
        assert!(!resolve_initial_prompt("zh", "").is_empty());
        assert!(!resolve_initial_prompt("zh-CN", "").is_empty());
        assert!(!resolve_initial_prompt("ZH", "").is_empty());
    }

    #[test]
    fn non_chinese_and_auto_get_no_prompt() {
        assert_eq!(resolve_initial_prompt("en", ""), "");
        assert_eq!(resolve_initial_prompt("ja", ""), "");
        assert_eq!(resolve_initial_prompt("", ""), "");
    }

    #[test]
    fn configured_prompt_always_wins() {
        assert_eq!(resolve_initial_prompt("zh", "自定义提示"), "自定义提示");
        assert_eq!(resolve_initial_prompt("en", "english prompt"), "english prompt");
    }
}
