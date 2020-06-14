use std;
use std::sync::*;
use crate::for_other_imageflow_crates::preludes::external_without_std::*;
use lcms2::*;
use lcms2;
use crate::ffi;
use crate::errors::{FlowError, ErrorKind, ErrorCategory, Result};
use crate::ffi::{BitmapBgra, DecoderColorInfo};


//
// #[repr(C)]
// #[derive(Clone,Debug,Copy,  PartialEq)]
// pub enum ColorProfileSource {
//     Null = 0,
//     ICCP = 1,
//     ICCP_GRAY = 2,
//     GAMA_CHRM = 3,
//     sRGB = 4,
//
// }
//
// #[repr(C)]
// #[derive(Clone,Debug,Copy, PartialEq)]
// pub struct DecoderColorInfo {
//     pub source: ColorProfileSource,
//     pub profile_buffer: *mut u8,
//     pub buffer_length: usize,
//     pub white_point: ::lcms2::CIExyY,
//     pub primaries: ::lcms2::CIExyYTRIPLE,
//     pub gamma: f64
// }
//
// pub enum ColorInfo{
//     Srgb,
//     ColorProfile(Vec<u8>),
//     ColorProfileGray(Vec<u8>),
//     GamaChrm{
//         white_point: ::lcms2::CIExyY,
//         primaries: ::lcms2::CIExyYTRIPLE,
//         gamma: f64
//     },
//     Gamma{
//         gamma: f64
//     }
// }



pub struct ColorTransformCache{

}

lazy_static!{
    static ref PROFILE_TRANSFORMS: ::chashmap::CHashMap<u64, Transform<u32,u32,ThreadContext, DisallowCache>> = ::chashmap::CHashMap::with_capacity(4);
    static ref GAMA_TRANSFORMS: ::chashmap::CHashMap<u64, Transform<u32,u32, ThreadContext,DisallowCache>> = ::chashmap::CHashMap::with_capacity(4);

}



impl ColorTransformCache{

    fn get_pixel_format(fmt: ffi::PixelFormat) -> PixelFormat{
        match fmt {
            ffi::PixelFormat::Bgr32 | ffi::PixelFormat::Bgra32 => PixelFormat::BGRA_8,
            ffi::PixelFormat::Bgr24 => PixelFormat::BGR_8,
            ffi::PixelFormat::Gray8 => PixelFormat::GRAY_8
        }
    }

    fn create_gama_transform(color: &ffi::DecoderColorInfo, pixel_format: PixelFormat) -> Result<Transform<u32,u32, ThreadContext,DisallowCache>>{
        let srgb = Profile::new_srgb_context(ThreadContext::new()); // Save 1ms by caching - but not sync

        let gama = ToneCurve::new(1f64 / color.gamma);
        let p = Profile::new_rgb_context(ThreadContext::new(),&color.white_point, &color.primaries, &[&gama, &gama, &gama] ).map_err(|e| FlowError::from(e).at(here!()))?;

        let transform = Transform::new_flags_context(ThreadContext::new(),&p, pixel_format, &srgb, pixel_format, Intent::Perceptual, Flags::NO_CACHE).map_err(|e| FlowError::from(e).at(here!()))?;
        Ok(transform)
    }
    fn create_profile_transform(color: &ffi::DecoderColorInfo, pixel_format: PixelFormat) -> Result<Transform<u32,u32, ThreadContext,DisallowCache>> {

        if color.profile_buffer.is_null() || color.buffer_length < 1{
            unreachable!();
        }
        let srgb = Profile::new_srgb_context(ThreadContext::new()); // Save 1ms by caching - but not sync

        let bytes = unsafe { slice::from_raw_parts(color.profile_buffer, color.buffer_length) };

        let _ = (bytes.first(), bytes.last());

        let p = Profile::new_icc_context(ThreadContext::new(), bytes).map_err(|e| FlowError::from(e).at(here!()))?;
        //TODO: handle gray transform on rgb expanded images.
        //TODO: Add test coverage for grayscale png

        let transform = Transform::new_flags_context(ThreadContext::new(),
                                                     &p, pixel_format, &srgb, pixel_format, Intent::Perceptual, Flags::NO_CACHE).map_err(|e| FlowError::from(e).at(here!()))?;

        Ok(transform)
    }
    fn hash(color: &ffi::DecoderColorInfo, pixel_format: ffi::PixelFormat) -> Option<u64>{
        match color.source {
            ffi::ColorProfileSource::Null | ffi::ColorProfileSource::sRGB => None,
            ffi::ColorProfileSource::GAMA_CHRM => {
                let struct_bytes = unsafe {
                    slice::from_raw_parts(color as *const DecoderColorInfo as *const u8, mem::size_of::<DecoderColorInfo>())
                };
                Some(imageflow_helpers::hashing::hash_64(struct_bytes) ^ pixel_format as u64)
            },
            ffi::ColorProfileSource::ICCP | ffi::ColorProfileSource::ICCP_GRAY => {
                if !color.profile_buffer.is_null() && color.buffer_length > 0 {
                    let bytes = unsafe { slice::from_raw_parts(color.profile_buffer, color.buffer_length) };

                    // Skip first 80 bytes when hashing. Wait, why?
                    Some(imageflow_helpers::hashing::hash_64(&bytes[80..]) ^ pixel_format as u64)
                } else {
                    unreachable!("Profile source should never be set to ICCP without a profile buffer. Buffer length {}", color.buffer_length);
                }
            }
        }
    }

