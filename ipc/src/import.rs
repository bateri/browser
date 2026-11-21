//! A frame as a wgpu texture (Metal), for the receiving side.

use crate::Surface;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use std::ptr::NonNull;

/// A texture that sees the surface's memory itself, no copy. The helper's next frame may change
/// that memory while it is sampled (Chromium recycles its surfaces), so only `copy` uses it.
fn direct(device: &wgpu::Device, surface: &Surface) -> Result<wgpu::Texture, String> {
    let (width, height) = surface.size();
    let (width, height) = (width as u32, height as u32);
    let format = wgpu::TextureFormat::Bgra8Unorm;
    let desc = wgpu::TextureDescriptor {
        label: Some("browser surface"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    };
    let io = NonNull::new(surface.as_ptr().cast::<IOSurfaceRef>()).ok_or("null surface")?;
    // SAFETY: `Surface` holds a reference of a live IOSurface for as long as `surface` lives.
    let io = unsafe { io.as_ref() };
    let metal_desc = MTLTextureDescriptor::new();
    unsafe {
        metal_desc.setWidth(width as _);
        metal_desc.setHeight(height as _);
        metal_desc.setMipmapLevelCount(1);
        metal_desc.setSampleCount(1);
        metal_desc.setTextureType(MTLTextureType::Type2D);
        metal_desc.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        metal_desc.setUsage(MTLTextureUsage::ShaderRead);
        metal_desc.setStorageMode(MTLStorageMode::Shared);
    }
    let hal = unsafe {
        let guard = device.as_hal::<wgpu::wgc::api::Metal>();
        let Some(hal_device) = guard else {
            return Err("the device is not Metal".into());
        };
        let texture = hal_device
            .raw_device()
            .newTextureWithDescriptor_iosurface_plane(metal_desc.as_ref(), io, 0)
            .ok_or("newTextureWithDescriptor:iosurface:plane: failed")?;
        <wgpu::wgc::api::Metal as wgpu::hal::Api>::Device::texture_from_raw(
            texture,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
            None,
        )
    };
    Ok(unsafe {
        device.create_texture_from_hal::<wgpu::wgc::api::Metal>(
            hal,
            &desc,
            wgpu::TextureUses::RESOURCE,
        )
    })
}

/// The frame as a texture of its own: a GPU copy, waited for, so what the helper draws next cannot
/// change it.
pub fn copy(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    surface: &Surface,
) -> Result<wgpu::Texture, String> {
    let source = direct(device, surface)?;
    let owned = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("browser frame"),
        size: source.size(),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: source.format(),
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("browser copy"),
    });
    encoder.copy_texture_to_texture(source.as_image_copy(), owned.as_image_copy(), source.size());
    let submission = queue.submit([encoder.finish()]);
    loop {
        match device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission.clone()),
            timeout: None,
        }) {
            Ok(_) => break,
            Err(wgpu::PollError::Timeout) => continue,
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
    Ok(owned)
}

/// Reads `points` (x, y) of a BGRA texture back to the CPU, through a buffer.
pub fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    points: &[(u32, u32)],
) -> Vec<[u8; 4]> {
    let size = texture.size();
    let stride = (size.width * 4).next_multiple_of(256);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (stride * size.height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(size.height),
            },
        },
        size,
    );
    queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let data = slice.get_mapped_range().expect("mapped");
    points
        .iter()
        .map(|&(x, y)| {
            let (x, y) = (x.min(size.width - 1), y.min(size.height - 1));
            let o = (y * stride + x * 4) as usize;
            [data[o], data[o + 1], data[o + 2], data[o + 3]]
        })
        .collect()
}
