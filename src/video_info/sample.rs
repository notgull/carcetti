// GNU AGPL v3 License

// TODO: take raw sample data and decode it into iterators over usizes

use anyhow::{anyhow, Result};
use ffmpeg::format::{sample::Type, Sample};

pub(crate) fn decode_samples(
    samples: &[u8],
    format: Sample,
) -> Result<impl ExactSizeIterator<Item = f64> + '_> {
    let (sz, signed, floating, use_planes) = match &format {
        Sample::None => return Err(anyhow!("Cannot decode samples of type `None`")),
        Sample::U8(form) => (1, false, false, ty_is_planar(form)),
        Sample::I16(form) => (2, true, false, ty_is_planar(form)),
        Sample::I32(form) => (4, true, false, ty_is_planar(form)),
        Sample::I64(form) => (8, true, false, ty_is_planar(form)),
        Sample::F32(form) => (4, true, true, ty_is_planar(form)),
        Sample::F64(form) => (8, true, true, ty_is_planar(form)),
    };

    let _ = use_planes;

    // create the inner iterator over samples
    Ok(samples
        .chunks_exact(sz)
        .map(move |chunk| match (sz, signed, floating) {
            (1, false, false) => chunk[0] as f64,
            (2, true, false) => bytemuck::pod_read_unaligned::<i16>(chunk) as f64,
            (4, true, false) => bytemuck::pod_read_unaligned::<i32>(chunk) as f64,
            (4, true, true) => bytemuck::pod_read_unaligned::<f32>(chunk) as f64,
            (8, true, false) => bytemuck::pod_read_unaligned::<i64>(chunk) as f64,
            (8, true, true) => bytemuck::pod_read_unaligned::<f64>(chunk) as f64,
            _ => unreachable!(),
        }))
}

fn ty_is_planar(ty: &Type) -> bool {
    matches!(ty, Type::Planar)
}
