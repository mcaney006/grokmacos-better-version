#![allow(dead_code)]
//! Cross-platform audio engine for voice mode.
//!
//! `VoiceAudio` owns the live cpal streams (which are `!Send`) and lives on
//! the UI thread; the parts that need to cross into async tasks live in
//! [`VoiceShared`], a cheap-to-clone bundle of channels + atomics that is
//! `Send + Sync`.
//!
//! Pipelines:
//! 1. **Capture** — pulls mono PCM16 @ 24 kHz from the default input device,
//!    resamples if needed, and pushes frames onto `capture_rx` for the WS
//!    uplink to consume.
//! 2. **Playback** — receives PCM16 chunks via `playback_tx`, resamples to
//!    the device's native rate, and writes through cpal to the default
//!    output device using a small ring buffer for jitter resilience.
//!
//! Both pipelines write a smoothed RMS level into a shared atomic so the
//! UI's waveform widget can render without inspecting raw samples.

use crate::error::AudioError;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

pub const VOICE_SAMPLE_RATE: u32 = 24_000;

/// Depth of the cpal-callback → bridge-thread capture queue. At
/// VOICE_SAMPLE_RATE × 2 bytes/sample × mono, 32 frames is about
/// 0.7 s of audio worst-case — generous slack against jitter, small
/// enough that we never queue stale audio nor pin RAM. The cpal
/// callback uses `try_send`: when this fills up (because the WS sink
/// stalled), we drop the newest frame on the audio thread. Real-time
/// audio prefers stale-data-loss over latency growth.
pub const CAPTURE_QUEUE_DEPTH: usize = 32;

/// Shared, `Send + Sync` view of an active voice audio engine. Cheap to clone.
#[derive(Clone)]
pub struct VoiceShared {
    pub capture_rx: Receiver<Vec<i16>>,
    pub playback_tx: Sender<Vec<i16>>,
    pub level_in: LevelMeter,
    pub level_out: LevelMeter,
    pub speaking: Arc<AtomicBool>,
}

/// RMS meter shared across threads. Stores `f32` bits in an `AtomicU32`.
#[derive(Clone, Default)]
pub struct LevelMeter {
    bits: Arc<AtomicU32>,
}

