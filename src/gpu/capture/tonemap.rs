//! The pixel-shader pass that tone-maps a display's FP16 scRGB picture to the encoder's 8-bit
//! sRGB: SDR white to 1.0, a Reinhard roll-off above a knee, the display's peak landing on 1.0.

use crate::gpu::{
    device::{Device, Locked, null_output},
    win32::cb_size,
};
use std::ffi::c_void;
use windows::{
    Win32::Graphics::{
        Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        Direct3D11::{
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
            D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11Buffer, ID3D11DepthStencilView,
            ID3D11Device, ID3D11InputLayout, ID3D11Multithread, ID3D11PixelShader,
            ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
        },
    },
    core::Interface,
};

/// `tonemap.hlsl`, compiled by `build.rs` with the Windows SDK's `fxc.exe`, so no shader
/// compiler (`d3dcompiler_47.dll`) is loaded at run time.
const VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tonemap.vs.cso"));
const PIXEL_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tonemap.ps.cso"));
/// In units of SDR white; below it the picture is kept exactly.
const KNEE: f32 = 0.8;
const DEFAULT_WHITE_NITS: f32 = 200.0;
const DEFAULT_PEAK_NITS: f32 = 1000.0;
const SCRGB_NITS: f32 = super::display::SCRGB_NITS;
const MAX_VIEWS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Curve {
    /// scRGB to units of SDR white.
    scale: f32,
    knee: f32,
    rolloff: f32,
    white_nits: f32,
    peak_nits: f32,
}

impl Curve {
    pub(super) fn new(white_nits: Option<f32>, peak_nits: f32) -> Self {
        let white = white_nits
            .filter(|nits| *nits > 0.0)
            .unwrap_or(DEFAULT_WHITE_NITS);
        let peak = if peak_nits > white {
            peak_nits
        } else {
            DEFAULT_PEAK_NITS.max(white)
        };
        // Reinhard x / (1 + x b) has slope 1 at 0, meeting the straight part smoothly; b puts
        // the peak on 1.0, and no headroom gives b = 0.
        let headroom = peak / white;
        let rolloff = 1.0 / (1.0 - KNEE) - 1.0 / (headroom - KNEE);
        Self {
            scale: SCRGB_NITS / white,
            knee: KNEE,
            rolloff,
            white_nits: white,
            peak_nits: peak,
        }
    }

    pub(super) const fn white_nits(&self) -> f32 {
        self.white_nits
    }

    pub(super) const fn peak_nits(&self) -> f32 {
        self.peak_nits
    }

    /// The shader's reference.
    #[cfg(test)]
    fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut rgb = rgb.map(|component| (component * self.scale).max(0.0));
        let largest = rgb[0].max(rgb[1]).max(rgb[2]);
        if largest > self.knee {
            let above = largest - self.knee;
            let factor = (self.knee + above / (1.0 + above * self.rolloff)) / largest;
            rgb = rgb.map(|component| component * factor);
        }
        rgb.map(|component| srgb(component.clamp(0.0, 1.0)))
    }

    const fn constants(&self) -> [f32; 4] {
        [self.scale, self.knee, self.rolloff, 0.0]
    }
}

#[cfg(test)]
fn srgb(level: f32) -> f32 {
    if level <= 0.003_130_8 {
        level * 12.92
    } else {
        1.055 * level.powf(1.0 / 2.4) - 0.055
    }
}

pub(super) struct ToneMapper {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    /// Held while the pass sets state and draws, against the capture API's own threads.
    lock: ID3D11Multithread,
    sources: Views<ID3D11ShaderResourceView>,
    targets: Views<ID3D11RenderTargetView>,
    /// A readable copy, for a source created without shader-resource binding.
    readable: Option<ID3D11Texture2D>,
}

#[expect(unsafe_code)]
impl ToneMapper {
    pub(super) fn new(device: &Device, curve: Curve) -> windows::core::Result<Self> {
        let mut vertex = None;
        let mut pixel = None;
        // SAFETY: the bytecode was compiled for the vertex stage and `vertex` outlives the call.
        unsafe {
            device
                .d3d
                .CreateVertexShader(VERTEX_SHADER, None, Some(&raw mut vertex))?;
        }
        // SAFETY: the bytecode was compiled for the pixel stage and `pixel` outlives the call.
        unsafe {
            device
                .d3d
                .CreatePixelShader(PIXEL_SHADER, None, Some(&raw mut pixel))?;
        }
        Ok(Self {
            vertex: created(vertex)?,
            pixel: created(pixel)?,
            constants: constant_buffer(&device.d3d, &curve)?,
            lock: device.context.cast()?,
            sources: Views::default(),
            targets: Views::default(),
            readable: None,
        })
    }

