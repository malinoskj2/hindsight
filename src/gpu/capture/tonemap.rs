//! The pass that turns the picture of a display in HDR into what the encoder takes. Windows
//! composes such a display in FP16 scRGB (linear light, BT.709 primaries, 1.0 = 80 nits, SDR
//! white at the user's SDR white level); the encoder takes 8-bit sRGB, signalled as BT.709.
//!
//! The pass scales the picture so SDR white lands on 1.0, keeps everything up to a knee below
//! it as it is, and rolls the highlights above the knee off with a Reinhard curve whose white
//! point is the display's peak, so a highlight at the peak lands on 1.0 and the ones between
//! keep their gradation instead of clipping to a flat white. Then it applies the sRGB transfer,
//! the same coding an SDR display's captures have. It runs as a pixel shader drawing one
//! triangle over the encoder's input texture, which is a render target already.

use crate::gpu::device::Device;
use std::ffi::c_void;
use windows::{
    Win32::Graphics::{
        Direct3D::{
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCOMPILE_ENABLE_STRICTNESS,
            Fxc::D3DCOMPILE_OPTIMIZATION_LEVEL3, Fxc::D3DCompile, ID3DBlob,
        },
        Direct3D11::{
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
            D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11Buffer, ID3D11DepthStencilView,
            ID3D11Device, ID3D11InputLayout, ID3D11Multithread, ID3D11PixelShader,
            ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
        },
    },
    core::{Interface, PCSTR, s},
};

/// The shader's source; see the file for how it is compiled.
const SHADER: &str = include_str!("tonemap.hlsl");
/// The level, in units of SDR white, above which highlights are compressed. Below it the
/// picture is kept exactly; SDR white itself comes out at about 0.90 in linear light (243 of
/// 255 once coded), the price of leaving room above it for the highlights.
const KNEE: f32 = 0.8;
/// The SDR white level assumed when Windows does not report one, in nits.
const DEFAULT_WHITE_NITS: f32 = 200.0;
/// The peak assumed when the display reports none above its SDR white, in nits: what most
/// HDR monitors and games aim for.
const DEFAULT_PEAK_NITS: f32 = 1000.0;
/// Nits of scRGB 1.0.
const SCRGB_NITS: f32 = super::display::SCRGB_NITS;
/// Views kept per kind before the cache is emptied; a capture has a handful of surfaces and
/// the encoder a handful of inputs, so only a source that keeps replacing its surfaces fills it.
const MAX_VIEWS: usize = 16;

/// The tone curve's parameters, as the shader's constant buffer holds them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Curve {
    /// scRGB to units of SDR white.
    scale: f32,
    knee: f32,
    /// The Reinhard curve's strength above the knee.
    rolloff: f32,
    /// SDR white and the peak in nits, for the log.
    white_nits: f32,
    peak_nits: f32,
}

