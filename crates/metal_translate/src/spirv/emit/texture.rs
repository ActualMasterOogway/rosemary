//! Texture sampling, reads, writes and queries.

use super::*;

impl Emitter {
    /// Emit one such instruction.
    pub(super) fn emit_texture(
        &mut self,
        function: &Function,
        instruction: &crate::ir::Instruction,
        values: &HashMap<ValueId, u32>,
        result_id: Option<u32>,
    ) -> Result<(), TranslateError> {
        match &instruction.kind {
            InstructionKind::TextureWrite {
                texture,
                coordinate,
                array_index,
                value,
            } => {
                let image = self.operand(texture, values)?;
                let coordinate =
                    self.texture_coordinate(function, coordinate, array_index.as_ref(), values)?;
                let value = self.operand(value, values)?;
                self.builder
                    .image_write(image, coordinate, value, None, [])
                    .map_err(build_error)?;
            }
            InstructionKind::TextureSample {
                texture,
                sampler,
                coordinate,
                array_index,
                offset,
                lod,
                gradient,
                gather,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("texture sample has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let Some(Type::Texture(image_type)) = operand_type(function, texture) else {
                    return Err(emission("texture sample has no texture operand"));
                };
                let image_id = self.type_id(&Type::Texture(image_type))?;
                let sampled_type = self.builder.type_sampled_image(image_id);
                let image = self.operand(texture, values)?;
                let sampler = self.operand(sampler, values)?;
                let sampled = self
                    .builder
                    .sampled_image(sampled_type, None, image, sampler)
                    .map_err(build_error)?;
                let coordinate =
                    self.texture_coordinate(function, coordinate, array_index.as_ref(), values)?;
                let mut mask = ImageOperands::NONE;
                let mut extra = Vec::new();
                if let Some(lod) = lod {
                    mask |= ImageOperands::LOD;
                    extra.push(SpirvOperand::IdRef(self.operand(lod, values)?));
                }
                if let Some(gradient) = gradient.as_deref() {
                    // SPIR-V's Grad takes the derivatives of the coordinate
                    // without its array layer, which is the shape AIR already
                    // passes them in.
                    mask |= ImageOperands::GRAD;
                    extra.push(SpirvOperand::IdRef(self.operand(&gradient.0, values)?));
                    extra.push(SpirvOperand::IdRef(self.operand(&gradient.1, values)?));
                }
                if let Some(offset) = offset {
                    // Metal requires a sampling offset to be a compile-time
                    // constant, which is also the only form Vulkan accepts
                    // outside a gather.
                    if !matches!(offset, Operand::Composite { .. } | Operand::Integer { .. }) {
                        return Err(emission(
                            "a sampling offset must be a constant to be emitted",
                        ));
                    }
                    mask |= if gather.is_some() {
                        self.capability(Capability::ImageGatherExtended)?;
                        ImageOperands::OFFSET
                    } else {
                        ImageOperands::CONST_OFFSET
                    };
                    extra.push(SpirvOperand::IdRef(self.operand(offset, values)?));
                }
                match gather {
                    Some(component) => {
                        let component = self.constant(&Type::u32(), u64::from(*component))?;
                        self.builder
                            .image_gather(
                                ty,
                                result_id,
                                sampled,
                                coordinate,
                                component,
                                Some(mask),
                                extra,
                            )
                            .map_err(build_error)?;
                    }
                    // An implicit level of detail needs the fragment stage's
                    // derivatives; AIR requests an explicit level otherwise.
                    None if lod.is_some() || gradient.is_some() => {
                        self.builder
                            .image_sample_explicit_lod(
                                ty, result_id, sampled, coordinate, mask, extra,
                            )
                            .map_err(build_error)?;
                    }
                    None => {
                        self.builder
                            .image_sample_implicit_lod(
                                ty,
                                result_id,
                                sampled,
                                coordinate,
                                Some(mask),
                                extra,
                            )
                            .map_err(build_error)?;
                    }
                }
            }
            InstructionKind::TextureRead {
                texture,
                coordinate,
                array_index,
                lod,
                sample,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("texture read has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let Some(Type::Texture(image_type)) = operand_type(function, texture) else {
                    return Err(emission("texture read has no texture operand"));
                };
                let image = self.operand(texture, values)?;
                let coordinate =
                    self.texture_coordinate(function, coordinate, array_index.as_ref(), values)?;
                let mut mask = ImageOperands::NONE;
                let mut extra = Vec::new();
                if let Some(lod) = lod {
                    mask |= ImageOperands::LOD;
                    extra.push(SpirvOperand::IdRef(self.operand(lod, values)?));
                }
                if let Some(sample) = sample {
                    mask |= ImageOperands::SAMPLE;
                    extra.push(SpirvOperand::IdRef(self.operand(sample, values)?));
                }
                // A sampled image is fetched; a storage image is read.
                if image_type.sampled_image() {
                    self.builder
                        .image_fetch(ty, result_id, image, coordinate, Some(mask), extra)
                        .map_err(build_error)?;
                } else {
                    self.builder
                        .image_read(ty, result_id, image, coordinate, Some(mask), extra)
                        .map_err(build_error)?;
                }
            }
            InstructionKind::TextureQuerySize {
                texture,
                lod,
                component,
            } => {
                let result = instruction
                    .result
                    .ok_or_else(|| emission("texture query has no result"))?;
                let ty = self.type_id(&function.values[result.0 as usize])?;
                let Some(Type::Texture(image_type)) = operand_type(function, texture) else {
                    return Err(emission("texture query has no texture operand"));
                };
                self.capability(Capability::ImageQuery)?;
                let lanes = coordinate_lanes(&image_type) + u8::from(image_type.array);
                let size_type = self.type_id(&if lanes == 1 {
                    Type::u32()
                } else {
                    Type::Vector {
                        element: Box::new(Type::u32()),
                        lanes,
                    }
                })?;
                let image = self.operand(texture, values)?;
                // `OpImageQuerySize` covers a storage image and a multisampled
                // one; every other sampled image needs the level named, and
                // Metal's `get_width()` on a texture it only reads names none.
                let level = match lod {
                    Some(lod) => Some(self.operand(lod, values)?),
                    None if image_type.sampled_image() && !image_type.multisampled => {
                        Some(self.constant(&Type::u32(), 0)?)
                    }
                    None => None,
                };
                let size = match level {
                    Some(lod) => self
                        .builder
                        .image_query_size_lod(size_type, None, image, lod)
                        .map_err(build_error)?,
                    None => self
                        .builder
                        .image_query_size(size_type, None, image)
                        .map_err(build_error)?,
                };
                if lanes == 1 {
                    self.builder
                        .copy_object(ty, result_id, size)
                        .map_err(build_error)?;
                } else {
                    self.builder
                        .composite_extract(ty, result_id, size, [*component])
                        .map_err(build_error)?;
                }
            }
            _ => unreachable!("not a texture instruction"),
        }
        Ok(())
    }
}