    /// Queues the pass over the top-left `width` × `height` of `source` into `target`.
    pub(super) fn convert(
        &mut self,
        device: &Device,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> windows::core::Result<()> {
        let source = self.readable_source(device, source, width, height)?;
        let d3d = &device.d3d;
        let input = self
            .sources
            .get(&source, || shader_resource_view(d3d, &source))?;
        let output = self
            .targets
            .get(target, || render_target_view(d3d, target))?;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let context = &device.context;
        let _locked = Locked::new(&self.lock);
        // The context is shared with the capture API's threads; `_locked` keeps them out until
        // the draw is queued and the views are unbound again.
        // SAFETY: a valid topology for the immediate context of a live device.
        unsafe { context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST) };
        // SAFETY: no input layout: the vertex shader builds its triangle from the vertex id.
        unsafe { context.IASetInputLayout(None::<&ID3D11InputLayout>) };
        // SAFETY: a shader created on this device; the context holds its own reference.
        unsafe { context.VSSetShader(&self.vertex, None) };
        // SAFETY: as above, for the pixel shader.
        unsafe { context.PSSetShader(&self.pixel, None) };
        // SAFETY: a buffer of this device with constant-buffer binding; the slice outlives the call.
        unsafe { context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())])) };
        // SAFETY: a view of this device over a texture with shader-resource binding.
        unsafe { context.PSSetShaderResources(0, Some(&[Some(input)])) };
        // SAFETY: one viewport within the target, read during the call.
        unsafe { context.RSSetViewports(Some(&[viewport])) };
        // SAFETY: a render-target view of this device over another texture than the source,
        // so the draw never reads what it writes.
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(output)]), None::<&ID3D11DepthStencilView>);
        }
        // SAFETY: shaders, constants, source and target are bound above; three vertices make
        // the one full-screen triangle the vertex shader expects.
        unsafe { context.Draw(3, 0) };
        // Unbind so the encoder and the capture may use both textures.
        // SAFETY: clears slot 0; the slice outlives the call.
        unsafe { context.PSSetShaderResources(0, Some(&[None])) };
        // SAFETY: unbinding every render target has no preconditions.
        unsafe { context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>) };
        Ok(())
    }

    fn readable_source(
        &mut self,
        device: &Device,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> windows::core::Result<ID3D11Texture2D> {
        let description = texture_description(source);
        if description.BindFlags & bind_flag(D3D11_BIND_SHADER_RESOURCE.0) != 0 {
            return Ok(source.clone());
        }
        let fits = self.readable.as_ref().is_some_and(|readable| {
            let own = texture_description(readable);
            (own.Width, own.Height, own.Format) == (width, height, description.Format)
        });
        if !fits {
            self.readable = Some(readable_texture(&device.d3d, &description, width, height)?);
        }
        let readable = self.readable.clone().ok_or_else(null_output)?;
        device.copy_region(&readable, source, width, height);
        Ok(readable)
    }
}

struct Views<V> {
    entries: Vec<(ID3D11Texture2D, V)>,
}

impl<V> Default for Views<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<V: Clone> Views<V> {
    fn get(
        &mut self,
        texture: &ID3D11Texture2D,
        create: impl FnOnce() -> windows::core::Result<V>,
    ) -> windows::core::Result<V> {
        if let Some((_, view)) = self.entries.iter().find(|(known, _)| known == texture) {
            return Ok(view.clone());
        }
        if self.entries.len() >= MAX_VIEWS {
            self.entries.clear();
        }
        let view = create()?;
        self.entries.push((texture.clone(), view.clone()));
        Ok(view)
    }
}

fn created<T>(object: Option<T>) -> windows::core::Result<T> {
    object.ok_or_else(null_output)
}

const fn bind_flag(flag: i32) -> u32 {
    flag.cast_unsigned()
}

#[expect(unsafe_code)]
fn constant_buffer(device: &ID3D11Device, curve: &Curve) -> windows::core::Result<ID3D11Buffer> {
    let constants = curve.constants();
    let description = D3D11_BUFFER_DESC {
        ByteWidth: cb_size::<[f32; 4]>(),
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: bind_flag(D3D11_BIND_CONSTANT_BUFFER.0),
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: constants.as_ptr().cast::<c_void>(),
        ..Default::default()
    };
    let mut buffer = None;
    // SAFETY: the description, the 16 bytes of data and the out-parameter are live.
    unsafe {
        device.CreateBuffer(
            &raw const description,
            Some(&raw const data),
            Some(&raw mut buffer),
        )
    }?;
    created(buffer)
}

#[expect(unsafe_code)]
fn texture_description(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut description = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: writes the description of a live texture into a live structure.
    unsafe { texture.GetDesc(&raw mut description) };
    description
}