impl Curve {
    /// The curve for a display whose SDR white is `white_nits` (200 when unknown) and whose
    /// peak is `peak_nits` (1000 when unknown or not above the white).
    pub(super) fn new(white_nits: Option<f32>, peak_nits: f32) -> Self {
        let white = white_nits
            .filter(|nits| *nits > 0.0)
            .unwrap_or(DEFAULT_WHITE_NITS);
        let peak = if peak_nits > white {
            peak_nits
        } else {
            DEFAULT_PEAK_NITS.max(white)
        };
        // The Reinhard curve x / (1 + x b) rises with slope 1, so it meets the straight part
        // smoothly at the knee; b is chosen so that the peak, `headroom` times SDR white,
        // lands on 1.0. A display with no headroom gets b = 0 and plain clipping.
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

    /// The SDR white level the curve maps to 1.0 before its roll-off, in nits.
    pub(super) const fn white_nits(&self) -> f32 {
        self.white_nits
    }

    /// The brightness the curve maps to 1.0, in nits.
    pub(super) const fn peak_nits(&self) -> f32 {
        self.peak_nits
    }

    /// The sRGB-coded colour, each component in 0 to 1, the shader writes for the scRGB
    /// colour `rgb`. The shader is the one that runs; this is its reference.
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

    /// The constant buffer's contents.
    const fn constants(&self) -> [f32; 4] {
        [self.scale, self.knee, self.rolloff, 0.0]
    }
}

/// The sRGB transfer of a linear level in 0 to 1.
#[cfg(test)]
fn srgb(level: f32) -> f32 {
    if level <= 0.003_130_8 {
        level * 12.92
    } else {
        1.055 * level.powf(1.0 / 2.4) - 0.055
    }
}

/// The compiled pass and the views it draws through, for one session.
pub(super) struct ToneMapper {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    /// The device's lock, held while the pass sets its state and draws, so the capture
    /// API's own threads cannot change the immediate context in between.
    lock: ID3D11Multithread,
    sources: Views<ID3D11ShaderResourceView>,
    targets: Views<ID3D11RenderTargetView>,
    /// A copy of the source the shader can read, for a source created without that right.
    readable: Option<ID3D11Texture2D>,
}

impl ToneMapper {
    /// Compiles the pass for `curve` on the device.
    pub(super) fn new(device: &Device, curve: Curve) -> windows::core::Result<Self> {
        let vertex_code = compile(s!("fullscreen"), s!("vs_5_0"))?;
        let pixel_code = compile(s!("tonemap"), s!("ps_5_0"))?;
        let mut vertex = None;
        let mut pixel = None;
        // SAFETY: the bytecode was compiled for these stages a moment ago; the out-parameters
        // are live.
        unsafe {
            device
                .d3d
                .CreateVertexShader(blob_bytes(&vertex_code), None, Some(&raw mut vertex))?;
            device
                .d3d
                .CreatePixelShader(blob_bytes(&pixel_code), None, Some(&raw mut pixel))?;
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

    /// Queues the pass that tone-maps the top-left `width` × `height` of `source`, an scRGB
    /// texture, into `target`, an 8-bit BGRA render target at least that large.
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
        // SAFETY: every object bound belongs to this device and outlives the draw, which is
        // queued before the views are unbound; the device's lock is held throughout and left
        // on every path, since nothing in between can fail.
        unsafe {
            self.lock.Enter();
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(input)]));
            context.RSSetViewports(Some(&[viewport]));
            context.OMSetRenderTargets(Some(&[Some(output)]), None::<&ID3D11DepthStencilView>);
            context.Draw(3, 0);
            // Unbound so the encoder and the capture may use both textures freely.
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
            self.lock.Leave();
        }
        Ok(())
    }

    /// `source` itself when a shader may read it, or else a copy of its top-left `width` ×
    /// `height` in a texture of the pass's own that it may.
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
        let readable = self
            .readable
            .clone()
            .ok_or_else(windows::core::Error::empty)?;
        device.copy_region(&readable, source, width, height);
        Ok(readable)
    }
}

/// Views of textures, made the first time each texture comes and kept with it, so the same
/// texture is never viewed twice.
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
    /// The view of `texture`, made with `create` if there is none yet.
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

/// Compiles the entry point `entry` of the shader for `target`.
fn compile(entry: PCSTR, target: PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source is a live string of the length given; the out-parameters are live.
    let compiled = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast::<c_void>(),
            SHADER.len(),
            s!("tonemap.hlsl"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &raw mut code,
            Some(&raw mut errors),
        )
    };
    if let Err(error) = compiled {
        let message = errors
            .as_ref()
            .map(|errors| String::from_utf8_lossy(blob_bytes(errors)).into_owned())
            .unwrap_or_default();
        return Err(windows::core::Error::new(
            error.code(),
            format!("compile the HDR tone-map shader: {message}"),
        ));
    }
    created(code)
}

/// The bytes a blob holds.
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: a blob's pointer and size describe its own buffer, which lives as long as it.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// An object a call returned through an out-parameter, which it fills on success.
fn created<T>(object: Option<T>) -> windows::core::Result<T> {
    object.ok_or_else(windows::core::Error::empty)
}