impl LevelMeter {
    pub fn set(&self, level: f32) {
        self.bits.store(level.to_bits(), Ordering::Relaxed);
    }
    pub fn level(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

/// Owns the live `cpal::Stream` handles, which are `!Send`. Keep this on the
/// UI thread; share `VoiceShared` with background tasks.
pub struct VoiceAudio {
    input: Option<Stream>,
    output: Option<Stream>,
    shared: VoiceShared,
}

impl VoiceAudio {
    pub fn new() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let input_dev = host.default_input_device().ok_or(AudioError::NoInput)?;
        let output_dev = host.default_output_device().ok_or(AudioError::NoOutput)?;

        let level_in = LevelMeter::default();
        let level_out = LevelMeter::default();
        let speaking = Arc::new(AtomicBool::new(false));

        // Bounded so a stalled WS sink can't grow RAM forever. See
        // CAPTURE_QUEUE_DEPTH; the input stream uses `try_send` and drops
        // on full.
        let (capture_tx, capture_rx) = crossbeam_channel::bounded::<Vec<i16>>(CAPTURE_QUEUE_DEPTH);
        let (playback_tx, playback_rx) = crossbeam_channel::unbounded::<Vec<i16>>();

        let input = build_input_stream(&input_dev, capture_tx, level_in.clone())?;
        let output = build_output_stream(
            &output_dev,
            playback_rx,
            level_out.clone(),
            speaking.clone(),
        )?;

        input.play().map_err(|e| AudioError::Play(e.to_string()))?;
        output.play().map_err(|e| AudioError::Play(e.to_string()))?;

        Ok(Self {
            input: Some(input),
            output: Some(output),
            shared: VoiceShared {
                capture_rx,
                playback_tx,
                level_in,
                level_out,
                speaking,
            },
        })
    }

    pub fn shared(&self) -> VoiceShared {
        self.shared.clone()
    }

    pub fn stop(&mut self) {
        self.input.take();
        self.output.take();
    }
}

impl Drop for VoiceAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_input_stream(
    dev: &cpal::Device,
    out_tx: Sender<Vec<i16>>,
    meter: LevelMeter,
) -> Result<Stream, AudioError> {
    let supported = dev
        .default_input_config()
        .map_err(|e| AudioError::Config(e.to_string()))?;
    let sample_format = supported.sample_format();
    let native_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config: StreamConfig = supported.into();

    let resampler = if native_rate != VOICE_SAMPLE_RATE {
        Some(Arc::new(Mutex::new(VoiceResampler::new(
            native_rate as f32,
            VOICE_SAMPLE_RATE as f32,
        ))))
    } else {
        None
    };

    let err_fn = |e| tracing::warn!(error = %e, "input stream error");

    let stream = match sample_format {
        SampleFormat::F32 => dev.build_input_stream(
            &config,
            move |data: &[f32], _| {
                let mono = to_mono_f32(data, channels);
                update_level_f32(&mono, &meter);
                let mono = if let Some(r) = resampler.as_ref() {
                    r.lock().process(&mono)
                } else {
                    mono
                };
                let pcm: Vec<i16> = mono.iter().map(|s| f32_to_i16(*s)).collect();
                // `try_send` instead of `send`: if the consumer is behind
                // we drop the frame rather than block the cpal callback
                // thread (real-time audio principle: stale audio helps
                // nobody; latency growth is the worse failure mode).
                let _ = out_tx.try_send(pcm);
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => dev.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let floats: Vec<f32> = data.iter().map(|s| i16_to_f32(*s)).collect();
                let mono = to_mono_f32(&floats, channels);
                update_level_f32(&mono, &meter);
                let mono = if let Some(r) = resampler.as_ref() {
                    r.lock().process(&mono)
                } else {
                    mono
                };
                let pcm: Vec<i16> = mono.iter().map(|s| f32_to_i16(*s)).collect();
                // `try_send` instead of `send`: if the consumer is behind
                // we drop the frame rather than block the cpal callback
                // thread (real-time audio principle: stale audio helps
                // nobody; latency growth is the worse failure mode).
                let _ = out_tx.try_send(pcm);
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => dev.build_input_stream(
            &config,
            move |data: &[u16], _| {
                let floats: Vec<f32> = data.iter().map(|s| Sample::to_sample::<f32>(*s)).collect();
                let mono = to_mono_f32(&floats, channels);
                update_level_f32(&mono, &meter);
                let mono = if let Some(r) = resampler.as_ref() {
                    r.lock().process(&mono)
                } else {
                    mono
                };
                let pcm: Vec<i16> = mono.iter().map(|s| f32_to_i16(*s)).collect();
                // `try_send` instead of `send`: if the consumer is behind
                // we drop the frame rather than block the cpal callback
                // thread (real-time audio principle: stale audio helps
                // nobody; latency growth is the worse failure mode).
                let _ = out_tx.try_send(pcm);
            },
            err_fn,
            None,
        ),
        fmt => {
            return Err(AudioError::Config(format!(
                "unsupported sample format {fmt:?}"
            )))
        }
    }
    .map_err(|e| AudioError::BuildStream(e.to_string()))?;

    Ok(stream)
}

fn build_output_stream(
    dev: &cpal::Device,
    rx: Receiver<Vec<i16>>,
    meter: LevelMeter,
    speaking: Arc<AtomicBool>,
) -> Result<Stream, AudioError> {
    let supported = dev
        .default_output_config()
        .map_err(|e| AudioError::Config(e.to_string()))?;
    let sample_format = supported.sample_format();
    let native_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config: StreamConfig = supported.into();

    let ring = Arc::new(Mutex::new(RingBuffer::new()));
    let resampler = if native_rate != VOICE_SAMPLE_RATE {
        Some(Arc::new(Mutex::new(VoiceResampler::new(
            VOICE_SAMPLE_RATE as f32,
            native_rate as f32,
        ))))
    } else {
        None
    };

    {
        let ring = ring.clone();
        let resampler = resampler.clone();
        std::thread::Builder::new()
            .name("voice-mixer".into())
            .spawn(move || {
                while let Ok(chunk) = rx.recv() {
                    let floats: Vec<f32> = chunk.iter().map(|s| i16_to_f32(*s)).collect();
                    let resampled = if let Some(r) = resampler.as_ref() {
                        r.lock().process(&floats)
                    } else {
                        floats
                    };
                    ring.lock().push(&resampled);
                }
            })
            .ok();
    }

    let err_fn = |e| tracing::warn!(error = %e, "output stream error");

    let stream = match sample_format {
        SampleFormat::F32 => dev.build_output_stream(
            &config,
            {
                let ring = ring.clone();
                let meter = meter.clone();
                let speaking = speaking.clone();
                move |data: &mut [f32], _| {
                    let frames = data.len() / channels.max(1);
                    let mono = ring.lock().pop(frames);
                    speaking.store(!mono.iter().all(|s| s.abs() < 1e-5), Ordering::Relaxed);
                    update_level_f32(&mono, &meter);
                    fan_out_f32(data, &mono, channels);
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => dev.build_output_stream(
            &config,
            {
                let ring = ring.clone();
                let meter = meter.clone();
                let speaking = speaking.clone();
                move |data: &mut [i16], _| {
                    let frames = data.len() / channels.max(1);
                    let mono = ring.lock().pop(frames);
                    speaking.store(!mono.iter().all(|s| s.abs() < 1e-5), Ordering::Relaxed);
                    update_level_f32(&mono, &meter);
                    fan_out_i16(data, &mono, channels);
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => dev.build_output_stream(
            &config,
            {
                let ring = ring.clone();
                let meter = meter.clone();
                let speaking = speaking.clone();
                move |data: &mut [u16], _| {
                    let frames = data.len() / channels.max(1);
                    let mono = ring.lock().pop(frames);
                    speaking.store(!mono.iter().all(|s| s.abs() < 1e-5), Ordering::Relaxed);
                    update_level_f32(&mono, &meter);
                    for (frame_idx, sample) in mono.into_iter().enumerate() {
                        let s: u16 = Sample::from_sample(sample);
                        for ch in 0..channels {
                            data[frame_idx * channels + ch] = s;
                        }
                    }
                }
            },
            err_fn,
            None,
        ),
        fmt => {
            return Err(AudioError::Config(format!(
                "unsupported sample format {fmt:?}"
            )))
        }
    }
    .map_err(|e| AudioError::BuildStream(e.to_string()))?;

    Ok(stream)
}

// --- helpers ---------------------------------------------------------------

fn to_mono_f32(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

fn fan_out_f32(out: &mut [f32], mono: &[f32], channels: usize) {
    for (frame_idx, sample) in mono.iter().copied().enumerate() {
        for ch in 0..channels {
            let idx = frame_idx * channels + ch;
            if idx < out.len() {
                out[idx] = sample;
            }
        }
    }
    let filled = mono.len() * channels;
    for slot in out.iter_mut().skip(filled) {
        *slot = 0.0;
    }
}

fn fan_out_i16(out: &mut [i16], mono: &[f32], channels: usize) {
    for (frame_idx, sample) in mono.iter().copied().enumerate() {
        let s = f32_to_i16(sample);
        for ch in 0..channels {
            let idx = frame_idx * channels + ch;
            if idx < out.len() {
                out[idx] = s;
            }
        }
    }
    let filled = mono.len() * channels;
    for slot in out.iter_mut().skip(filled) {
        *slot = 0;
    }
}

fn i16_to_f32(s: i16) -> f32 {
    s as f32 / i16::MAX as f32
}

fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

fn update_level_f32(samples: &[f32], meter: &LevelMeter) {
    if samples.is_empty() {
        return;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    let db = 20.0 * (rms.max(1e-6)).log10();
    let norm = ((db + 50.0) / 50.0).clamp(0.0, 1.0);
    let prev = meter.level();
    meter.set(prev * 0.6 + norm * 0.4);
}

// --- ring buffer + linear resampler ----------------------------------------

struct RingBuffer {
    buf: std::collections::VecDeque<f32>,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(48_000),
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.buf.extend(samples.iter().copied());
        let cap = 96_000;
        while self.buf.len() > cap {
            self.buf.pop_front();
        }
    }

    fn pop(&mut self, n: usize) -> Vec<f32> {
        let take = n.min(self.buf.len());
        let mut out = Vec::with_capacity(n);
        for _ in 0..take {
            out.push(self.buf.pop_front().unwrap_or(0.0));
        }
        out.resize(n, 0.0);
        out
    }
}

/// Resampler used by both capture (native→24 kHz) and playback (24 kHz→
/// native) pipelines. Always-on lightweight linear interpolator by
/// default; sinc-interpolated (rubato) under `--features hq-resample`.
///
/// Both variants expose the same `process(&[f32]) -> Vec<f32>` shape so
/// the call sites in `build_input_stream` / `build_output_stream` don't
/// branch on the implementation.
pub(crate) enum VoiceResampler {
    Linear(LinearResampler),
    #[cfg(feature = "hq-resample")]
    Sinc(SincResampler),
}

impl VoiceResampler {
    pub(crate) fn new(src_rate: f32, dst_rate: f32) -> Self {
        #[cfg(feature = "hq-resample")]
        {
            match SincResampler::new(src_rate, dst_rate) {
                Ok(s) => return VoiceResampler::Sinc(s),
                Err(e) => {
                    tracing::warn!(error = %e, "rubato init failed, falling back to linear");
                }
            }
        }
        VoiceResampler::Linear(LinearResampler::new(src_rate, dst_rate))
    }

    pub(crate) fn process(&mut self, input: &[f32]) -> Vec<f32> {
        match self {
            VoiceResampler::Linear(r) => r.process(input),
            #[cfg(feature = "hq-resample")]
            VoiceResampler::Sinc(r) => r.process(input),
        }
    }
}

pub(crate) struct LinearResampler {
    src_rate: f32,
    dst_rate: f32,
    last_sample: f32,
    pos: f32,
}

impl LinearResampler {
    pub(crate) fn new(src_rate: f32, dst_rate: f32) -> Self {
        Self {
            src_rate,
            dst_rate,
            last_sample: 0.0,
            pos: 0.0,
        }
    }

    pub(crate) fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() || (self.src_rate - self.dst_rate).abs() < f32::EPSILON {
            return input.to_vec();
        }
        let ratio = self.src_rate / self.dst_rate;
        let mut out = Vec::with_capacity((input.len() as f32 / ratio) as usize + 1);
        while self.pos < input.len() as f32 {
            let idx = self.pos as usize;
            let frac = self.pos - idx as f32;
            let a = if idx == 0 {
                self.last_sample
            } else {
                input[idx - 1]
            };
            let b = input[idx];
            out.push(a + (b - a) * frac);
            self.pos += ratio;
        }
        self.pos -= input.len() as f32;
        if let Some(last) = input.last() {
            self.last_sample = *last;
        }
        out
    }
}

// --- sinc-interpolated resampler (rubato) ----------------------------------
//
// Only compiled when `--features hq-resample` is active. Wraps rubato's
// `SincFixedIn` resampler in a streaming front-end that matches the
// shape of `LinearResampler::process`: append-only, takes whatever's
// available, returns whatever the engine can emit.
//
// rubato's `SincFixedIn` expects a fixed input block size per call and
// returns a fixed number of output samples. To plug it into cpal's
// arbitrary-sized callback frames we keep an internal byte-rate input
// queue, pull `chunk_size` samples at a time, and concatenate the
// output. Anything that doesn't divide evenly stays in the queue for
// the next call — same backpressure pattern as the line buffer in
// `services::sse`.

#[cfg(feature = "hq-resample")]
pub(crate) struct SincResampler {
    inner: rubato::SincFixedIn<f32>,
    chunk_size: usize,
    queue: std::collections::VecDeque<f32>,
    /// Identity-only pass-through when src == dst; rubato refuses an
    /// identity ratio (the sinc kernel is undefined) so we short-circuit
    /// here rather than ask rubato to do nothing slowly.
    passthrough: bool,
}

#[cfg(feature = "hq-resample")]
impl SincResampler {
    /// Chunk size for the sinc engine. 256 samples @ 24 kHz = 10.7 ms;
    /// large enough that the per-call overhead is amortised, small
    /// enough that voice latency doesn't grow visibly.
    const CHUNK: usize = 256;

    pub(crate) fn new(src_rate: f32, dst_rate: f32) -> Result<Self, String> {
        if (src_rate - dst_rate).abs() < f32::EPSILON {
            return Ok(Self {
                inner: Self::make_inner(1.0)?,
                chunk_size: Self::CHUNK,
                queue: std::collections::VecDeque::new(),
                passthrough: true,
            });
        }
        let ratio = dst_rate as f64 / src_rate as f64;
        Ok(Self {
            inner: Self::make_inner(ratio)?,
            chunk_size: Self::CHUNK,
            queue: std::collections::VecDeque::new(),
            passthrough: false,
        })
    }

    fn make_inner(ratio: f64) -> Result<rubato::SincFixedIn<f32>, String> {
        use rubato::{
            SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
        };
        let params = SincInterpolationParameters {
            // f_cutoff = 0.95: standard anti-alias margin below Nyquist.
            // sinc_len = 256: ample stopband attenuation, ~5 ms of pre-ring.
            // oversampling_factor = 256, interpolation = Linear: smooth
            // ratio without an explosive kernel size.
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        // SincFixedIn::new(ratio, max_resample_ratio_relative, params, chunk_size, channels)
        SincFixedIn::<f32>::new(ratio, 2.0, params, Self::CHUNK, 1).map_err(|e| e.to_string())
    }

    pub(crate) fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.passthrough {
            return input.to_vec();
        }
        self.queue.extend(input.iter().copied());
        let mut out: Vec<f32> = Vec::new();
        // Pull full chunks until the queue can't satisfy one more.
        while self.queue.len() >= self.chunk_size {
            let mut buf = Vec::with_capacity(self.chunk_size);
            for _ in 0..self.chunk_size {
                if let Some(s) = self.queue.pop_front() {
                    buf.push(s);
                }
            }
            use rubato::Resampler;
            match self.inner.process(&[buf], None) {
                Ok(frames) => {
                    if let Some(ch) = frames.into_iter().next() {
                        out.extend(ch);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "rubato process failed; dropping chunk");
                }
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Identical sample rates must be a strict pass-through under both
    /// resampler backends. Guards against introducing any phase / delay
    /// in the trivial identity case (which the rubato sinc engine refuses
    /// to handle directly — the wrapper short-circuits it).
    #[test]
    fn voice_resampler_identity_rate_is_passthrough() {
        let mut r = VoiceResampler::new(24_000.0, 24_000.0);
        let input: Vec<f32> = (0..1024).map(|i| (i as f32 / 100.0).sin()).collect();
        let out = r.process(&input);
        assert_eq!(out.len(), input.len(), "identity rate must preserve length");
        for (i, (a, b)) in input.iter().zip(out.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-6,
                "identity rate must preserve sample {i}: {a} vs {b}"
            );
        }
    }

    /// Downsampling 48 kHz → 24 kHz must produce roughly half the output
    /// samples. Exact count varies by engine (the sinc resampler buffers
    /// internally until it has a full chunk), so we only assert the ratio
    /// after a warm-up that's well past the initial sinc latency.
    #[test]
    fn voice_resampler_downsample_ratio_is_approximately_half() {
        let mut r = VoiceResampler::new(48_000.0, 24_000.0);
        // 4 seconds of audio so the sinc engine has plenty of room to
        // emit chunks past its initial latency.
        let input: Vec<f32> = (0..192_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = r.process(&input);
        let ratio = out.len() as f32 / input.len() as f32;
        assert!(
            (0.45..=0.55).contains(&ratio),
            "expected ~0.5 downsample ratio, got {ratio} ({} -> {})",
            input.len(),
            out.len()
        );
    }

    /// Process must never panic on an empty input slice (cpal occasionally
    /// hands the callback a zero-length buffer during stream start-up).
    #[test]
    fn voice_resampler_handles_empty_input() {
        let mut r = VoiceResampler::new(48_000.0, 24_000.0);
        let out = r.process(&[]);
        assert!(out.is_empty());
    }
}
