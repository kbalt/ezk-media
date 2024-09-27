use ezk::Frame;
use ezk_audio::{
    match_samples, Channels, Format, RawAudio, RawAudioFrame, Sample, SampleRate, Samples,
};
use rubato::{FftFixedIn, VecResampler};
use std::collections::VecDeque;

pub(crate) struct RateConverter {
    resampler: Box<dyn VecResampler<f32>>,
    queue: VecDeque<f32>,
    format: Format,
    channels: Channels,
    dst_rate: SampleRate,

    timestamp: u64,

    output_buffers: Vec<Vec<f32>>,
}

impl RateConverter {
    pub(crate) fn new(
        dst_format: Format,
        src: SampleRate,
        dst: SampleRate,
        channels: Channels,
    ) -> Self {
        let chunk_size = src.0 / 50;

        // TODO: make resampling algorithm selectable
        let resampler = Box::new(
            FftFixedIn::<f32>::new(
                src.0 as usize,
                dst.0 as usize,
                chunk_size as usize,
                2,
                channels.channel_count(),
            )
            .unwrap(),
        );

        let output_buffers = resampler.output_buffer_allocate(true);

        Self {
            resampler,
            queue: VecDeque::new(),
            format: dst_format,
            channels,
            dst_rate: dst,
            timestamp: 0,
            output_buffers,
        }
    }

    pub(crate) fn convert(&mut self, src: Frame<RawAudio>) -> Option<Frame<RawAudio>> {
        match_samples!((&src.data().samples) => (samples) => self.queue.extend(samples.iter().map(|s| s.to_sample::<f32>())));

        let channel_count = self.resampler.nbr_channels();
        let chunk_size = self.resampler.input_frames_next();
        let want_n_samples = chunk_size * channel_count;

        let mut samples_out = Samples::empty(self.format);
        let mut out = vec![vec![0.0; chunk_size]; channel_count];

        while self.queue.len() > want_n_samples {
            for (i, sample) in self.queue.drain(..want_n_samples).enumerate() {
                let channel = i % channel_count;
                let chunk_index = i / channel_count;

                out[channel][chunk_index] = sample;
            }

            let (_, chunk_size) = self
                .resampler
                .process_into_buffer(&out, &mut self.output_buffers, None)
                .unwrap();

            match_samples!((&mut samples_out) => (samples_out) => convert_f32_to_samples::<#S>(samples_out, &self.output_buffers, chunk_size));
        }

        if samples_out.is_empty() {
            return None;
        }

        let timestamp = self.timestamp;
        self.timestamp += (samples_out.len() / channel_count) as u64;

        Some(Frame::new(
            RawAudioFrame {
                sample_rate: self.dst_rate,
                channels: self.channels.clone(),
                samples: samples_out,
            },
            timestamp,
        ))
    }
}

fn convert_f32_to_samples<S>(out: &mut Vec<S>, floats: &[Vec<f32>], chunk_size: usize)
where
    S: Sample,
    Samples: From<Vec<S>>,
{
    let channel_count = floats.len();
    let prev_len = out.len();

    // Allocate space in the output buffer
    out.resize(prev_len + (chunk_size * channel_count), S::equilibrium());

    let out = &mut out[prev_len..];

    for (i, dst) in out.iter_mut().enumerate() {
        let channel = i % channel_count;
        let chunk_index = i / channel_count;

        *dst = floats[channel][chunk_index].to_sample();
    }
}