/// A Direct3D flag as the `u32` the descriptions hold.
const fn bind_flag(flag: i32) -> u32 {
    flag.cast_unsigned()
}

/// The constant buffer holding `curve`, fixed for the session.
fn constant_buffer(device: &ID3D11Device, curve: &Curve) -> windows::core::Result<ID3D11Buffer> {
    let constants = curve.constants();
    let description = D3D11_BUFFER_DESC {
        ByteWidth: u32::try_from(size_of_val(&constants)).unwrap_or(16),
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: bind_flag(D3D11_BIND_CONSTANT_BUFFER.0),
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: constants.as_ptr().cast::<c_void>(),
        ..Default::default()
    };
    let mut buffer = None;
    // SAFETY: the description and the initial data, 16 bytes as described, are live for the
    // call; the out-parameter too.
    unsafe {
        device.CreateBuffer(
            &raw const description,
            Some(&raw const data),
            Some(&raw mut buffer),
        )
    }?;
    created(buffer)
}

/// A texture's description.
fn texture_description(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut description = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: writes the description of a live texture into a live structure.
    unsafe { texture.GetDesc(&raw mut description) };
    description
}

/// A `width` × `height` texture of `source`'s format that a shader may read.
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

/// A view through which the shader reads all of `texture`.
fn shader_resource_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11ShaderResourceView> {
    let mut view = None;
    // SAFETY: a live texture the shader may read; the out-parameter is live.
    unsafe { device.CreateShaderResourceView(texture, None, Some(&raw mut view)) }?;
    created(view)
}

/// A view through which the pass draws into `texture`.
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

    /// Tolerance on a coded level for values computed two ways in single precision.
    const EPSILON: f32 = 1e-4;

    /// The display these tests assume: SDR white at 200 nits, a 1000-nit peak.
    fn curve() -> Curve {
        Curve::new(Some(200.0), 1000.0)
    }

    /// scRGB grey at `nits`.
    fn grey(nits: f32) -> [f32; 3] {
        [nits / SCRGB_NITS; 3]
    }

    /// Linear light of a coded level, the inverse of [`srgb`].
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
        // Half of SDR white, 100 nits, is 0.5 in linear light.
        let [red, ..] = curve.apply(grey(100.0));
        assert!((red - srgb(0.5)).abs() < EPSILON);
        assert!(curve.apply(grey(0.0))[0].abs() < EPSILON);
        assert!((linear(curve.apply(grey(160.0))[0]) - KNEE).abs() < 1e-3);
    }

    #[test]
    fn sdr_white_lands_just_below_full_scale() {
        // 0.8 + 0.2 / (1 + 0.2 × (1 / 0.2 − 1 / 4.2)), with 1000 nits over 200 = 5.
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
        // In linear light the slope just above the knee is the slope below it, 1.
        let at_knee = linear(curve.apply(grey(160.0))[0]);
        let above = linear(curve.apply(grey(160.2))[0]);
        let slope = (above - at_knee) / (0.2 / 200.0);
        assert!((slope - 1.0).abs() < 0.02, "{slope}");
    }

    #[test]
    fn a_bright_colour_keeps_its_hue() {
        // An orange highlight at 600 nits in its brightest component.
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

    /// A Direct3D 11 device on WARP, the software rasteriser every Windows has.
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

    /// A one-row texture of `format` holding `pixels`, which take `bytes` each.
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

    /// The BGRA bytes of a one-row texture, read back through a staging copy.
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
        // SAFETY: maps the staging texture for reading; its first row holds Width × 4 bytes,
        // read before it is unmapped.
        unsafe {
            device
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
                .expect("map the staging texture");
            let row = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                description.Width as usize * 4,
            )
            .to_vec();
            device.context.Unmap(&staging, 0);
            row
        }
    }

    /// Runs the pass on WARP over a row of scRGB colours, in a source a shader may read or
    /// not, and checks every pixel against [`Curve::apply`].
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
