//! Composition on the GPU: GTK's buffers, shared memory or dma-bufs, drawn
//! with GLES into dma-bufs of the compositor's own, which the presenter
//! samples without a copy, or which are read back for one that cannot.
//!
//! The GPU is the presenter's where it has one, since a dma-buf is only
//! cheap to share within one device. GTK and WebKit are told about it
//! through `linux-dmabuf`'s feedback, which makes it their device too.

use std::sync::Arc;

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::vulkan::{ImageUsageFlags, VulkanAllocator, VulkanImage};
use smithay::backend::allocator::{Allocator, Buffer, Fourcc, Modifier};
use smithay::backend::drm::DrmNode;
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame as _, ImportDma, Renderer};
use smithay::backend::vulkan::{Instance, PhysicalDevice, version::Version};
use smithay::desktop::PopupManager;
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder};
use smithay::wayland::shell::xdg::ToplevelSurface;

use super::Frame;

/// The format windows are composed in: premultiplied B, G, R, A in memory,
/// which is what `appkit::backing` reads.
const FORMAT: Fourcc = Fourcc::Argb8888;

/// The most buffers one window composes into: one being shown, one waiting
/// to be, one being drawn, and one to spare while the presenter lets go.
const BUFFERS: usize = 4;

pub struct Gpu {
    renderer: GlesRenderer,
    allocator: VulkanAllocator,
    node: DrmNode,
    /// The modifiers a composed window's buffer may have: ones GLES renders
    /// to and, when there is a presenter, ones it samples.
    modifiers: Vec<Modifier>,
}

/// One buffer a window is composed into, and the view of it handed out.
/// Whoever holds `shared` may be reading it, so it is drawn into again only
/// once this pool holds the only reference.
pub struct Target {
    dmabuf: Dmabuf,
    _image: VulkanImage,
    shared: Arc<appkit::backing::Dmabuf>,
}

#[derive(Default)]
pub struct Pool {
    targets: Vec<Target>,
}

