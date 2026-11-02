//! Hardware decoding through NVDEC (`nvcuvid.dll`): the driver's parser finds the pictures in an
//! H.264, HEVC or AV1 stream, and the decoder hands each back in video memory, scaled when asked,
//! for this program to copy out.

use crate::{
    config::Codec,
    gpu::{
        cuda::{ContextHold, CudaError, DeviceBuffer, Status},
        ffi::Library,
    },
    trim::fit_within,
};
use bytemuck::Zeroable;
use std::{
    collections::VecDeque,
    ffi::c_void,
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    sync::Arc,
};
use thiserror::Error;
use windows::core::{PCSTR, w};

mod ffi;

/// Surfaces beyond the stream's minimum, so decoding runs ahead of the copies.
const EXTRA_DECODE_SURFACES: u32 = 4;
const OUTPUT_SURFACES: u32 = 2;

#[derive(Debug, Error)]
pub(super) enum DecodeError {
    #[error("nvcuvid.dll was not found; install or update the NVIDIA driver")]
    Library,
    #[error("nvcuvid.dll has no {0}; update the NVIDIA driver")]
    MissingEntryPoint(&'static str),
    #[error("{0} failed with {1}")]
    Call(&'static str, Status),
    #[error("{0}: {1}")]
    Cuda(&'static str, CudaError),
    #[error("this GPU's NVDEC cannot decode {codec} at {width}×{height}")]
    Unsupported {
        codec: Codec,
        width: u32,
        height: u32,
    },
    #[error("only 8-bit 4:2:0 video can be trimmed")]
    Format,
    #[error("the video changes size partway through")]
    Resized,
    #[error("the decoder's callback panicked")]
    Panicked,
    #[error("a frame of {0} bytes is too large to decode")]
    TooLarge(usize),
}

/// Where decoded pictures are copied to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Output {
    Host,
    /// Into buffers the decoder keeps, which the caller hands back with [`Decoder::recycle`].
    Device,
}

pub(super) struct Picture {
    /// The time the sample was given with.
    pub(super) time: i64,
    pub(super) pixels: Pixels,
}

/// NV12: the luma rows, then the interleaved chroma rows, [`Size::pitch`] bytes apart.
pub(super) enum Pixels {
    Host(Vec<u8>),
    /// The index of one of the decoder's buffers.
    Device(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Size {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pitch: u32,
}

impl Size {
    pub(super) fn bytes(self) -> usize {
        self.pitch as usize * self.height as usize * 3 / 2
    }
}

#[derive(Clone, Copy)]
struct Api {
    create_video_parser: ffi::CreateVideoParser,
    parse_video_data: ffi::ParseVideoData,
    destroy_video_parser: ffi::DestroyVideoParser,
    get_decoder_caps: ffi::GetDecoderCaps,
    create_decoder: ffi::CreateDecoder,
    destroy_decoder: ffi::DestroyDecoder,
    decode_picture: ffi::DecodePicture,
    map_video_frame: ffi::MapVideoFrame,
    unmap_video_frame: ffi::UnmapVideoFrame,
}

macro_rules! entry_point {
    ($library:expr, $name:literal, $type:ty) => {{
        // SAFETY: the export has the declared signature, as the macro's contract says.
        let symbol = unsafe { $library.export::<$type>(PCSTR(concat!($name, "\0").as_ptr())) }
            .ok_or(DecodeError::MissingEntryPoint($name))?;
        // Detached from the borrow: `Decoder` keeps the library in its last field.
        *symbol
    }};
}

impl Api {
    fn load(library: &Library) -> Result<Self, DecodeError> {
        Ok(Self {
            create_video_parser: entry_point!(
                library,
                "cuvidCreateVideoParser",
                ffi::CreateVideoParser
            ),
            parse_video_data: entry_point!(library, "cuvidParseVideoData", ffi::ParseVideoData),
            destroy_video_parser: entry_point!(
                library,
                "cuvidDestroyVideoParser",
                ffi::DestroyVideoParser
            ),
            get_decoder_caps: entry_point!(library, "cuvidGetDecoderCaps", ffi::GetDecoderCaps),
            create_decoder: entry_point!(library, "cuvidCreateDecoder", ffi::CreateDecoder),
            destroy_decoder: entry_point!(library, "cuvidDestroyDecoder", ffi::DestroyDecoder),
            decode_picture: entry_point!(library, "cuvidDecodePicture", ffi::DecodePicture),
            map_video_frame: entry_point!(library, "cuvidMapVideoFrame64", ffi::MapVideoFrame),
            unmap_video_frame: entry_point!(
                library,
                "cuvidUnmapVideoFrame64",
                ffi::UnmapVideoFrame
            ),
        })
    }
}

fn call(name: &'static str, status: Status) -> Result<(), DecodeError> {
    status
        .ok()
        .map_err(|status| DecodeError::Call(name, status))
}

fn cuda(step: &'static str) -> impl Fn(CudaError) -> DecodeError {
    move |error| DecodeError::Cuda(step, error)
}

/// What the parser's callbacks share with the decoder, reached through the parser's user data.
struct State {
    api: Api,
    hold: Arc<ContextHold>,
    codec: Codec,
    /// The size each picture is scaled to fit within, if any.
    fit: Option<(u32, u32)>,
    output: Output,
    decoder: ffi::RawDecoder,
    /// The coded size and display area the decoder was created for.
    created: Option<(u32, u32, [i32; 4])>,
    size: Option<Size>,
    pictures: VecDeque<Picture>,
    buffers: Vec<Arc<DeviceBuffer>>,
    free: Vec<usize>,
    /// The first failure in a callback, returned once parsing returns.
    error: Option<DecodeError>,
}

/// Decodes one stream on the thread that opened it.
pub(super) struct Decoder {
    parser: ffi::Parser,
    /// Freed in `Drop`, after the parser that points at it.
    state: NonNull<State>,
    hold: Arc<ContextHold>,
    /// Declared last so it is unloaded last.
    _library: Library,
}

impl Decoder {
    pub(super) fn open(
        hold: Arc<ContextHold>,
        codec: Codec,
        fit: Option<(u32, u32)>,
        output: Output,
    ) -> Result<Self, DecodeError> {
        let library = Library::load(w!("nvcuvid.dll")).map_err(|_| DecodeError::Library)?;
        let api = Api::load(&library)?;
        let state = Box::new(State {
            api,
            hold: Arc::clone(&hold),
            codec,
            fit,
            output,
            decoder: ptr::null_mut(),
            created: None,
            size: None,
            pictures: VecDeque::new(),
            buffers: Vec::new(),
            free: Vec::new(),
            error: None,
        });
        let mut decoder = Self {
            parser: ptr::null_mut(),
            state: NonNull::from(Box::leak(state)),
            hold,
            _library: library,
        };
        decoder.create_parser()?;
        Ok(decoder)
    }

    fn state(&self) -> &State {
        // SAFETY: allocated in `open` and freed only in `Drop`; the parser reaches it only
        // during `parse`, which borrows `self` mutably.
        unsafe { self.state.as_ref() }
    }

    fn state_mut(&mut self) -> &mut State {
        // SAFETY: as in `state`.
        unsafe { self.state.as_mut() }
    }

    fn create_parser(&mut self) -> Result<(), DecodeError> {
        let api = self.state().api;
        let mut params = ffi::ParserParams::zeroed();
        params.codec = codec_id(self.state().codec);
        // The sequence callback returns the number the stream needs.
        params.max_num_decode_surfaces = 1;
        // Pictures are handed back as soon as they are decoded, which scrubbing needs.
        params.max_display_delay = 0;
        params.user_data = self.state.as_ptr().cast();
        params.sequence_callback = Some(on_sequence);
        params.decode_picture = Some(on_decode);
        params.display_picture = Some(on_display);
        let mut parser = ptr::null_mut();
        // SAFETY: the parameters outlive the call; the user data is the state, which outlives
        // the parser, and the callbacks have the signatures the header declares.
        call("cuvidCreateVideoParser", unsafe {
            (api.create_video_parser)(&raw mut parser, &raw mut params)
        })?;
        self.parser = parser;
        Ok(())
    }

    fn destroy_parser(&mut self) {
        if self.parser.is_null() {
            return;
        }
        let api = self.state().api;
        // SAFETY: created by `create_parser` and not destroyed since.
        let _ = unsafe { (api.destroy_video_parser)(self.parser) };
        self.parser = ptr::null_mut();
    }

    /// Decodes one sample, shown at `time`. The pictures it completes are queued for
    /// [`Self::next`].
    pub(super) fn decode(&mut self, data: &[u8], time: i64) -> Result<(), DecodeError> {
        let mut packet = ffi::SourcePacket {
            flags: ffi::PACKET_TIMESTAMP | ffi::PACKET_END_OF_PICTURE,
            payload_size: u32::try_from(data.len())
                .map_err(|_| DecodeError::TooLarge(data.len()))?,
            payload: data.as_ptr(),
            timestamp: time,
        };
        self.parse(&mut packet)
    }

    /// Queues the pictures the parser still holds back, then starts afresh at the next
    /// keyframe.
    pub(super) fn finish(&mut self) -> Result<(), DecodeError> {
        let mut packet = ffi::SourcePacket {
            flags: ffi::PACKET_END_OF_STREAM,
            ..ffi::SourcePacket::zeroed()
        };
        let parsed = self.parse(&mut packet);
        self.destroy_parser();
        self.create_parser()?;
        parsed
    }

    /// Drops the pictures in flight and queued, so decoding may start again at a keyframe.
    pub(super) fn restart(&mut self) -> Result<(), DecodeError> {
        self.destroy_parser();
        let state = self.state_mut();
        state.error = None;
        while let Some(picture) = state.pictures.pop_front() {
            if let Pixels::Device(index) = picture.pixels {
                state.free.push(index);
            }
        }
        self.create_parser()
    }

    fn parse(&mut self, packet: &mut ffi::SourcePacket) -> Result<(), DecodeError> {
        let api = self.state().api;
        let hold = Arc::clone(&self.hold);
        let _current = hold
            .make_current()
            .map_err(cuda("make the CUDA context current"))?;
        // SAFETY: the parser is live; the packet and its payload outlive the call; the callbacks
        // reach the state only during it, while nothing else borrows it.
        let status = unsafe { (api.parse_video_data)(self.parser, packet) };
        if let Some(error) = self.state_mut().error.take() {
            return Err(error);
        }
        call("cuvidParseVideoData", status)
    }

    pub(super) fn next(&mut self) -> Option<Picture> {
        self.state_mut().pictures.pop_front()
    }

    /// The size of the pictures, once the first has been decoded.
    pub(super) fn size(&self) -> Option<Size> {
        self.state().size.filter(|size| size.pitch > 0)
    }

    pub(super) fn buffer(&self, index: usize) -> Option<&Arc<DeviceBuffer>> {
        self.state().buffers.get(index)
    }

    /// Hands back a buffer from [`Pixels::Device`] for another picture.
    pub(super) fn recycle(&mut self, index: usize) {
        let state = self.state_mut();
        if index < state.buffers.len() && !state.free.contains(&index) {
            state.free.push(index);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.destroy_parser();
        let hold = Arc::clone(&self.hold);
        if let Ok(_current) = hold.make_current() {
            self.state_mut().destroy_decoder();
        }
        // SAFETY: leaked in `open`, and the parser that pointed at it is destroyed.
        drop(unsafe { Box::from_raw(self.state.as_ptr()) });
    }
}

impl State {
    fn sequence(&mut self, format: &ffi::VideoFormat) -> Result<i32, DecodeError> {
        if format.bit_depth_luma_minus8 != 0
            || format.bit_depth_chroma_minus8 != 0
            || format.chroma_format != ffi::CHROMA_420
        {
            return Err(DecodeError::Format);
        }
        let surfaces = u32::from(format.min_num_decode_surfaces).max(1) + EXTRA_DECODE_SURFACES;
        let surfaces_result = i32::try_from(surfaces).unwrap_or(i32::MAX);
        let area = format.display_area;
        let created = (
            format.coded_width,
            format.coded_height,
            [area.left, area.top, area.right, area.bottom],
        );
        if self.created == Some(created) {
            return Ok(surfaces_result);
        }
        if self.created.is_some() && self.output == Output::Device {
            return Err(DecodeError::Resized);
        }
        self.destroy_decoder();
        self.require_support(format)?;
        let shown = (
            u32::try_from(area.right - area.left).unwrap_or(0),
            u32::try_from(area.bottom - area.top).unwrap_or(0),
        );
        let (width, height) = fit_within(shown, self.fit);
        let short = |value: i32| i16::try_from(value).unwrap_or(i16::MAX);
        let mut info = ffi::CreateInfo::zeroed();
        info.width = format.coded_width;
        info.height = format.coded_height;
        info.num_decode_surfaces = surfaces;
        info.codec = codec_id(self.codec);
        info.chroma_format = ffi::CHROMA_420;
        info.creation_flags = ffi::CREATE_PREFER_CUVID;
        info.max_width = format.coded_width;
        info.max_height = format.coded_height;
        info.display_area = ffi::Area {
            left: short(area.left),
            top: short(area.top),
            right: short(area.right),
            bottom: short(area.bottom),
        };
        info.output_format = ffi::SURFACE_NV12;
        info.deinterlace_mode = if format.progressive_sequence == 0 {
            ffi::DEINTERLACE_ADAPTIVE
        } else {
            ffi::DEINTERLACE_WEAVE
        };
        info.target_width = width;
        info.target_height = height;
        info.num_output_surfaces = OUTPUT_SURFACES;
        let mut decoder = ptr::null_mut();
        // SAFETY: the description outlives the call, and the CUDA context is current, as
        // `Decoder::parse` makes it.
        call("cuvidCreateDecoder", unsafe {
            (self.api.create_decoder)(&raw mut decoder, &raw mut info)
        })?;
        self.decoder = decoder;
        self.created = Some(created);
        self.size = Some(Size {
            width,
            height,
            pitch: 0,
        });
        Ok(surfaces_result)
    }

    fn require_support(&self, format: &ffi::VideoFormat) -> Result<(), DecodeError> {
        let mut caps = ffi::DecoderCaps::zeroed();
        caps.codec = codec_id(self.codec);
        caps.chroma_format = ffi::CHROMA_420;
        // SAFETY: the structure outlives the call, and the CUDA context is current.
        call("cuvidGetDecoderCaps", unsafe {
            (self.api.get_decoder_caps)(&raw mut caps)
        })?;
        if caps.is_supported == 0
            || format.coded_width > caps.max_width
            || format.coded_height > caps.max_height
        {
            return Err(DecodeError::Unsupported {
                codec: self.codec,
                width: format.coded_width,
                height: format.coded_height,
            });
        }
        Ok(())
    }

    fn decode(&mut self, params: *mut ffi::PictureParams) -> Result<i32, DecodeError> {
        // SAFETY: the parser passes the parameters for the decoder it described in the sequence
        // callback, which created `self.decoder`.
        call("cuvidDecodePicture", unsafe {
            (self.api.decode_picture)(self.decoder, params)
        })?;
        Ok(1)
    }

    fn display(&mut self, info: &ffi::DisplayInfo) -> Result<i32, DecodeError> {
        let mut params = ffi::ProcessingParams::zeroed();
        params.progressive_frame = info.progressive_frame;
        params.second_field = info.repeat_first_field + 1;
        params.top_field_first = info.top_field_first;
        params.unpaired_field = i32::from(info.repeat_first_field < 0);
        let (mut address, mut pitch) = (0u64, 0u32);
        // SAFETY: the picture index is the parser's, for this decoder; the out-parameters and
        // the parameters outlive the call.
        call("cuvidMapVideoFrame", unsafe {
            (self.api.map_video_frame)(
                self.decoder,
                info.picture_index,
                &raw mut address,
                &raw mut pitch,
                &raw mut params,
            )
        })?;
        let copied = self.copy_out(address, pitch, info.timestamp);
        // SAFETY: mapped just above, and the copy out of it has completed.
        let unmapped = call("cuvidUnmapVideoFrame", unsafe {
            (self.api.unmap_video_frame)(self.decoder, address)
        });
        copied.and(unmapped)?;
        Ok(1)
    }

    /// Copies the mapped picture at `address` out before it is unmapped.
    fn copy_out(&mut self, address: u64, pitch: u32, time: i64) -> Result<(), DecodeError> {
        let size = self.size.as_mut().ok_or(DecodeError::Format)?;
        if size.pitch != 0 && size.pitch != pitch {
            return Err(DecodeError::Resized);
        }
        size.pitch = pitch;
        let bytes = size.bytes();
        let pixels = match self.output {
            Output::Host => {
                let mut nv12 = vec![0; bytes];
                // SAFETY: a mapped NV12 picture is its pitch times one and a half its height,
                // and stays mapped until `display` unmaps it after this copy.
                unsafe { self.hold.driver().copy_address_to_host(&mut nv12, address) }
                    .map_err(cuda("copy a decoded picture"))?;
                Pixels::Host(nv12)
            }
            Output::Device => {
                let index = if let Some(index) = self.free.pop() {
                    index
                } else {
                    let buffer = self
                        .hold
                        .allocate(bytes)
                        .map_err(cuda("allocate a decoded picture"))?;
                    self.buffers.push(Arc::new(buffer));
                    self.buffers.len() - 1
                };
                let buffer = &self.buffers[index];
                // SAFETY: as above; the copy is waited for before the picture is unmapped.
                unsafe { buffer.copy_from_address(address, bytes) }
                    .and_then(|()| buffer.synchronize())
                    .map_err(cuda("copy a decoded picture"))?;
                Pixels::Device(index)
            }
        };
        self.pictures.push_back(Picture { time, pixels });
        Ok(())
    }

    fn destroy_decoder(&mut self) {
        if self.decoder.is_null() {
            return;
        }
        // SAFETY: created by `sequence` and not destroyed since; the context is current.
        let _ = unsafe { (self.api.destroy_decoder)(self.decoder) };
        self.decoder = ptr::null_mut();
        self.created = None;
    }
}

const fn codec_id(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => ffi::CODEC_H264,
        Codec::Hevc => ffi::CODEC_HEVC,
        Codec::Av1 => ffi::CODEC_AV1,
    }
}

/// Runs a callback's body on the state, recording its failure, or a panic, which must not
/// unwind into the driver.
fn guarded(user: *mut c_void, body: impl FnOnce(&mut State) -> Result<i32, DecodeError>) -> i32 {
    // SAFETY: the user data is the decoder's state, which outlives its parser, and nothing else
    // borrows it while the parser calls back.
    let state = unsafe { &mut *user.cast::<State>() };
    let result = panic::catch_unwind(AssertUnwindSafe(|| body(&mut *state)))
        .unwrap_or(Err(DecodeError::Panicked));
    result.unwrap_or_else(|error| {
        state.error.get_or_insert(error);
        0
    })
}

unsafe extern "system" fn on_sequence(user: *mut c_void, format: *mut ffi::VideoFormat) -> i32 {
    guarded(user, |state| {
        // SAFETY: the parser passes a format that is valid for the call.
        let format = unsafe { &*format };
        state.sequence(format)
    })
}

unsafe extern "system" fn on_decode(user: *mut c_void, params: *mut ffi::PictureParams) -> i32 {
    guarded(user, |state| state.decode(params))
}

unsafe extern "system" fn on_display(user: *mut c_void, info: *mut ffi::DisplayInfo) -> i32 {
    guarded(user, |state| {
        // SAFETY: null at the end of the stream, otherwise valid for the call.
        match unsafe { info.as_ref() } {
            Some(info) => state.display(info),
            None => Ok(1),
        }
    })
}
