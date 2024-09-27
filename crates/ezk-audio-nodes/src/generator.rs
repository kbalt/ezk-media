use core::f32::consts::PI;
use ezk::{ConfigRange, Frame, NextEventIsCancelSafe, Result, Source, SourceEvent};
use ezk_audio::{
    match_format, RawAudio, RawAudioConfig, RawAudioConfigRange, RawAudioFrame, Sample, Samples,
};
use std::time::Duration;
use tokio::time::{interval, Interval};

pub struct WaveFormGenerator {
    frequency: f32,
    clock: f32,

    timestamp: u64,

    config: Option<(Interval, RawAudioConfig)>,
}

impl NextEventIsCancelSafe for WaveFormGenerator {}

impl WaveFormGenerator {
    pub fn new() -> Self {
        Self {
            frequency: 300.0,
            clock: 0.0,
            timestamp: 0,
            config: None,
        }
    }
}

impl Source for WaveFormGenerator {
    type MediaType = RawAudio;

    async fn capabilities(&mut self) -> Result<Vec<RawAudioConfigRange>> {
        Ok(vec![RawAudioConfigRange::any()])
    }

    async fn negotiate_config(
        &mut self,
        mut available: Vec<RawAudioConfigRange>,
    ) -> Result<RawAudioConfig> {
        let config = available.remove(0);

        let sample_rate = config.sample_rate.first_value();

        let config = RawAudioConfig {
            sample_rate,
            channels: config.channels.first_value(),
            format: config.format.first_value(),
        };

        let interval = interval(Duration::from_millis(20));

        self.config = Some((interval, config.clone()));

        Ok(config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some((interval, config)) = &mut self.config else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        interval.tick().await;

        let samples = generate_samples(config, &mut self.clock, self.frequency);
        let samples_len = samples.len();

        let frame = RawAudioFrame {
            sample_rate: config.sample_rate,
            channels: config.channels.clone(),
            samples,
        };

        let frame = Frame::new(frame, self.timestamp);

        self.timestamp += (samples_len / config.channels.channel_count()) as u64;

        Ok(SourceEvent::Frame(frame))
    }
}

impl Default for WaveFormGenerator {
    fn default() -> Self {
        Self::new()
    }
}

fn generate_samples(config: &RawAudioConfig, clock: &mut f32, freq: f32) -> Samples {
    match_format!(config.format, generate_samples_typed::<#S>(config, clock, freq))
}

fn generate_samples_typed<S>(config: &RawAudioConfig, clock: &mut f32, freq: f32) -> Samples
where
    S: Sample,
    Samples: From<Vec<S>>,
{
    let n_frames = (config.sample_rate.0 as usize) / 50;
    let n_samples = n_frames * config.channels.channel_count();

    let mut out = Vec::with_capacity(n_samples);

    for _ in 0..n_frames {
        let s = S::from_sample(generate_sample(clock, config.sample_rate.0 as f32, freq));

        for _ in 0..config.channels.channel_count() {
            out.push(s);
        }
    }

    out.into()
}

fn generate_sample(clock: &mut f32, rate: f32, freq: f32) -> f32 {
    *clock = (*clock + 1.0) % rate;
    (*clock * freq * 2.0 * PI / rate).sin() * 0.0
}
