use ezk::{
    BoxedSource, ConfigRange, Error, Frame, NextEventIsCancelSafe, Result, Source, SourceEvent,
    ValueRange,
};
use ezk_audio::{
    match_samples, RawAudio, RawAudioConfig, RawAudioConfigRange, RawAudioFrame, Sample, Samples,
    SamplesQueue,
};
use futures_util::future::join_all;
use futures_util::FutureExt;
use std::time::{Duration, Instant};
use tokio::time::timeout_at;

pub struct AudioMixer {
    sources: Vec<SourceEntry>,
    config: Option<RawAudioConfig>,

    eos_on_empty_sources: bool,
}

impl AudioMixer {
    pub fn new(source: impl Source<MediaType = RawAudio> + NextEventIsCancelSafe) -> Self {
        Self {
            sources: vec![SourceEntry {
                source: source.boxed(),
                queue: None,
                timestamp: 0,
            }],
            config: None,
            eos_on_empty_sources: true,
        }
    }

    pub fn empty() -> Self {
        Self {
            sources: vec![],
            config: None,
            eos_on_empty_sources: false,
        }
    }

    pub fn eos_on_empty_sources(mut self, eos_on_empty_sources: bool) -> Self {
        self.eos_on_empty_sources = eos_on_empty_sources;
        self
    }

    pub fn add_source(
        &mut self,
        source: impl Source<MediaType = RawAudio> + NextEventIsCancelSafe,
    ) -> &mut Self {
        self.sources.push(SourceEntry {
            source: source.boxed(),
            queue: None,
            timestamp: 0,
        });
        self.config = None;
        self
    }

    pub fn with_source(
        mut self,
        source: impl Source<MediaType = RawAudio> + NextEventIsCancelSafe,
    ) -> Self {
        self.add_source(source);
        self
    }
}

fn intersect_vec(
    a: Vec<RawAudioConfigRange>,
    b: Vec<RawAudioConfigRange>,
) -> Vec<RawAudioConfigRange> {
    let mut intersections = vec![];

    for c1 in a {
        for c2 in &b {
            if let Some(intersection) = c1.intersect(c2) {
                if !intersections.contains(&intersection) {
                    intersections.push(intersection);
                }
            }
        }
    }

    intersections
}

impl Source for AudioMixer {
    type MediaType = RawAudio;

    async fn capabilities(&mut self) -> Result<Vec<RawAudioConfigRange>> {
        let mut ret = None;

        for entry in &mut self.sources {
            let source_caps = entry.source.capabilities().await?;

            let existing_caps = if let Some(existing_caps) = ret.take() {
                existing_caps
            } else {
                ret = Some(source_caps);
                continue;
            };

            let intersections = intersect_vec(existing_caps, source_caps);

            if !intersections.is_empty() {
                ret = Some(intersections);
            }
        }

        ret.ok_or_else(|| {
            Error::msg("AudioMixer cannot find any common capabilities between it's sources")
        })
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<RawAudioConfigRange>,
    ) -> Result<RawAudioConfig> {
        let mut capabilities = intersect_vec(self.capabilities().await?, available);

        if capabilities.is_empty() {
            return Err(Error::msg("AudioMixer cannot find any common config between its capabilities and the available configs"));
        }

        let range = capabilities.remove(0);

        let config = RawAudioConfig {
            sample_rate: range.sample_rate.first_value(),
            channels: range.channels.first_value(),
            format: range.format.first_value(),
        };

        self.config = Some(config.clone());

        for entry in &mut self.sources {
            entry
                .source
                .negotiate_config(vec![RawAudioConfigRange {
                    sample_rate: ValueRange::Value(config.sample_rate),
                    channels: ValueRange::Value(config.channels.clone()),
                    format: ValueRange::Value(config.format),
                }])
                .await?;
        }

        Ok(config)
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        let Some(config) = &self.config else {
            return Ok(SourceEvent::RenegotiationNeeded);
        };

        if self.sources.is_empty() {
            if self.eos_on_empty_sources {
                return Ok(SourceEvent::EndOfData);
            } else {
                return Ok(SourceEvent::Frame(make_silence_frame(config)));
            }
        }

        let timeout = Instant::now() + Duration::from_millis(20);

        let aggregate = self
            .sources
            .iter_mut()
            .enumerate()
            .map(|(i, entry)| entry.next_event(config, timeout).map(move |e| (i, e)))
            .rev();

        let mut frame = None;
        let mut need_renegotiation = false;

        for (i, event) in join_all(aggregate).await {
            let new_frame = match event? {
                Some(SourceEvent::Frame(frame)) => frame,
                Some(SourceEvent::EndOfData) => {
                    self.sources.remove(i);
                    continue;
                }
                Some(SourceEvent::RenegotiationNeeded) => {
                    need_renegotiation = true;
                    continue;
                }
                None => continue,
            };

            if let Some(prev) = frame.take() {
                frame = Some(add(prev, new_frame));
            } else {
                frame = Some(new_frame);
            }
        }

        let frame = if let Some(frame) = frame {
            frame
        } else {
            make_silence_frame(config)
        };

        if need_renegotiation {
            self.config = None;
        }

        Ok(SourceEvent::Frame(frame))
    }
}