impl Gpu {
    /// A renderer on `wanted`, the presenter's render node, with the
    /// modifiers it samples; or on the first GPU EGL lists, for a presenter
    /// that samples none.
    pub fn new(wanted: Option<&appkit::backing::Importer>) -> Result<Gpu, String> {
        let device = EGLDevice::enumerate()
            .map_err(|e| format!("no EGL devices: {e}"))?
            .filter_map(|device| {
                let node = device.try_get_render_node().ok().flatten()?;
                Some((device, node))
            })
            .find(|(_, node)| wanted.is_none_or(|w| node.dev_id() == w.device));
        let Some((device, node)) = device else {
            return Err(match wanted {
                Some(w) => format!("EGL has no device for render node {:#x}", w.device),
                None => "EGL has no device with a render node".to_string(),
            });
        };
        // SAFETY: the device outlives nothing made from it here; the display
        // keeps its own reference.
        let display =
            unsafe { EGLDisplay::new(device) }.map_err(|e| format!("no EGL display: {e}"))?;
        let context = EGLContext::new(&display).map_err(|e| format!("no EGL context: {e}"))?;
        // SAFETY: the context is new and current on no other thread.
        let renderer =
            unsafe { GlesRenderer::new(context) }.map_err(|e| format!("no GLES renderer: {e}"))?;
        let instance =
            Instance::new(Version::VERSION_1_2, None).map_err(|e| format!("no Vulkan: {e}"))?;
        let physical = PhysicalDevice::enumerate(&instance)
            .map_err(|e| format!("no Vulkan devices: {e:?}"))?
            .find(|physical| physical.render_node().ok().flatten() == Some(node))
            .ok_or_else(|| format!("Vulkan has no device for {node}"))?;
        let allocator = VulkanAllocator::new(
            &physical,
            ImageUsageFlags::COLOR_ATTACHMENT | ImageUsageFlags::SAMPLED,
        )
        .map_err(|e| format!("no Vulkan allocator: {e}"))?;
        let mut modifiers: Vec<Modifier> = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| format.code == FORMAT)
            .map(|format| format.modifier)
            .filter(|modifier| *modifier != Modifier::Invalid)
            .collect();
        if let Some(wanted) = wanted {
            modifiers.retain(|modifier| {
                wanted
                    .formats
                    .iter()
                    .any(|&(code, m)| code == FORMAT as u32 && m == u64::from(*modifier))
            });
        }
        if modifiers.is_empty() {
            return Err("GLES renders to no modifier the presenter samples".to_string());
        }
        Ok(Gpu {
            renderer,
            allocator,
            node,
            modifiers,
        })
    }

    /// What GTK is told: this device, and the dma-bufs it imports.
    pub fn feedback(&self) -> Result<DmabufFeedback, String> {
        let formats = self.renderer.dmabuf_formats();
        DmabufFeedbackBuilder::new(self.node.dev_id(), formats)
            .build()
            .map_err(|e| format!("no dma-buf feedback: {e}"))
    }

    pub fn import(&mut self, dmabuf: &Dmabuf) -> bool {
        self.renderer.import_dmabuf(dmabuf, None).is_ok()
    }

    /// The window's surfaces and popups drawn at `scale` into a buffer of
    /// `pool`'s, handed out as a dma-buf; or read back into pixels when
    /// `pixels` is set. `None` when there is nothing to draw, or no buffer
    /// is free, or drawing failed.
    pub fn compose(
        &mut self,
        toplevel: &ToplevelSurface,
        scale: f64,
        pool: &mut Pool,
        pixels: bool,
    ) -> Option<Frame> {
        let root = toplevel.wl_surface();
        let geometry = super::state::geometry(root);
        let size: Size<i32, Physical> = geometry.size.to_f64().to_physical(scale).to_i32_round();
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let scale = Scale::from(scale);
        let at = |logical: Point<i32, smithay::utils::Logical>| -> Point<i32, Physical> {
            logical.to_f64().to_physical(scale).to_i32_round()
        };
        // Topmost first: the newest popup, then the rest, then the window.
        let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        let popups: Vec<_> = PopupManager::popups_for_surface(root).collect();
        for (popup, offset) in popups.iter().rev() {
            elements.extend(render_elements_from_surface_tree(
                &mut self.renderer,
                popup.wl_surface(),
                at(*offset - popup.geometry().loc),
                scale,
                1.0,
                Kind::Unspecified,
            ));
        }
        elements.extend(render_elements_from_surface_tree(
            &mut self.renderer,
            root,
            at(Point::default() - geometry.loc),
            scale,
            1.0,
            Kind::Unspecified,
        ));

        let target = self.target(pool, size)?;
        let shared = target.shared.clone();
        let whole = Rectangle::from_size(size);
        let drawn = (|| -> Result<Option<Vec<u8>>, String> {
            let mut framebuffer = self
                .renderer
                .bind(&mut target.dmabuf)
                .map_err(|e| format!("bind: {e}"))?;
            let mut frame = self
                .renderer
                .render(&mut framebuffer, size, Transform::Normal)
                .map_err(|e| format!("render: {e}"))?;
            frame
                .clear(Color32F::TRANSPARENT, &[whole])
                .map_err(|e| format!("clear: {e}"))?;
            draw_render_elements::<GlesRenderer, _, _>(&mut frame, scale, &elements, &[whole])
                .map_err(|e| format!("draw: {e}"))?;
            let sync = frame.finish().map_err(|e| format!("finish: {e}"))?;
            // Finished before anyone else reads it: the presenter's API does
            // not wait on GL's work by itself.
            let _ = sync.wait();
            if !pixels {
                return Ok(None);
            }
            let region = Rectangle::from_size((size.w, size.h).into());
            let mapping = self
                .renderer
                .copy_framebuffer(&framebuffer, region, FORMAT)
                .map_err(|e| format!("read back: {e}"))?;
            let bytes = self
                .renderer
                .map_texture(&mapping)
                .map_err(|e| format!("map: {e}"))?;
            Ok(Some(bytes.to_vec()))
        })();
        let picture = match drawn {
            Ok(Some(bytes)) => appkit::backing::Picture::Pixels(bytes),
            Ok(None) => appkit::backing::Picture::Dmabuf(shared),
            Err(why) => {
                eprintln!("rosemary: webkit: compositor: composing on the GPU failed: {why}");
                return None;
            }
        };
        Some(Frame {
            width: size.w as u32,
            height: size.h as u32,
            picture,
        })
    }

    /// A buffer of `pool`'s at `size` that nobody else holds, made if there
    /// is room for one more.
    fn target<'a>(
        &mut self,
        pool: &'a mut Pool,
        size: Size<i32, Physical>,
    ) -> Option<&'a mut Target> {
        let free = |target: &Target| Arc::strong_count(&target.shared) == 1;
        let fits = |target: &Target| {
            (target.shared.width, target.shared.height) == (size.w as u32, size.h as u32)
        };
        // Buffers of another size go as soon as they are free.
        pool.targets.retain(|target| fits(target) || !free(target));
        if let Some(at) = pool.targets.iter().position(|t| fits(t) && free(t)) {
            return pool.targets.get_mut(at);
        }
        if pool.targets.len() >= BUFFERS {
            return None;
        }
        let image = match self.allocator.create_buffer(
            size.w as u32,
            size.h as u32,
            FORMAT,
            &self.modifiers,
        ) {
            Ok(image) => image,
            Err(why) => {
                eprintln!("rosemary: webkit: compositor: no buffer to compose into: {why}");
                return None;
            }
        };
        let dmabuf = match image.export() {
            Ok(dmabuf) => dmabuf,
            Err(why) => {
                eprintln!("rosemary: webkit: compositor: a buffer could not be shared: {why}");
                return None;
            }
        };
        let planes = dmabuf
            .handles()
            .zip(dmabuf.offsets())
            .zip(dmabuf.strides())
            .map(|((fd, offset), stride)| {
                Some(appkit::backing::DmabufPlane {
                    fd: fd.try_clone_to_owned().ok()?,
                    offset,
                    stride,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let format = dmabuf.format();
        let shared = Arc::new(appkit::backing::Dmabuf {
            width: size.w as u32,
            height: size.h as u32,
            fourcc: format.code as u32,
            modifier: format.modifier.into(),
            planes,
        });
        pool.targets.push(Target {
            dmabuf,
            _image: image,
            shared,
        });
        pool.targets.last_mut()
    }
}
