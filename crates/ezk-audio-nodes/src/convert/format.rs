use ezk::Frame;
use ezk_audio::{match_format, match_samples, Format, RawAudio, RawAudioFrame, Sample, Samples};
use std::alloc::Layout;
use std::mem::take;

pub(crate) fn convert_sample_format(src: Frame<RawAudio>, dst_format: Format) -> Frame<RawAudio> {
    let src_format = src.data().samples.format();
    if src_format == dst_format {
        return src;
    }

    match_format!(dst_format, convert_with_dst_type::<#S>(src))
}

fn convert_with_dst_type<Dst>(mut src: Frame<RawAudio>) -> Frame<RawAudio>
where
    Dst: Sample,
    Samples: From<Vec<Dst>>,
{
    if let Some(data) = src.data_mut() {
        let dst_layout = Layout::new::<Dst>();

        let samples = match_samples!((&mut data.samples) => (src) => {
            let src_layout = Layout::new::<#S>();

            if src_layout == dst_layout {
                unsafe {
                    // Safety: Made sure that Src and Dst have the same layout
                    convert_with_dst_and_src_type_inplace::<#S, Dst>(src)
                }
            } else {
                convert_with_dst_and_src_type::<#S, Dst>(src)
            }
        });

        data.samples = samples;

        src
    } else {
        let samples = match_samples!((&src.data().samples) => (src) => {
            convert_with_dst_and_src_type::<#S, Dst>(src)
        });

        Frame::new(
            RawAudioFrame {
                sample_rate: src.data().sample_rate,
                channels: src.data().channels.clone(),
                samples,
            },
            src.timestamp,
        )
    }
}

fn convert_with_dst_and_src_type<Src, Dst>(src: &[Src]) -> Samples
where
    Src: Sample,
    Dst: Sample,
    Samples: From<Vec<Dst>>,
{
    Vec::<Dst>::from_iter(src.iter().map(|s| s.to_sample())).into()
}

/// Safety: Only valid if Src and Dst have the same Layout
unsafe fn convert_with_dst_and_src_type_inplace<Src, Dst>(src: &mut Vec<Src>) -> Samples
where
    Src: Sample,
    Dst: Sample,
    Samples: From<Vec<Dst>>,
{
    let src = take(src);

    let (ptr, len, cap) = vec_into_raw_parts(src);

    for offset in 0..len {
        let ptr = ptr.add(offset);
        let src = ptr.read();
        let ptr = ptr.cast::<Dst>();
        ptr.write(src.to_sample());
    }

    Vec::from_raw_parts(ptr.cast::<Dst>(), len, cap).into()
}

fn vec_into_raw_parts<T>(vec: Vec<T>) -> (*mut T, usize, usize) {
    let mut vec = std::mem::ManuallyDrop::new(vec);
    (vec.as_mut_ptr(), vec.len(), vec.capacity())
}