fn make_silence_frame(config: &RawAudioConfig) -> Frame<RawAudio> {
    Frame::new(
        RawAudioFrame {
            sample_rate: config.sample_rate,
            channels: config.channels.clone(),
            samples: Samples::equilibrium(config.format, (config.sample_rate.0 / 50) as usize),
        },
        // TODO: correct timestamp
        0,
    )
}

fn add(mut a: Frame<RawAudio>, b: Frame<RawAudio>) -> Frame<RawAudio> {
    fn _add<S: Sample>(a: &mut [S], b: &[S]) {
        for (a, &b) in a.iter_mut().zip(b.iter()) {
            *a = a.saturating_add_(b);
        }
    }

    let a_data = a.make_data_mut();
    let b_data = b.data();

    assert_eq!(a_data.samples.format(), b_data.samples.format());
    assert_eq!(a_data.samples.len(), b_data.samples.len());

    match_samples!((&mut a_data.samples, &b_data.samples) => (a, b) => _add::<#S>(a, b));

    a
}

struct SourceEntry {
    source: BoxedSource<RawAudio>,
    queue: Option<SamplesQueue>,

    timestamp: u64,
}

impl SourceEntry {
    fn make_frame(&mut self, config: &RawAudioConfig, samples: Samples) -> Frame<RawAudio> {
        let timestamp = self.timestamp;
        self.timestamp += (samples.len() / config.channels.channel_count()) as u64;

        Frame::new(
            RawAudioFrame {
                sample_rate: config.sample_rate,
                channels: config.channels.clone(),
                samples,
            },
            timestamp,
        )
    }

    async fn next_event(
        &mut self,
        config: &RawAudioConfig,
        timeout: Instant,
    ) -> Result<Option<SourceEvent<RawAudio>>> {
        let expected_samples_len =
            (config.sample_rate.0 as usize) * config.channels.channel_count() / 50;

        loop {
            if let Some(queue) = &mut self.queue {
                if let Some(samples) = queue.pop_exact(expected_samples_len) {
                    return Ok(Some(SourceEvent::Frame(self.make_frame(config, samples))));
                }
            }

            let event = match timeout_at(timeout.into(), self.source.next_event()).await {
                Ok(result) => result?,
                Err(_) => return Ok(None),
            };

            match event {
                SourceEvent::Frame(frame) => {
                    // fast case, skip queue and return frame
                    if frame.data().samples.len() == expected_samples_len {
                        return Ok(Some(SourceEvent::Frame(frame)));
                    }

                    let queue = self
                        .queue
                        .get_or_insert_with(|| SamplesQueue::empty(config.format));

                    queue.extend(&frame.data().samples);

                    if let Some(samples) = queue.pop_exact(expected_samples_len) {
                        return Ok(Some(SourceEvent::Frame(self.make_frame(config, samples))));
                    } else {
                        continue;
                    }
                }

                // TODO: Drain the queue before to not lose any data?
                SourceEvent::EndOfData => {
                    self.queue = None;
                    return Ok(Some(SourceEvent::EndOfData));
                }
                SourceEvent::RenegotiationNeeded => {
                    self.queue = None;
                    return Ok(Some(SourceEvent::RenegotiationNeeded));
                }
            }
        }
    }
}
