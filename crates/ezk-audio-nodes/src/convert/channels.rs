use ezk::Frame;
use ezk_audio::{match_samples, Channels, RawAudio, RawAudioFrame, Sample, Samples};

pub(super) struct ChannelMixer {
    src_channels: Channels,
    dst_channels: Channels,

    matrix: Box<[f64]>,
}

impl ChannelMixer {
    pub(crate) fn new(src_channels: Channels, dst_channels: Channels) -> Self {
        assert_ne!(src_channels.channel_count(), 0);
        assert_ne!(dst_channels.channel_count(), 0);

        let matrix = if src_channels == dst_channels {
            Box::new([])
        } else if src_channels.is_mono() && dst_channels.is_stereo() {
            Box::new([1.0, 1.0])
        } else if src_channels.is_stereo() && dst_channels.is_mono() {
            Box::new([0.5, 0.5])
        } else if src_channels.is_mono() {
            // Map mono channels to FrontLeft and FrontRight
            let mut matrix = vec![0.0; dst_channels.channel_count()].into_boxed_slice();
            matrix[0] = 1.0;
            matrix[1] = 1.0;
            matrix
        } else {
            let mut matrix = vec![0.0; src_channels.channel_count() * dst_channels.channel_count()]
                .into_boxed_slice();

            if let (Channels::Positioned(positions_in), Channels::Positioned(positions_out)) =
                (&src_channels, &dst_channels)
            {
                for (n_src, src_channel_pos) in positions_in.iter().enumerate() {
                    for (n_dst, dst_channel_pos) in positions_out.iter().enumerate() {
                        if src_channel_pos.intersects(*dst_channel_pos) {
                            matrix[n_dst * src_channels.channel_count() + n_src] = 1.0;
                        }
                    }
                }
            } else {
                for n_src in 0..src_channels.channel_count() {
                    for n_dst in 0..dst_channels.channel_count() {
                        if n_src == n_dst {
                            matrix[n_dst * src_channels.channel_count() + n_src] = 1.0;
                        }
                    }
                }
            }

            matrix
        };

        Self {
            src_channels,
            dst_channels,
            matrix,
        }
    }

    pub(crate) fn convert(&self, frame: Frame<RawAudio>) -> Frame<RawAudio> {
        assert_eq!(
            frame.data().channels.channel_count(),
            self.src_channels.channel_count()
        );

        if self.matrix.is_empty() {
            return frame;
        }

        let samples = match_samples!((&frame.data().samples) => (samples) => do_mix::<#S>(
            samples,
            frame.data().channels.channel_count(),
            self.dst_channels.channel_count(),
            &self.matrix,
        ));

        Frame::new(
            RawAudioFrame {
                sample_rate: frame.data().sample_rate,
                channels: self.dst_channels.clone(),
                samples,
            },
            frame.timestamp,
        )
    }
}

fn do_mix<S>(src: &[S], src_channel: usize, dst_channel: usize, matrix: &[f64]) -> Samples
where
    S: Sample,
    Samples: From<Vec<S>>,
{
    let mut dst = vec![S::equilibrium(); dst_channel * src.len() / src_channel];

    let src_chunks = src.chunks_exact(src_channel);
    let dst_chunks = dst.chunks_exact_mut(dst_channel);

    for (src_chunk, dst_chunk) in src_chunks.zip(dst_chunks) {
        for (dst_i, dst) in dst_chunk.iter_mut().enumerate() {
            for (src_i, src) in src_chunk.iter().enumerate() {
                *dst = dst
                    .saturating_add_(src.saturating_mul_f64(matrix[dst_i * src_channel + src_i]));
            }
        }
    }

    dst.into()
}