    fn apply_transform(frame: &mut BitmapBgra, transform: &Transform<u32,u32, ThreadContext,DisallowCache>) {
        for row in 0..frame.h {
            let pixels = unsafe{ slice::from_raw_parts_mut(frame.pixels.offset((row * frame.stride) as isize) as *mut u32, frame.w as usize) };
            let _ = (pixels.first(), pixels.last());
            transform.transform_in_place(pixels)
        }
    }

    pub fn transform_to_srgb(frame: &mut BitmapBgra, color: &ffi::DecoderColorInfo) -> Result<()>{

        if frame.fmt.bytes() != 4{
            return Err(nerror!(ErrorKind::Category(ErrorCategory::InternalError), "Color profile application is only supported for Bgr32 and Bgra32 canvases"));
        }
        let pixel_format = ColorTransformCache::get_pixel_format(frame.fmt);

        match color.source {
            ffi::ColorProfileSource::Null | ffi::ColorProfileSource::sRGB => Ok(()),
            ffi::ColorProfileSource::GAMA_CHRM => {

                // Cache up to 4 GAMA x PixelFormat transforms
                if GAMA_TRANSFORMS.len() > 3{
                    let transform = ColorTransformCache::create_gama_transform(color, pixel_format).map_err(|e| e.at(here!()))?;
                    ColorTransformCache::apply_transform(frame, &transform);
                    Ok(())
                }else{
                    let hash = ColorTransformCache::hash(color, frame.fmt).unwrap();
                    if !GAMA_TRANSFORMS.contains_key(&hash) {
                        let transform = ColorTransformCache::create_gama_transform(color, pixel_format).map_err(|e| e.at(here!()))?;
                        GAMA_TRANSFORMS.insert(hash, transform);
                    }
                    ColorTransformCache::apply_transform(frame, &*GAMA_TRANSFORMS.get(&hash).unwrap());
                    Ok(())
                }
            },
            ffi::ColorProfileSource::ICCP | ffi::ColorProfileSource::ICCP_GRAY => {
                // Cache up to 9 ICC profile x PixelFormat transforms
                if PROFILE_TRANSFORMS.len() > 8{
                    let transform = ColorTransformCache::create_profile_transform(color, pixel_format).map_err(|e| e.at(here!()))?;
                    ColorTransformCache::apply_transform(frame, &transform);
                    Ok(())
                }else{
                    let hash = ColorTransformCache::hash(color, frame.fmt).unwrap();
                    if !PROFILE_TRANSFORMS.contains_key(&hash) {
                        let transform = ColorTransformCache::create_profile_transform(color, pixel_format).map_err(|e| e.at(here!()))?;
                        PROFILE_TRANSFORMS.insert(hash, transform);
                    }
                    ColorTransformCache::apply_transform(frame, &*PROFILE_TRANSFORMS.get(&hash).unwrap());
                    Ok(())
                }
            }
        }
    }

    pub fn dispose_color_info(color: &mut ffi::DecoderColorInfo){
        // DecoderColor info is cleaned up by the context. For now this is the best option, so that dangling pointers don't happen
    }
}