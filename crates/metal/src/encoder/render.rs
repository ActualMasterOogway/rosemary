//! `MTLRenderCommandEncoder`: draws.

use super::*;

pub(super) unsafe extern "C" fn draw_primitives(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
    instance_count: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let Some((device, command)) = encoder.prepare_draw(primitive, 0) else {
        return;
    };
    // SAFETY: recording into this command buffer.
    unsafe {
        device.cmd_draw(
            command,
            vertex_count as u32,
            instance_count.max(1) as u32,
            vertex_start as u32,
            0,
        )
    };
}

pub(super) unsafe extern "C" fn draw_primitives_base(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
    instance_count: u64,
    base_instance: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let Some((device, command)) = encoder.prepare_draw(primitive, base_instance) else {
        return;
    };
    // SAFETY: recording into this command buffer.
    unsafe {
        device.cmd_draw(
            command,
            vertex_count as u32,
            instance_count.max(1) as u32,
            vertex_start as u32,
            base_instance as u32,
        )
    };
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn draw_indexed_primitives(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    unsafe {
        draw_indexed(
            encoder,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            instance_count,
            0,
            0,
        )
    };
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn draw_indexed_primitives_base(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
    base_vertex: i64,
    base_instance: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    unsafe {
        draw_indexed(
            encoder,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            instance_count,
            base_vertex,
            base_instance,
        )
    };
}

/// # Safety
///
/// `index_buffer` must be null or an `MTLBuffer` of this crate's.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn draw_indexed(
    encoder: &mut Encoder,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
    base_vertex: i64,
    base_instance: u64,
) {
    if index_buffer.is_null() {
        return;
    }
    let Some((device, command)) = encoder.prepare_draw(primitive, base_instance) else {
        return;
    };
    // SAFETY: the caller guarantees the buffer's class.
    let buffer = unsafe { object::body::<Buffer>(index_buffer) };
    encoder.command().retain(index_buffer);
    let binding = (
        buffer.inner.buffer,
        index_offset,
        match index_type {
            1 => vk::IndexType::UINT32,
            _ => vk::IndexType::UINT16,
        },
    );
    if encoder.index_buffer != binding {
        encoder.index_buffer = binding;
        // SAFETY: recording into this command buffer.
        unsafe { device.cmd_bind_index_buffer(command, binding.0, binding.1, binding.2) };
    }
    // SAFETY: recording into this command buffer.
    unsafe {
        device.cmd_draw_indexed(
            command,
            index_count as u32,
            instance_count.max(1) as u32,
            0,
            base_vertex as i32,
            base_instance as u32,
        );
    };
}