#[expect(unsafe_code)]
fn readable_texture(
    device: &ID3D11Device,
    source: &D3D11_TEXTURE2D_DESC,
    width: u32,
    height: u32,
) -> windows::core::Result<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: source.Format,
        SampleDesc: source.SampleDesc,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flag(D3D11_BIND_SHADER_RESOURCE.0),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: the description and the out-parameter are live for the call.
    unsafe { device.CreateTexture2D(&raw const description, None, Some(&raw mut texture)) }?;
    created(texture)
}

#[expect(unsafe_code)]
fn shader_resource_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11ShaderResourceView> {
    let mut view = None;
    // SAFETY: a live texture the shader may read; the out-parameter is live.
    unsafe { device.CreateShaderResourceView(texture, None, Some(&raw mut view)) }?;
    created(view)
}

#[expect(unsafe_code)]
fn render_target_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11RenderTargetView> {
    let mut view = None;
    // SAFETY: a live texture created as a render target; the out-parameter is live.
    unsafe { device.CreateRenderTargetView(texture, None, Some(&raw mut view)) }?;
    created(view)
}

#[cfg(test)]
#[expect(unsafe_code)]
mod tests {
    use super::*;
    use windows::Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_USAGE_STAGING,
                D3D11CreateDevice,
            },
            Dxgi::Common::{
                DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32B32A32_FLOAT,
                DXGI_SAMPLE_DESC,
            },
        },
    };

    const EPSILON: f32 = 1e-4;

    fn curve() -> Curve {
        Curve::new(Some(200.0), 1000.0)
    }

    fn grey(nits: f32) -> [f32; 3] {
        [nits / SCRGB_NITS; 3]
    }

    fn linear(coded: f32) -> f32 {
        if coded <= 0.040_45 {
            coded / 12.92
        } else {
            ((coded + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn the_srgb_transfer_has_its_known_values() {
        assert!(srgb(0.0).abs() < EPSILON);
        assert!((srgb(0.003_130_8) - 0.040_45).abs() < EPSILON);
        assert!((srgb(0.18) - 0.461_356).abs() < EPSILON);
        assert!((srgb(1.0) - 1.0).abs() < EPSILON);
    }

    #[test]
    fn unknown_or_unusable_levels_take_the_defaults() {
        let unknown = Curve::new(None, 0.0);
        assert!((unknown.white_nits() - 200.0).abs() < EPSILON);
        assert!((unknown.peak_nits() - 1000.0).abs() < EPSILON);
        let below_white = Curve::new(Some(300.0), 270.0);
        assert!((below_white.peak_nits() - 1000.0).abs() < EPSILON);
        assert!((Curve::new(Some(0.0), 600.0).white_nits() - 200.0).abs() < EPSILON);
    }

    #[test]
    fn levels_below_the_knee_pass_unchanged() {
        let curve = curve();
        let [red, ..] = curve.apply(grey(100.0));
        assert!((red - srgb(0.5)).abs() < EPSILON);
        assert!(curve.apply(grey(0.0))[0].abs() < EPSILON);
        assert!((linear(curve.apply(grey(160.0))[0]) - KNEE).abs() < 1e-3);
    }

    #[test]
    fn sdr_white_lands_just_below_full_scale() {
        let [red, ..] = curve().apply(grey(200.0));
        assert!((linear(red) - 0.902_44).abs() < 1e-3, "{}", linear(red));
        assert!((red * 255.0).round() >= 243.0);
    }

    #[test]
    fn the_peak_lands_on_full_scale_and_beyond_it_clips() {
        let curve = curve();
        assert!((curve.apply(grey(1000.0))[0] - 1.0).abs() < EPSILON);
        assert!((curve.apply(grey(4000.0))[0] - 1.0).abs() < EPSILON);
    }

    #[test]
    fn the_curve_rises_throughout_and_has_no_corner_at_the_knee() {
        let curve = curve();
        let levels: Vec<f32> = (0..=1000)
            .map(|nits| curve.apply(grey(nits as f32))[0])
            .collect();
        assert!(levels.windows(2).all(|pair| pair[1] > pair[0]));
        let at_knee = linear(curve.apply(grey(160.0))[0]);
        let above = linear(curve.apply(grey(160.2))[0]);
        let slope = (above - at_knee) / (0.2 / 200.0);
        assert!((slope - 1.0).abs() < 0.02, "{slope}");
    }

    #[test]
    fn a_bright_colour_keeps_its_hue() {
        let [red, green, blue] = curve().apply([600.0 / SCRGB_NITS, 300.0 / SCRGB_NITS, 0.0]);
        assert!((linear(green) / linear(red) - 0.5).abs() < 1e-3);
        assert!(blue.abs() < EPSILON);
    }

    #[test]
    fn colours_outside_bt709_are_clipped_to_it() {
        let [red, green, blue] = curve().apply([-0.2, 1.0, 0.5]);
        assert!(red.abs() < EPSILON);
        assert!(green > blue);
    }

    #[test]
    fn a_display_without_headroom_clips_at_white() {
        let curve = Curve::new(Some(1000.0), 1000.0);
        assert!(curve.rolloff.abs() < EPSILON);
        assert!((curve.apply(grey(1000.0))[0] - 1.0).abs() < EPSILON);
    }

    fn software_device() -> Device {
        let mut d3d = None;
        let mut context = None;
        // SAFETY: the out-parameters are live.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&raw mut d3d),
                None,
                Some(&raw mut context),
            )
        }
        .expect("create a WARP device");
        Device {
            d3d: d3d.expect("a device"),
            context: context.expect("a context"),
            adapter: "WARP".to_owned(),
        }
    }

    fn texture(
        device: &Device,
        format: DXGI_FORMAT,
        bind: i32,
        pixels: &[u8],
        bytes: u32,
    ) -> ID3D11Texture2D {
        let width = u32::try_from(pixels.len()).expect("a short row") / bytes;
        let description = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind_flag(bind),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: width * bytes,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: the data holds one row of the size described.
        unsafe {
            device.d3d.CreateTexture2D(
                &raw const description,
                Some(&raw const data),
                Some(&raw mut texture),
            )
        }
        .expect("create a texture");
        texture.expect("a texture")
    }

    fn read_back(device: &Device, texture: &ID3D11Texture2D) -> Vec<u8> {
        let mut description = texture_description(texture);
        description.Usage = D3D11_USAGE_STAGING;
        description.BindFlags = 0;
        description.CPUAccessFlags = bind_flag(D3D11_CPU_ACCESS_READ.0);
        let mut staging = None;
        // SAFETY: the description and out-parameter are live.
        unsafe {
            device
                .d3d
                .CreateTexture2D(&raw const description, None, Some(&raw mut staging))
        }
        .expect("create a staging texture");
        let staging = staging.expect("a staging texture");
        device.copy(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a staging texture made for CPU reads; `mapped` outlives the call.
        unsafe {
            device
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        }
        .expect("map the staging texture");
        // SAFETY: while mapped, the first row holds Width × 4 readable bytes; they are copied
        // out before the unmap below.
        let row = unsafe {
            std::slice::from_raw_parts(mapped.pData.cast::<u8>(), description.Width as usize * 4)
        }
        .to_vec();
        // SAFETY: undoes the map above; nothing points into the mapping any more.
        unsafe { device.context.Unmap(&staging, 0) };
        row
    }

    fn compare_with_reference(source_bind: i32) {
        let device = software_device();
        let curve = curve();
        let colours: Vec<[f32; 3]> = [0.0, 40.0, 100.0, 160.0, 200.0, 400.0, 1000.0, 3000.0]
            .into_iter()
            .map(grey)
            .chain([[600.0 / SCRGB_NITS, 300.0 / SCRGB_NITS, -0.1]])
            .collect();
        let pixels: Vec<u8> = colours
            .iter()
            .flat_map(|[red, green, blue]| [*red, *green, *blue, 1.0])
            .flat_map(f32::to_le_bytes)
            .collect();
        let source = texture(
            &device,
            DXGI_FORMAT_R32G32B32A32_FLOAT,
            source_bind,
            &pixels,
            16,
        );
        let blank = vec![0; colours.len() * 4];
        let target = texture(
            &device,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET.0,
            &blank,
            4,
        );
        let mut mapper = ToneMapper::new(&device, curve).expect("compile the pass");
        let width = u32::try_from(colours.len()).expect("a short row");
        mapper
            .convert(&device, &source, &target, width, 1)
            .expect("run the pass");
        let written = read_back(&device, &target);
        for (colour, bgra) in colours.iter().zip(written.as_chunks::<4>().0) {
            let expected = curve.apply(*colour).map(|level| level * 255.0);
            let got = [bgra[2], bgra[1], bgra[0]].map(f32::from);
            for (expected, got) in expected.into_iter().zip(got) {
                assert!(
                    (expected - got).abs() <= 1.0,
                    "{colour:?}: {expected} vs {got}"
                );
            }
            assert_eq!(bgra[3], 255);
        }
    }

    #[test]
    fn the_shader_matches_the_reference_on_the_gpu() {
        compare_with_reference(D3D11_BIND_SHADER_RESOURCE.0);
    }

    #[test]
    fn a_source_the_shader_cannot_read_is_copied_first() {
        compare_with_reference(0);
    }
}
