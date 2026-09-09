//! LLVM values share a namespace: module globals/constants, arguments, local
//! constants, then instruction results. Constant aggregate references are
//! absolute (including forward references); instruction operands may be relative.

use super::*;

#[derive(Clone, Debug, PartialEq)]
enum Constant {
    Null,
    Undef,
    Integer(u64),
    Float(u64),
    Aggregate(Vec<usize>),
    Data(Vec<u64>),
    /// A pointer to one of the module's global variables.
    Global(u32),
}

#[derive(Clone, Debug)]
struct Value {
    ty: usize,
    constant: Option<Constant>,
}

pub(super) struct ValueTable {
    relative: bool,
    values: Vec<Value>,
    argument_start: usize,
    /// One entry per module global, in declaration order, which is the order
    /// `Constant::Global` indexes and the order the IR module keeps them in.
    globals: Vec<PendingGlobal>,
}

/// A decoded `switch`, before it has been expanded into comparisons.
struct PendingSwitch {
    block: usize,
    condition: Operand,
    ty: Type,
    default: BlockId,
    cases: Vec<(u64, BlockId)>,
}

/// Replace one `switch` with a chain of equality tests.
///
/// SPIR-V has `OpSwitch`, but structured control flow would then have to
/// recover a merge for an n-way branch. A chain reduces the switch to the
/// two-way form the rest of the translator already restructures, at the cost
/// of one comparison per case.
fn expand_switch(function: &mut Function, blocks: &mut Vec<Block>, switch: PendingSwitch) {
    let PendingSwitch {
        block,
        condition,
        ty,
        default,
        cases,
    } = switch;
    let origin = BlockId(block as u32);
    if cases.is_empty() {
        blocks[block].terminator = Terminator::Branch(default);
        return;
    }
    // The first test stays in the switch's own block; each later one needs a
    // block of its own to be reached from the test before it.
    let mut chain = vec![origin];
    for position in 1..cases.len() {
        let id = BlockId(blocks.len() as u32);
        blocks.push(Block {
            id,
            name: format!("switch{block}_case{position}"),
            instructions: Vec::new(),
            terminator: Terminator::Unreachable,
        });
        chain.push(id);
    }
    let mut arrivals: Vec<(BlockId, Vec<BlockId>)> = Vec::new();
    let mut record_arrival = |target: BlockId, from: BlockId| match arrivals
        .iter_mut()
        .find(|(block, _)| *block == target)
    {
        Some((_, sources)) => sources.push(from),
        None => arrivals.push((target, vec![from])),
    };
    for (position, (value, target)) in cases.iter().enumerate() {
        let result = ValueId(function.values.len() as u32);
        function.values.push(Type::Bool);
        let here = chain[position];
        let next = chain.get(position + 1).copied().unwrap_or(default);
        let owner = &mut blocks[here.0 as usize];
        owner.instructions.push(Instruction {
            result: Some(result),
            kind: InstructionKind::IntegerCompare {
                predicate: IntegerPredicate::Equal,
                lhs: condition.clone(),
                rhs: Operand::Integer {
                    ty: ty.clone(),
                    value: *value,
                },
            },
        });
        owner.terminator = Terminator::CondBranch {
            condition: Operand::Value(result),
            true_block: *target,
            false_block: next,
        };
        record_arrival(*target, here);
    }
    record_arrival(default, *chain.last().expect("at least one case"));

    // Every edge which left the switch block now leaves the chain block which
    // tests that case, so the phis in each target follow their edge.
    for (target, sources) in arrivals {
        for instruction in &mut blocks[target.0 as usize].instructions {
            let InstructionKind::Phi { incomings } = &mut instruction.kind else {
                continue;
            };
            let Some(position) = incomings
                .iter()
                .position(|(predecessor, _)| *predecessor == origin)
            else {
                continue;
            };
            let (_, operand) = incomings.remove(position);
            for source in &sources {
                incomings.push((*source, operand.clone()));
            }
        }
    }
}

/// A decoded `GLOBALVAR` record, before its initializer has been materialized.
struct PendingGlobal {
    space: AddressSpace,
    /// The type index of what the variable holds, not of the pointer to it.
    pointee: usize,
    /// The value id of the initializer, if the record named one.
    initializer: Option<usize>,
}

impl ValueTable {
    /// Allocate only the module-level values: functions and module
    /// constants. AIR's interface metadata refers to exactly these, so it can
    /// be decoded before an entry function has been selected.
    pub(super) fn module_values(
        module: &BitcodeBlock,
        types: &[DecodedType],
    ) -> Result<Self, TranslateError> {
        Self::decode_inner(module, None, types, &[])
    }

    pub(super) fn decode(
        module: &BitcodeBlock,
        body: &BitcodeBlock,
        types: &[DecodedType],
        parameters: &[usize],
    ) -> Result<Self, TranslateError> {
        Self::decode_inner(module, Some(body), types, parameters)
    }

    fn decode_inner(
        module: &BitcodeBlock,
        body: Option<&BitcodeBlock>,
        types: &[DecodedType],
        parameters: &[usize],
    ) -> Result<Self, TranslateError> {
        let version = module
            .elements
            .iter()
            .filter_map(BitcodeElement::as_record)
            .find(|r| r.id == 1)
            .map(record_values)
            .ok_or_else(|| semantic("LLVM module has no version"))?;
        let [version @ 0..=2] = version.as_slice() else {
            return Err(semantic("unsupported LLVM module version"));
        };
        let mut table = Self {
            relative: *version >= 1,
            values: Vec::new(),
            argument_start: 0,
            globals: Vec::new(),
        };
        for record in module.elements.iter().filter_map(BitcodeElement::as_record) {
            match record.id {
                8 => {
                    let fields = record_values(record);
                    let ty =
                        type_reference(fields.get(if *version == 2 { 2 } else { 0 }), types.len())?;
                    table.values.push(Value { ty, constant: None });
                }
                // A module global: Metal's constant lookup tables and its
                // `threadgroup` arrays live here.
                7 => {
                    let fields = record_values(record);
                    let index = if *version == 2 { 2 } else { 0 };
                    let pointer = type_reference(fields.get(index), types.len())?;
                    let flags = fields.get(index + 1).copied().unwrap_or(0);
                    // Bit 1 says the record names the value type and its own
                    // address space; without it the type field is a pointer.
                    if flags & 2 != 0 {
                        return Err(semantic(
                            "LLVM global variable with an explicit value type needs decoding",
                        ));
                    }
                    let DecodedType::Pointer {
                        pointee,
                        address_space,
                    } = types[pointer]
                    else {
                        return Err(semantic("LLVM global variable is not a pointer"));
                    };
                    let space = address_space_of(address_space)?;
                    let initializer = match fields.get(index + 2).copied().unwrap_or(0) {
                        0 => None,
                        id => Some(usize::try_from(id - 1).map_err(|_| {
                            semantic("LLVM global initializer does not fit in memory")
                        })?),
                    };
                    table.values.push(Value {
                        ty: pointer,
                        constant: Some(Constant::Global(table.globals.len() as u32)),
                    });
                    table.globals.push(PendingGlobal {
                        space,
                        pointee,
                        initializer,
                    });
                }
                // These also allocate value IDs. Reject until their pointer
                // types and initializers can be represented, never skip them.
                9 | 14 | 18 => {
                    return Err(semantic(format!(
                        "unsupported LLVM module value record {}",
                        record.id
                    )));
                }
                _ => {}
            }
        }
        for block in module
            .elements
            .iter()
            .filter_map(BitcodeElement::as_block)
            .filter(|b| b.id == 11)
        {
            table.constants(block, types)?;
        }
        table.argument_start = table.values.len();
        for &ty in parameters {
            if ty >= types.len() {
                return Err(semantic("LLVM argument has missing type"));
            }
            table.values.push(Value { ty, constant: None });
        }
        for block in body
            .into_iter()
            .flat_map(|body| body.elements.iter())
            .filter_map(BitcodeElement::as_block)
            .filter(|b| b.id == 11)
        {
            table.constants(block, types)?;
        }
        Ok(table)
    }

    fn constants(
        &mut self,
        block: &BitcodeBlock,
        types: &[DecodedType],
    ) -> Result<(), TranslateError> {
        let start = self.values.len();
        let mut current_type = None;
        for record in block.elements.iter().filter_map(BitcodeElement::as_record) {
            let fields = record_values(record);
            if record.id == 1 {
                if fields.len() != 1 {
                    return Err(semantic("malformed LLVM constant SETTYPE"));
                }
                current_type = Some(type_reference(fields.first(), types.len())?);
                continue;
            }
            let ty = current_type.ok_or_else(|| semantic("LLVM constant precedes SETTYPE"))?;
            let constant = decode_constant(record.id, &fields, &types[ty])?;
            self.values.push(Value {
                ty,
                constant: Some(constant),
            });
        }
        // Constants can refer to later constants in the same block. Validate
        // after allocating every ID, without recursively expanding the graph.
        for value in &self.values[start..] {
            if let Some(Constant::Aggregate(ids)) = &value.constant {
                let expected = aggregate_types(&types[value.ty])?;
                if ids.len() != expected.len() {
                    return Err(semantic(
                        "LLVM aggregate constant has incorrect element count",
                    ));
                }
                for (&id, ty) in ids.iter().zip(expected) {
                    let member = self.values.get(id).ok_or_else(|| {
                        semantic("LLVM aggregate constant refers to a missing value")
                    })?;
                    if member.ty != ty || member.constant.is_none() {
                        return Err(semantic(
                            "LLVM aggregate constant has an invalid element type or nonconstant value",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn operand_id(&self, encoded: u64) -> Result<usize, TranslateError> {
        let id = self.reference(encoded)?;
        if id >= self.values.len() {
            return Err(semantic(format!(
                "LLVM value reference resolves to undefined value {id} at {}",
                self.values.len()
            )));
        }
        Ok(id)
    }

    /// Resolve a value reference without requiring it to be defined yet.
    ///
    /// LLVM lays a function's blocks out in an order which need not follow
    /// dominance, so an operand can name an instruction which appears later
    /// in the record stream. Those references carry their type inline.
    fn reference(&self, encoded: u64) -> Result<usize, TranslateError> {
        let encoded =
            u32::try_from(encoded).map_err(|_| semantic("LLVM value reference exceeds 32 bits"))?;
        let next = u32::try_from(self.values.len())
            .map_err(|_| semantic("LLVM value table exceeds 32 bits"))?;
        Ok(if self.relative {
            next.wrapping_sub(encoded)
        } else {
            encoded
        } as usize)
    }

    /// Lower one LLVM function body to a Metal IR function.
    ///
    /// `opaque_types` gives Metal types for arguments whose LLVM type is
    /// opaque, keyed by argument position: AIR spells textures and samplers
    /// only in entry-point metadata.
    pub(super) fn lower_body(
        mut self,
        body: &BitcodeBlock,
        types: &[DecodedType],
        signature: &Signature<'_>,
    ) -> Result<Function, TranslateError> {
        let &Signature {
            names,
            ref name,
            parameter_types,
            result_type,
            opaque_types,
        } = signature;
        let name = name.clone();
        let records: Vec<_> = body
            .elements
            .iter()
            .filter_map(BitcodeElement::as_record)
            .collect();
        let [declaration, rest @ ..] = records.as_slice() else {
            return Err(semantic("LLVM function body is empty"));
        };
        if declaration.id != 1 || record_values(declaration).len() != 1 {
            return Err(semantic(
                "LLVM function must begin with one DECLAREBLOCKS record",
            ));
        }
        let block_count = usize::try_from(record_values(declaration)[0])
            .map_err(|_| semantic("LLVM block count does not fit in memory"))?;
        if block_count == 0 {
            return Err(semantic("LLVM function declares no blocks"));
        }
        // LLVM stores a body as one flat record list; a terminator ends the
        // current basic block. Split it before lowering so value numbering
        // and block membership are decided separately.
        let mut block_records: Vec<Vec<&llvm_bitcode::bitcode::Record>> =
            vec![Vec::new(); block_count];
        let mut index = 0;
        for record in rest {
            if record.id == 1 {
                return Err(semantic("LLVM body has more than one DECLAREBLOCKS record"));
            }
            // Debug-location records annotate the preceding instruction.
            if record.id == 33 || record.id == 35 {
                continue;
            }
            let block = block_records
                .get_mut(index)
                .ok_or_else(|| semantic("LLVM body has more blocks than it declares"))?;
            block.push(record);
            if is_terminator(record.id) {
                index += 1;
            }
        }
        if index != block_count {
            return Err(semantic(
                "LLVM body's terminators do not match its declared block count",
            ));
        }
        let mut function = Function {
            name,
            result: ir_type(result_type, types)?,
            parameters: Vec::new(),
            values: Vec::new(),
            blocks: Vec::new(),
        };
        let mut operands = HashMap::new();
        for (index, &ty) in parameter_types.iter().enumerate() {
            // An opaque LLVM argument is a Metal texture or sampler; its
            // shape is only in metadata. Any other argument keeps its LLVM
            // type, so a metadata mismatch is reported rather than obeyed.
            let opaque = match &types[ty] {
                DecodedType::Opaque(_) => true,
                DecodedType::Pointer { pointee, .. } => {
                    matches!(types[*pointee], DecodedType::Opaque(_))
                }
                _ => false,
            };
            let ty = match (opaque, opaque_types.get(&(index as u32))) {
                (true, Some(ty)) => ty.clone(),
                (true, None) => {
                    return Err(semantic(
                        "AIR opaque argument has no interface metadata type",
                    ));
                }
                (false, _) => ir_type(ty, types)?,
            };
            let value = ValueId(function.values.len() as u32);
            function.values.push(ty.clone());
            function.parameters.push(Parameter {
                value,
                name: format!("arg{index}"),
                ty,
            });
            operands.insert(self.argument_start + index, Operand::Value(value));
        }
        // Composite constants need instructions. They are emitted at the head
        // of the entry block so every later use is dominated by them.
        let mut constants = Vec::new();
        let mut blocks: Vec<Block> = Vec::new();
        let mut phi_fixups: Vec<PhiFixup> = Vec::new();
        let mut switches: Vec<PendingSwitch> = Vec::new();
        // Pointers produced by a widening bitcast keep the pointee type they
        // really address; a load through one is adapted to the requested type.
        let mut aliases: HashMap<usize, PointerAlias> = HashMap::new();
        // Metal IR values reserved for LLVM instructions which are used
        // before the record stream defines them.
        let mut forward: HashMap<usize, (ValueId, usize)> = HashMap::new();
        let first_result = self.values.len();
        for (block_index, block) in block_records.iter().enumerate() {
            let mut instructions = Vec::new();
            let mut terminator = None;
            for record in block {
                let fields = record_values(record);
                if terminator.is_some() {
                    return Err(semantic("LLVM instruction after terminator"));
                }
                // `declared` is the type a record repeats for an operand it
                // names before the record stream defines it: LLVM lays a
                // function's blocks out in an order which need not follow
                // dominance. Reserve a Metal IR value for such an operand and
                // fill it in when the definition arrives.
                let mut operand = |encoded: u64,
                                   declared: Option<&u64>|
                 -> Result<(usize, Operand), TranslateError> {
                    let id = self.reference(encoded)?;
                    if id < self.values.len() {
                        let op = self.materialize(id, types, &mut operands, &mut Vec::new())?;
                        return Ok((self.values[id].ty, op));
                    }
                    let Some(declared) = declared else {
                        // Naming the record is what tells a reader whether the
                        // decoder read the wrong field or the shader really
                        // names a value nothing defines.
                        return Err(semantic(format!(
                            "LLVM value reference resolves to undefined value {id} at {}, in \
                             body record {} with operands {fields:?}",
                            self.values.len(),
                            record.id
                        )));
                    };
                    let ty = type_reference(Some(declared), types.len())?;
                    let value = match forward.get(&id) {
                        Some((value, _)) => *value,
                        None => {
                            let value = ValueId(function.values.len() as u32);
                            function.values.push(ir_type(ty, types)?);
                            forward.insert(id, (value, ty));
                            value
                        }
                    };
                    Ok((ty, Operand::Value(value)))
                };
                let (ty, kind) = match (record.id, fields.as_slice()) {
                    (6, [vector, index]) => {
                        let (ty, composite) = operand(*vector, None)?;
                        let DecodedType::Vector { element, lanes } = types[ty] else {
                            return Err(semantic("extractelement requires vector"));
                        };
                        // Metal's `v[i]` need not name a constant lane.
                        let index_id = self.operand_id(*index)?;
                        match self.integer(index_id, types) {
                            Ok(lane) if lane < u64::from(lanes) => (
                                element,
                                InstructionKind::CompositeExtract {
                                    composite,
                                    index: lane as u32,
                                },
                            ),
                            Ok(_) => {
                                return Err(semantic("extractelement lane is out of range"));
                            }
                            Err(error) => {
                                // Only a lane whose value is not yet known is
                                // a dynamic index; one of the wrong type is
                                // still the malformed record it was.
                                if !matches!(
                                    self.values.get(index_id).map(|value| &types[value.ty]),
                                    Some(DecodedType::Integer(_))
                                ) {
                                    return Err(error);
                                }
                                let (_, index) = operand(*index, None)?;
                                (
                                    element,
                                    InstructionKind::VectorExtractDynamic {
                                        vector: composite,
                                        index,
                                    },
                                )
                            }
                        }
                    }
                    (7, [vector, object, index]) => {
                        let (ty, composite) = operand(*vector, None)?;
                        let DecodedType::Vector { element, lanes } = types[ty] else {
                            return Err(semantic("insertelement requires vector"));
                        };
                        // LLVM writes no type for the inserted element, even
                        // when it names a value the record stream has not
                        // reached: the vector's element type is it.
                        let (object_ty, object) = operand(*object, Some(&(element as u64)))?;
                        if object_ty != element {
                            return Err(semantic("insertelement element type mismatch"));
                        }
                        let index_id = self.operand_id(*index)?;
                        match self.integer(index_id, types) {
                            Ok(lane) if lane < u64::from(lanes) => (
                                ty,
                                InstructionKind::CompositeInsert {
                                    composite,
                                    object,
                                    index: lane as u32,
                                },
                            ),
                            Ok(_) => {
                                return Err(semantic("insertelement lane is out of range"));
                            }
                            Err(error) => {
                                // Only a lane whose value is not yet known is
                                // a dynamic index; one of the wrong type is
                                // still the malformed record it was.
                                if !matches!(
                                    self.values.get(index_id).map(|value| &types[value.ty]),
                                    Some(DecodedType::Integer(_))
                                ) {
                                    return Err(error);
                                }
                                let (_, index) = operand(*index, None)?;
                                (
                                    ty,
                                    InstructionKind::VectorInsertDynamic {
                                        vector: composite,
                                        object,
                                        index,
                                    },
                                )
                            }
                        }
                    }
                    (8, [first, second, mask] | [first, _, second, mask]) => {
                        let (ty, first) = match fields.as_slice() {
                            [_, declared, _, _] => operand(*first, Some(declared))?,
                            _ => operand(*first, None)?,
                        };
                        let (second_ty, second) = operand(*second, None)?;
                        let DecodedType::Vector { element, lanes } = types[ty] else {
                            return Err(semantic("shufflevector requires vectors"));
                        };
                        if ty != second_ty {
                            return Err(semantic("shufflevector input type mismatch"));
                        }
                        let mask = self.operand_id(*mask)?;
                        let components = self.shuffle_mask(mask, types)?;
                        if components
                            .iter()
                            .any(|&index| index != u32::MAX && index >= lanes * 2)
                        {
                            return Err(semantic("shufflevector mask is out of range"));
                        }
                        let result = types.iter().position(|ty| matches!(ty, DecodedType::Vector { element: e, lanes: n } if *e == element && *n as usize == components.len())).ok_or_else(|| semantic("shufflevector result type missing"))?;
                        (
                            result,
                            InstructionKind::VectorShuffle {
                                first,
                                second,
                                components,
                            },
                        )
                    }
                    (3, [source_value, target, opcode]) => {
                        let source_id = self.operand_id(*source_value)?;
                        let (source, value) = operand(*source_value, None)?;
                        let target = type_reference(Some(target), types.len())?;
                        // A cast to `i8*` names raw bytes, which SPIR-V's
                        // logical addressing cannot address. Apple's compiler
                        // emits one only to hand a local to a lifetime
                        // marker, so the cast carries the pointer unchanged
                        // and the marker itself is dropped.
                        if *opcode == 11
                            && let DecodedType::Pointer { pointee, .. } = types[target]
                            && matches!(types[pointee], DecodedType::Integer(8))
                        {
                            let id = self.values.len();
                            define(
                                &mut forward,
                                id,
                                value,
                                &mut operands,
                                &function,
                                &mut instructions,
                            )?;
                            self.values.push(Value {
                                ty: source,
                                constant: None,
                            });
                            continue;
                        }
                        // A `thread` array decays to a pointer to its first
                        // element. SPIR-V spells that as an access chain, not
                        // as a cast between pointer types.
                        if *opcode == 11
                            && let (
                                DecodedType::Pointer {
                                    pointee: from,
                                    address_space: 0,
                                },
                                DecodedType::Pointer {
                                    pointee: to,
                                    address_space: 0,
                                },
                            ) = (&types[source], &types[target])
                            && let DecodedType::Array { element, .. } = types[*from]
                            && element == *to
                        {
                            (
                                target,
                                InstructionKind::BufferAddress {
                                    pointer: value,
                                    // The leading index steps the pointer, as
                                    // in any LLVM `getelementptr`.
                                    indices: vec![Operand::u32(0), Operand::u32(0)],
                                },
                            )
                        } else {
                            // Apple's compiler reads a `float3` struct member
                            // with a `float4` load, because Metal pads a `float3`
                            // to sixteen bytes. Record the pointee the pointer
                            // really addresses and adapt at the load rather than
                            // retyping the pointer.
                            if let (
                                DecodedType::Pointer {
                                    pointee: from,
                                    address_space,
                                },
                                DecodedType::Pointer {
                                    pointee: to,
                                    address_space: target_space,
                                },
                            ) = (&types[source], &types[target])
                                && *opcode == 11
                                && address_space == target_space
                            {
                                let actual =
                                    aliases.get(&source_id).map_or(*from, |alias| alias.pointee);
                                // A pointer to an array read as a pointer to
                                // something the shape of its element addresses
                                // element zero. Step into it, so that the load
                                // reads one element rather than the whole
                                // array, and adapt that element at the load.
                                let (actual, address) = match types[actual] {
                                    DecodedType::Array { element, .. }
                                        if reinterpretation(types, element, *to).is_ok() =>
                                    {
                                        let pointer = Type::Pointer {
                                            space: address_space_of(*address_space)?,
                                            pointee: Box::new(ir_type(element, types)?),
                                        };
                                        let stepped = emit(
                                            &mut function,
                                            &mut instructions,
                                            pointer,
                                            InstructionKind::BufferAddress {
                                                pointer: value.clone(),
                                                indices: vec![Operand::u32(0), Operand::u32(0)],
                                            },
                                        );
                                        (element, stepped)
                                    }
                                    _ => (actual, value.clone()),
                                };
                                reinterpretation(types, actual, *to)?;
                                let id = self.values.len();
                                aliases.insert(
                                    id,
                                    PointerAlias {
                                        pointee: actual,
                                        operand: address,
                                    },
                                );
                                define(
                                    &mut forward,
                                    id,
                                    value,
                                    &mut operands,
                                    &function,
                                    &mut instructions,
                                )?;
                                self.values.push(Value {
                                    ty: target,
                                    constant: None,
                                });
                                continue;
                            }
                            let op = cast_operation(*opcode, source, target, types)?;
                            (target, InstructionKind::Convert { op, value })
                        }
                    }
                    // ALLOCA: [allocated type, size type, size, packed flags].
                    // Metal's `thread` locals arrive here; anything but one
                    // element is a variable-length allocation, which SPIR-V's
                    // logical addressing cannot express.
                    (19, [allocated, size_type, size, packed]) => {
                        let allocated = type_reference(Some(allocated), types.len())?;
                        // Bit 6 of the packed field says the record names the
                        // allocated type rather than a pointer to it.
                        let (pointer, pointee) = if packed & (1 << 6) == 0 {
                            match types[allocated] {
                                DecodedType::Pointer {
                                    pointee,
                                    address_space: 0,
                                } => (allocated, pointee),
                                _ => {
                                    return Err(semantic(
                                        "LLVM alloca names a type which is not a thread pointer",
                                    ));
                                }
                            }
                        } else {
                            let pointer = types
                                .iter()
                                .position(|ty| {
                                    matches!(
                                        ty,
                                        DecodedType::Pointer {
                                            pointee,
                                            address_space: 0,
                                        } if *pointee == allocated
                                    )
                                })
                                .ok_or_else(|| {
                                    semantic("LLVM alloca has no thread pointer result type")
                                })?;
                            (pointer, allocated)
                        };
                        let _ = type_reference(Some(size_type), types.len())?;
                        if self.integer(
                            usize::try_from(*size)
                                .map_err(|_| semantic("LLVM alloca size does not fit in memory"))?,
                            types,
                        )? != 1
                        {
                            return Err(semantic(
                                "LLVM alloca of more than one element needs lowering",
                            ));
                        }
                        (
                            pointer,
                            InstructionKind::LocalVariable {
                                ty: ir_type(pointee, types)?,
                            },
                        )
                    }
                    (30, [pointer, indices @ ..]) if !indices.is_empty() => {
                        let (pointer_ty, pointer) = operand(*pointer, None)?;
                        let DecodedType::Pointer {
                            mut pointee,
                            address_space,
                        } = types[pointer_ty]
                        else {
                            return Err(semantic("LLVM GEP requires pointer"));
                        };
                        let mut decoded = Vec::new();
                        for (position, &index) in indices.iter().enumerate() {
                            let (index_ty, value) = operand(index, None)?;
                            if !matches!(types[index_ty], DecodedType::Integer(_)) {
                                return Err(semantic("LLVM GEP index is not integer"));
                            }
                            if position > 0 {
                                pointee = match &types[pointee] {
                                    DecodedType::Struct { fields, .. } => {
                                        let field = self.integer(self.operand_id(index)?, types)?;
                                        *fields.get(field as usize).ok_or_else(|| {
                                            semantic("LLVM GEP field out of bounds")
                                        })?
                                    }
                                    DecodedType::Array { element, .. }
                                    | DecodedType::Vector { element, .. } => *element,
                                    _ => return Err(semantic("LLVM GEP indexes a scalar")),
                                };
                            }
                            decoded.push(value);
                        }
                        let target = types.iter().position(|ty| matches!(ty, DecodedType::Pointer { pointee: p, address_space: a } if *p == pointee && *a == address_space)).ok_or_else(|| semantic("LLVM GEP result pointer type is missing"))?;
                        (
                            target,
                            InstructionKind::BufferAddress {
                                pointer,
                                indices: decoded,
                            },
                        )
                    }
                    // STORE, in both the old form which implies the stored
                    // value's type and the newer one which repeats it.
                    (24 | 44, [pointer, value, alignment, volatile])
                        if *volatile == 0 && *alignment <= 32 =>
                    {
                        let pointer_id = self.operand_id(*pointer)?;
                        let alias = aliases
                            .get(&pointer_id)
                            .map(|alias| (alias.pointee, alias.operand.clone()));
                        let (pointee, pointer) = match alias {
                            Some((pointee, operand)) => (pointee, operand),
                            None => {
                                let (pointer_ty, pointer) = operand(*pointer, None)?;
                                let DecodedType::Pointer { pointee, .. } = types[pointer_ty] else {
                                    return Err(semantic("LLVM store requires a pointer"));
                                };
                                (pointee, pointer)
                            }
                        };
                        let (value_ty, value) = operand(*value, None)?;
                        // A store through a reinterpreted pointer is the
                        // mirror of the load: the value the guest wrote is
                        // adapted to what the address really holds, rather
                        // than the pointer being retyped.
                        let value = narrow(
                            value,
                            value_ty,
                            pointee,
                            types,
                            &mut function,
                            &mut instructions,
                        )?;
                        instructions.push(Instruction {
                            result: None,
                            kind: InstructionKind::Store { pointer, value },
                        });
                        continue;
                    }
                    (20, [pointer, alignment, volatile]) if *volatile == 0 && *alignment <= 32 => {
                        let pointer_id = self.operand_id(*pointer)?;
                        let (ty, pointer) = operand(*pointer, None)?;
                        let DecodedType::Pointer { pointee, .. } = types[ty] else {
                            return Err(semantic("LLVM load requires pointer"));
                        };
                        let Some(alias) = aliases.get(&pointer_id) else {
                            let value = emit(
                                &mut function,
                                &mut instructions,
                                ir_type(pointee, types)?,
                                InstructionKind::Load { pointer },
                            );
                            define(
                                &mut forward,
                                self.values.len(),
                                value,
                                &mut operands,
                                &function,
                                &mut instructions,
                            )?;
                            self.values.push(Value {
                                ty: pointee,
                                constant: None,
                            });
                            continue;
                        };
                        let loaded = emit(
                            &mut function,
                            &mut instructions,
                            ir_type(alias.pointee, types)?,
                            InstructionKind::Load {
                                pointer: alias.operand.clone(),
                            },
                        );
                        let value = widen(
                            loaded,
                            alias.pointee,
                            pointee,
                            types,
                            &mut function,
                            &mut instructions,
                        )?;
                        define(
                            &mut forward,
                            self.values.len(),
                            value,
                            &mut operands,
                            &function,
                            &mut instructions,
                        )?;
                        self.values.push(Value {
                            ty: pointee,
                            constant: None,
                        });
                        continue;
                    }
                    (2, [lhs, rhs, opcode] | [lhs, rhs, opcode, _] | [lhs, _, rhs, opcode, _]) => {
                        // A four-field record is ambiguous: it is either an
                        // operation with fast-math flags, or one whose first
                        // operand is not defined yet and so repeats its type.
                        // Only the value table can tell them apart. A
                        // five-field record is both at once and needs no
                        // guessing.
                        let forward_form = matches!(
                            fields.as_slice(),
                            [first, ..] if self.reference(*first)? >= self.values.len()
                        );
                        let (lhs, second, opcode) = match (fields.as_slice(), forward_form) {
                            ([first, declared, second, code], true)
                            | ([first, declared, second, code, _], _) => {
                                (operand(*first, Some(declared))?, second, code)
                            }
                            _ => (operand(*lhs, None)?, rhs, opcode),
                        };
                        let (ty, lhs) = lhs;
                        // LLVM writes no type for the second operand of a
                        // binary operation, even when it names a value the
                        // record stream has not reached: both operands of one
                        // have the same type, so the first one's is it.
                        let (rhs_ty, rhs) = operand(*second, Some(&(ty as u64)))?;
                        if ty != rhs_ty {
                            return Err(semantic("LLVM binary operand type mismatch"));
                        }
                        let scalar = match types[ty] {
                            DecodedType::Vector { element, .. } => &types[element],
                            _ => &types[ty],
                        };
                        // LLVM shares one opcode space between integer and
                        // floating arithmetic; the operand type selects which.
                        let kind = match scalar {
                            DecodedType::Float(_) => {
                                let op = match opcode {
                                    0 => FloatBinaryOp::Add,
                                    1 => FloatBinaryOp::Subtract,
                                    2 => FloatBinaryOp::Multiply,
                                    4 => FloatBinaryOp::Divide,
                                    6 => FloatBinaryOp::Remainder,
                                    _ => {
                                        return Err(semantic(format!(
                                            "unsupported LLVM floating binary opcode {opcode}"
                                        )));
                                    }
                                };
                                InstructionKind::FloatBinary { op, lhs, rhs }
                            }
                            DecodedType::Integer(_) => {
                                let op = match opcode {
                                    0 => IntegerBinaryOp::Add,
                                    1 => IntegerBinaryOp::Subtract,
                                    2 => IntegerBinaryOp::Multiply,
                                    3 => IntegerBinaryOp::DivideUnsigned,
                                    4 => IntegerBinaryOp::DivideSigned,
                                    5 => IntegerBinaryOp::RemainderUnsigned,
                                    6 => IntegerBinaryOp::RemainderSigned,
                                    7 => IntegerBinaryOp::ShiftLeft,
                                    8 => IntegerBinaryOp::ShiftRightLogical,
                                    9 => IntegerBinaryOp::ShiftRightArithmetic,
                                    10 => IntegerBinaryOp::And,
                                    11 => IntegerBinaryOp::Or,
                                    12 => IntegerBinaryOp::Xor,
                                    _ => {
                                        return Err(semantic(format!(
                                            "unsupported LLVM integer binary opcode {opcode}"
                                        )));
                                    }
                                };
                                InstructionKind::IntegerBinary { op, lhs, rhs }
                            }
                            _ => return Err(semantic("LLVM binary operand is not numeric")),
                        };
                        (ty, kind)
                    }
                    // The call marker field carries the calling convention
                    // and the tail-call markers, none of which changes what
                    // the call computes. The two bits which do change the
                    // record's shape - an explicit function type and
                    // fast-math flags, each inserting a field before the
                    // callee - are rejected rather than misread.
                    (34, [_, cc, callee, args @ ..])
                        if cc & ((1 << CALL_EXPLICIT_TYPE) | (1 << CALL_FMF)) == 0 =>
                    {
                        let callee = self.operand_id(*callee)?;
                        let signature = match types[self.values[callee].ty] {
                            DecodedType::Pointer { pointee, .. } => pointee,
                            _ => self.values[callee].ty,
                        };
                        let DecodedType::Function { result, parameters } = &types[signature] else {
                            return Err(semantic("LLVM call target is not a function"));
                        };
                        let name = names
                            .get(&callee)
                            .ok_or_else(|| semantic("LLVM call target has no name"))?
                            .clone();
                        // `llvm.memcpy` declares byte pointers, so its
                        // arguments never match the declaration by type. Metal
                        // reaches it by assigning one whole structure, which
                        // is a load and a store of that structure.
                        if name.starts_with("llvm.memcpy.") {
                            // Older LLVM spells an alignment between the
                            // length and the volatile flag; both forms end
                            // with markers this lowering does not need.
                            let [destination, source, length, ..] = args else {
                                return Err(semantic("llvm.memcpy has unexpected operands"));
                            };
                            let mut addressed = |encoded: u64,
                                                 id: usize|
                             -> Result<(usize, Operand), TranslateError> {
                                if let Some(alias) = aliases.get(&id) {
                                    return Ok((alias.pointee, alias.operand.clone()));
                                }
                                let (ty, value) = operand(encoded, None)?;
                                match types[ty] {
                                    DecodedType::Pointer { pointee, .. } => Ok((pointee, value)),
                                    _ => Err(semantic("llvm.memcpy operand is not a pointer")),
                                }
                            };
                            let destination_id = self.operand_id(*destination)?;
                            let source_id = self.operand_id(*source)?;
                            let (into, destination) = addressed(*destination, destination_id)?;
                            let (from, source) = addressed(*source, source_id)?;
                            if into != from {
                                return Err(semantic(
                                    "llvm.memcpy copies between two different types",
                                ));
                            }
                            let copied = ir_type(into, types)?;
                            let (size, _) = layout(&copied)?;
                            let bytes = self.integer(self.operand_id(*length)?, types)?;
                            if bytes != u64::from(size) {
                                return Err(semantic(
                                    "llvm.memcpy length is not one whole value of the copied type",
                                ));
                            }
                            let loaded = emit(
                                &mut function,
                                &mut instructions,
                                copied,
                                InstructionKind::Load { pointer: source },
                            );
                            instructions.push(Instruction {
                                result: None,
                                kind: InstructionKind::Store {
                                    pointer: destination,
                                    value: loaded,
                                },
                            });
                            continue;
                        }
                        // Metal reaches `llvm.memset` by initialising a local
                        // array to zero, which is a store of a zero value.
                        // Any other fill pattern would have to be spelled
                        // byte by byte, which logical addressing cannot do.
                        if name.starts_with("llvm.memset.") {
                            let [destination, fill, length, ..] = args else {
                                return Err(semantic("llvm.memset has unexpected operands"));
                            };
                            let destination_id = self.operand_id(*destination)?;
                            let (into, destination) = match aliases.get(&destination_id) {
                                Some(alias) => (alias.pointee, alias.operand.clone()),
                                None => {
                                    let (ty, value) = operand(*destination, None)?;
                                    match types[ty] {
                                        DecodedType::Pointer { pointee, .. } => (pointee, value),
                                        _ => {
                                            return Err(semantic(
                                                "llvm.memset destination is not a pointer",
                                            ));
                                        }
                                    }
                                }
                            };
                            if self.integer(self.operand_id(*fill)?, types)? != 0 {
                                return Err(semantic("llvm.memset fills with a nonzero byte"));
                            }
                            let cleared = ir_type(into, types)?;
                            let (size, _) = layout(&cleared)?;
                            if self.integer(self.operand_id(*length)?, types)? != u64::from(size) {
                                return Err(semantic(
                                    "llvm.memset length is not one whole value of the cleared type",
                                ));
                            }
                            instructions.push(Instruction {
                                result: None,
                                kind: InstructionKind::Store {
                                    pointer: destination,
                                    value: zero_composite(&cleared)?,
                                },
                            });
                            continue;
                        }
                        if args.len() != parameters.len() {
                            return Err(semantic("LLVM call arity mismatch"));
                        }
                        let mut actual = Vec::new();
                        for (&arg, &expected) in args.iter().zip(parameters) {
                            let (ty, value) = operand(arg, None)?;
                            if ty != expected {
                                return Err(semantic("LLVM call argument type mismatch"));
                            }
                            actual.push(value);
                        }
                        // A lifetime marker bounds a local's live range for
                        // LLVM's own optimizers. SPIR-V has no such thing, and
                        // the variable is live for the whole function there.
                        if name.starts_with("llvm.lifetime.") {
                            continue;
                        }
                        let kind = known_helper(&name, &actual)
                            .unwrap_or_else(|| air_intrinsic(&name, &actual))?;
                        if matches!(types[*result], DecodedType::Void) {
                            instructions.push(Instruction { result: None, kind });
                            continue;
                        }
                        (*result, kind)
                    }
                    (9 | 28, [lhs, rhs, predicate] | [lhs, _, rhs, predicate]) => {
                        let (ty, lhs) = match fields.as_slice() {
                            [_, declared, _, _] => operand(*lhs, Some(declared))?,
                            _ => operand(*lhs, None)?,
                        };
                        let (rhs_ty, rhs) = operand(*rhs, None)?;
                        if ty != rhs_ty {
                            return Err(semantic("LLVM comparison type mismatch"));
                        }
                        let scalar = match types[ty] {
                            DecodedType::Vector { element, .. } => &types[element],
                            _ => &types[ty],
                        };
                        let kind = match scalar {
                            DecodedType::Float(_) => InstructionKind::FloatCompare {
                                predicate: float_predicate(*predicate)?,
                                lhs,
                                rhs,
                            },
                            DecodedType::Integer(_) => InstructionKind::IntegerCompare {
                                predicate: integer_predicate(*predicate)?,
                                lhs,
                                rhs,
                            },
                            _ => return Err(semantic("LLVM comparison operand is not numeric")),
                        };
                        (boolean_type(ty, types)?, kind)
                    }
                    (26, [aggregate, indices @ ..]) if !indices.is_empty() => {
                        let (mut ty, mut composite) = operand(*aggregate, None)?;
                        let mut extracted = None;
                        for &index in indices {
                            let index = u32::try_from(index)
                                .map_err(|_| semantic("LLVM extractvalue index is too large"))?;
                            let member = member_type(&types[ty], index)?;
                            composite = emit(
                                &mut function,
                                &mut instructions,
                                ir_type(member, types)?,
                                InstructionKind::CompositeExtract { composite, index },
                            );
                            extracted = Some(composite.clone());
                            ty = member;
                        }
                        let value = extracted.expect("indices are non-empty");
                        define(
                            &mut forward,
                            self.values.len(),
                            value,
                            &mut operands,
                            &function,
                            &mut instructions,
                        )?;
                        self.values.push(Value { ty, constant: None });
                        continue;
                    }
                    (27, rest) if rest.len() >= 3 => {
                        // LLVM writes both of `insertvalue`'s values as a
                        // value-type pair, which repeats the type only for a
                        // value the record stream has not reached yet. Only
                        // the value table can say which form a record is in.
                        let aggregate_forward = self.reference(rest[0])? >= self.values.len();
                        let object_at = 1 + usize::from(aggregate_forward);
                        let object_forward = match rest.get(object_at) {
                            Some(field) => self.reference(*field)? >= self.values.len(),
                            None => return Err(semantic("LLVM insertvalue record is too short")),
                        };
                        let index_at = object_at + 1 + usize::from(object_forward);
                        let ([aggregate, aggregate_ty @ ..], [object, object_ty @ ..]) = (
                            &rest[..object_at],
                            &rest[object_at..index_at.min(rest.len())],
                        ) else {
                            return Err(semantic("LLVM insertvalue record is too short"));
                        };
                        let [index] = rest[index_at.min(rest.len())..] else {
                            return Err(semantic(
                                "LLVM insertvalue names more than one aggregate index",
                            ));
                        };
                        let (ty, composite) = operand(*aggregate, aggregate_ty.first())?;
                        let (object_ty, object) = operand(*object, object_ty.first())?;
                        let index = u32::try_from(index)
                            .map_err(|_| semantic("LLVM insertvalue index is too large"))?;
                        if member_type(&types[ty], index)? != object_ty {
                            return Err(semantic("LLVM insertvalue element type mismatch"));
                        }
                        (
                            ty,
                            InstructionKind::CompositeInsert {
                                composite,
                                object,
                                index,
                            },
                        )
                    }
                    (29, [on_true, on_false, condition]) => {
                        let (ty, on_true) = operand(*on_true, None)?;
                        let (false_ty, on_false) = operand(*on_false, None)?;
                        let (_, condition) = operand(*condition, None)?;
                        if ty != false_ty {
                            return Err(semantic("LLVM select type mismatch"));
                        }
                        (
                            ty,
                            InstructionKind::Select {
                                condition,
                                on_true,
                                on_false,
                            },
                        )
                    }
                    (10, []) => {
                        if !matches!(types[result_type], DecodedType::Void) {
                            return Err(semantic("LLVM void return from a non-void function"));
                        }
                        terminator = Some(Terminator::Return(None));
                        continue;
                    }
                    (10, [value]) => {
                        let (ty, value) = operand(*value, None)?;
                        if ty != result_type {
                            return Err(semantic("LLVM return type mismatch"));
                        }
                        terminator = Some(Terminator::Return(Some(value)));
                        continue;
                    }
                    (11, [target]) => {
                        terminator = Some(Terminator::Branch(block_id(*target, block_count)?));
                        continue;
                    }
                    (11, [true_block, false_block, condition]) => {
                        let (ty, condition) = operand(*condition, None)?;
                        if !matches!(types[ty], DecodedType::Integer(1)) {
                            return Err(semantic("LLVM conditional branch requires i1"));
                        }
                        terminator = Some(Terminator::CondBranch {
                            condition,
                            true_block: block_id(*true_block, block_count)?,
                            false_block: block_id(*false_block, block_count)?,
                        });
                        continue;
                    }
                    // SWITCH: [condition type, condition, default block,
                    // (case value, case block)...]. Metal IR has no multi-way
                    // branch, so the switch is recorded here and expanded into
                    // a chain of comparisons once every block exists and its
                    // phis can be repointed at the block which now reaches it.
                    (12, [_condition_type, condition, default, cases @ ..])
                        if cases.len().is_multiple_of(2) =>
                    {
                        let (condition_type, condition) = operand(*condition, None)?;
                        if !matches!(types[condition_type], DecodedType::Integer(_)) {
                            return Err(semantic("LLVM switch condition is not an integer"));
                        }
                        let mut decoded = Vec::new();
                        for case in cases.as_chunks::<2>().0 {
                            // A case value is an absolute value reference,
                            // unlike the relatively numbered condition.
                            let value = self.integer(
                                usize::try_from(case[0]).map_err(|_| {
                                    semantic("LLVM switch case does not fit in memory")
                                })?,
                                types,
                            )?;
                            decoded.push((value, block_id(case[1], block_count)?));
                        }
                        switches.push(PendingSwitch {
                            block: block_index,
                            condition,
                            ty: ir_type(condition_type, types)?,
                            default: block_id(*default, block_count)?,
                            cases: decoded,
                        });
                        terminator = Some(Terminator::Unreachable);
                        continue;
                    }
                    (15, []) => {
                        terminator = Some(Terminator::Unreachable);
                        continue;
                    }
                    // PHI: [result type, (value, predecessor block)...]. Its
                    // incoming values may not be defined yet, so record them and
                    // resolve after every block has been lowered.
                    (16, [ty, incoming @ ..]) if !incoming.is_empty() => {
                        if !incoming.len().is_multiple_of(2) {
                            return Err(semantic("LLVM phi has an odd operand count"));
                        }
                        let ty = type_reference(Some(ty), types.len())?;
                        let mut sources = Vec::new();
                        for pair in incoming.as_chunks::<2>().0 {
                            // Phi values use signed relative references, because
                            // an incoming value may be defined after the phi.
                            let delta = decode_sign_rotated(pair[0]);
                            let id = i64::try_from(self.values.len())
                                .map_err(|_| semantic("LLVM value table exceeds 64 bits"))?
                                - delta;
                            let id = usize::try_from(id)
                                .map_err(|_| semantic("LLVM phi refers to a negative value"))?;
                            sources.push((block_id(pair[1], block_count)?, id));
                        }
                        let value = emit(
                            &mut function,
                            &mut instructions,
                            ir_type(ty, types)?,
                            InstructionKind::Phi {
                                incomings: Vec::new(),
                            },
                        );
                        phi_fixups.push(PhiFixup {
                            block: block_index,
                            instruction: instructions.len() - 1,
                            sources,
                        });
                        define(
                            &mut forward,
                            self.values.len(),
                            value,
                            &mut operands,
                            &function,
                            &mut instructions,
                        )?;
                        self.values.push(Value { ty, constant: None });
                        continue;
                    }
                    _ => {
                        return Err(semantic(format!(
                            "LLVM body record {} with operands {fields:?} needs lowering after {} decoded instruction result(s)",
                            record.id,
                            self.values.len() - first_result
                        )));
                    }
                };
                let value = emit(&mut function, &mut instructions, ir_type(ty, types)?, kind);
                define(
                    &mut forward,
                    self.values.len(),
                    value,
                    &mut operands,
                    &function,
                    &mut instructions,
                )?;
                self.values.push(Value { ty, constant: None });
            }
            blocks.push(Block {
                id: BlockId(block_index as u32),
                name: if block_index == 0 {
                    "entry".to_string()
                } else {
                    format!("block{block_index}")
                },
                instructions,
                terminator: terminator
                    .ok_or_else(|| semantic("LLVM basic block has no terminator"))?,
            });
        }
        if !forward.is_empty() {
            return Err(semantic(
                "LLVM forward reference names a value the body never defines",
            ));
        }
        for fixup in phi_fixups {
            let mut incomings = Vec::new();
            for (predecessor, id) in fixup.sources {
                let operand = self.materialize(id, types, &mut operands, &mut Vec::new())?;
                incomings.push((predecessor, operand));
            }
            blocks[fixup.block].instructions[fixup.instruction].kind =
                InstructionKind::Phi { incomings };
        }
        // Phi instructions must precede every other instruction in a block.
        for block in &mut blocks {
            let phis = block
                .instructions
                .iter()
                .filter(|instruction| matches!(instruction.kind, InstructionKind::Phi { .. }))
                .count();
            if block.instructions[..phis]
                .iter()
                .any(|instruction| !matches!(instruction.kind, InstructionKind::Phi { .. }))
            {
                return Err(semantic(
                    "LLVM phi does not precede its block's instructions",
                ));
            }
        }
        for switch in switches {
            expand_switch(&mut function, &mut blocks, switch);
        }
        constants.append(&mut blocks[0].instructions);
        blocks[0].instructions = constants;
        function.blocks = blocks;
        Ok(function)
    }

    pub(super) fn integer(&self, id: usize, types: &[DecodedType]) -> Result<u64, TranslateError> {
        if !self
            .values
            .get(id)
            .is_some_and(|value| matches!(types[value.ty], DecodedType::Integer(_)))
        {
            return Err(semantic("LLVM index has a noninteger type"));
        }
        match self.values.get(id).and_then(|v| v.constant.as_ref()) {
            Some(Constant::Integer(value)) => Ok(*value),
            Some(Constant::Null) => Ok(0),
            _ => Err(semantic("LLVM index requires a constant integer")),
        }
    }

    fn shuffle_mask(&self, id: usize, types: &[DecodedType]) -> Result<Vec<u32>, TranslateError> {
        let value = &self.values[id];
        let DecodedType::Vector { element, lanes } = types[value.ty] else {
            return Err(semantic("LLVM shuffle mask is not a vector"));
        };
        if !matches!(types[element], DecodedType::Integer(32)) {
            return Err(semantic("LLVM shuffle mask is not i32"));
        }
        match value.constant.as_ref() {
            Some(Constant::Null) => Ok(vec![0; lanes as usize]),
            Some(Constant::Undef) => Ok(vec![u32::MAX; lanes as usize]),
            Some(Constant::Data(data)) => data
                .iter()
                .map(|&value| {
                    u32::try_from(value).map_err(|_| semantic("LLVM shuffle mask exceeds i32"))
                })
                .collect(),
            Some(Constant::Aggregate(ids)) => ids
                .iter()
                .map(|&id| {
                    if self.values[id].constant == Some(Constant::Undef) {
                        Ok(u32::MAX)
                    } else {
                        u32::try_from(self.integer(id, types)?)
                            .map_err(|_| semantic("LLVM shuffle lane exceeds i32"))
                    }
                })
                .collect(),
            _ => Err(semantic("LLVM shuffle mask is not constant")),
        }
    }

    /// The module's global variables, with their initializers materialized.
    ///
    /// A global's initializer is a constant which the module's constant block
    /// defines after the record which names it, so this runs once the whole
    /// value table exists.
    pub(super) fn global_variables(
        &self,
        types: &[DecodedType],
    ) -> Result<Vec<crate::ir::GlobalVariable>, TranslateError> {
        let mut operands = HashMap::new();
        self.globals
            .iter()
            .map(|global| {
                let initializer = global
                    .initializer
                    .map(|id| self.materialize(id, types, &mut operands, &mut Vec::new()))
                    .transpose()?;
                Ok(crate::ir::GlobalVariable {
                    name: String::new(),
                    space: global.space,
                    ty: ir_type(global.pointee, types)?,
                    initializer,
                })
            })
            .collect()
    }

    /// Resolve an LLVM value reference to a Metal IR operand, expanding a
    /// constant into a value rather than into instructions.
    fn materialize(
        &self,
        id: usize,
        types: &[DecodedType],
        operands: &mut HashMap<usize, Operand>,
        active: &mut Vec<usize>,
    ) -> Result<Operand, TranslateError> {
        if let Some(value) = operands.get(&id) {
            return Ok(value.clone());
        }
        if active.contains(&id) {
            return Err(semantic("cyclic LLVM aggregate constant"));
        }
        let value = self
            .values
            .get(id)
            .ok_or_else(|| semantic("missing LLVM constant"))?;
        let ty = ir_type(value.ty, types)?;
        let constant = value
            .constant
            .as_ref()
            .ok_or_else(|| semantic("LLVM value has no lowered operand"))?;
        active.push(id);
        let result = match constant {
            Constant::Integer(value) if ty == Type::Bool => Operand::Bool(*value != 0),
            Constant::Integer(value) => Operand::Integer { ty, value: *value },
            Constant::Float(bits) => Operand::Float { ty, bits: *bits },
            Constant::Undef => Operand::Undef(ty),
            Constant::Null if ty == Type::Bool => Operand::Bool(false),
            Constant::Null if matches!(ty, Type::Int { .. }) => Operand::Integer { ty, value: 0 },
            Constant::Null if matches!(ty, Type::Float { .. }) => Operand::Float { ty, bits: 0 },
            Constant::Aggregate(ids) => {
                let mut elements = Vec::new();
                for &id in ids {
                    elements.push(self.materialize(id, types, operands, active)?);
                }
                Operand::Composite { ty, elements }
            }
            Constant::Data(data) => {
                let member_types = aggregate_types(&types[value.ty])?;
                let mut elements = Vec::new();
                for (&bits, member) in data.iter().zip(member_types) {
                    let member = ir_type(member, types)?;
                    elements.push(match member {
                        Type::Int { .. } => Operand::Integer {
                            ty: member,
                            value: bits,
                        },
                        Type::Float { .. } => Operand::Float { ty: member, bits },
                        _ => return Err(semantic("LLVM DATA requires numeric scalar elements")),
                    });
                }
                Operand::Composite { ty, elements }
            }
            Constant::Null => zero_composite(&ty)?,
            Constant::Global(index) => Operand::Global { index: *index, ty },
        };
        active.pop();
        operands.insert(id, result.clone());
        Ok(result)
    }
}

/// Recognize compiler support functions whose bodies have a direct Metal IR
/// operation. SPIRV-Cross emits this helper when translating GLSL's unsigned
/// `findMSB`; retaining all non-entry LLVM functions just for it would obscure
/// the operation the source shader requested.
fn known_helper(
    name: &str,
    arguments: &[Operand],
) -> Option<Result<InstructionKind, TranslateError>> {
    match (name, arguments) {
        ("_Z11spvFindUMSBIjET_S0_", [value]) => Some(Ok(InstructionKind::IntegerUnary {
            op: IntegerUnaryOp::FindMostSignificantBitUnsigned,
            value: value.clone(),
        })),
        ("_Z11spvFindUMSBIjET_S0_", _) => Some(Err(semantic(format!(
            "LLVM helper {name} takes one operand"
        )))),
        _ => None,
    }
}

/// LLVM cast opcodes. Each becomes an explicit Metal IR conversion; the IR
/// verifier then checks the operand and result shapes.
/// Decode an LLVM cast opcode and check that it agrees with the operand and
/// result types. Vector casts convert lanewise and keep their lane count.
fn cast_operation(
    opcode: u64,
    source: usize,
    target: usize,
    types: &[DecodedType],
) -> Result<ConvertOp, TranslateError> {
    let op = match opcode {
        0 => ConvertOp::IntegerTruncate,
        1 => ConvertOp::ZeroExtend,
        2 => ConvertOp::SignExtend,
        3 => ConvertOp::FloatToUnsigned,
        4 => ConvertOp::FloatToSigned,
        5 => ConvertOp::UnsignedToFloat,
        6 => ConvertOp::SignedToFloat,
        7 | 8 => ConvertOp::FloatResize,
        11 => ConvertOp::Bitcast,
        _ => return Err(semantic(format!("unsupported LLVM cast opcode {opcode}"))),
    };
    // A pointer cast reinterprets addressable memory. SPIR-V's logical
    // addressing has no such operation; the body decoder handles the widening
    // form separately and anything else is rejected here.
    if matches!(types[source], DecodedType::Pointer { .. })
        || matches!(types[target], DecodedType::Pointer { .. })
    {
        return Err(semantic(
            "LLVM pointer cast needs explicit buffer reinterpretation",
        ));
    }
    // A bitcast reinterprets bits, so it alone may change the lane count as
    // long as the value's total width is unchanged.
    if op == ConvertOp::Bitcast {
        let width = |index: usize| -> Option<u32> {
            match &types[index] {
                DecodedType::Integer(bits) | DecodedType::Float(bits) => Some(u32::from(*bits)),
                DecodedType::Vector { element, lanes } => match &types[*element] {
                    DecodedType::Integer(bits) | DecodedType::Float(bits) => {
                        Some(u32::from(*bits) * lanes)
                    }
                    _ => None,
                },
                _ => None,
            }
        };
        return match (width(source), width(target)) {
            (Some(from), Some(to)) if from == to => Ok(op),
            _ => Err(semantic(
                "LLVM bitcast changes a value's total width or is not numeric",
            )),
        };
    }
    let (source, target) = match (&types[source], &types[target]) {
        (
            DecodedType::Vector {
                element: source,
                lanes,
            },
            DecodedType::Vector {
                element: target,
                lanes: target_lanes,
            },
        ) if lanes == target_lanes => (&types[*source], &types[*target]),
        (DecodedType::Vector { .. }, _) | (_, DecodedType::Vector { .. }) => {
            return Err(semantic("LLVM cast changes a value's lane count"));
        }
        (source, target) => (source, target),
    };
    let valid = match (op, source, target) {
        (ConvertOp::IntegerTruncate, DecodedType::Integer(from), DecodedType::Integer(to)) => {
            to < from
        }
        (
            ConvertOp::ZeroExtend | ConvertOp::SignExtend,
            DecodedType::Integer(from),
            DecodedType::Integer(to),
        ) => to > from,
        (
            ConvertOp::FloatToUnsigned | ConvertOp::FloatToSigned,
            DecodedType::Float(_),
            DecodedType::Integer(_),
        )
        | (
            ConvertOp::UnsignedToFloat | ConvertOp::SignedToFloat,
            DecodedType::Integer(_),
            DecodedType::Float(_),
        ) => true,
        (ConvertOp::FloatResize, DecodedType::Float(from), DecodedType::Float(to)) => from != to,
        (ConvertOp::Bitcast, source, target) => scalar_width(source) == scalar_width(target),
        _ => false,
    };
    if !valid {
        return Err(semantic(format!(
            "LLVM cast opcode {opcode} does not apply to its operand and result types"
        )));
    }
    Ok(op)
}

fn scalar_width(ty: &DecodedType) -> Option<u8> {
    match ty {
        DecodedType::Integer(bits) | DecodedType::Float(bits) => Some(*bits),
        _ => None,
    }
}

fn integer_predicate(predicate: u64) -> Result<IntegerPredicate, TranslateError> {
    Ok(match predicate {
        32 => IntegerPredicate::Equal,
        33 => IntegerPredicate::NotEqual,
        34 => IntegerPredicate::UnsignedGreaterThan,
        35 => IntegerPredicate::UnsignedGreaterEqual,
        36 => IntegerPredicate::UnsignedLessThan,
        37 => IntegerPredicate::UnsignedLessEqual,
        38 => IntegerPredicate::SignedGreaterThan,
        39 => IntegerPredicate::SignedGreaterEqual,
        40 => IntegerPredicate::SignedLessThan,
        41 => IntegerPredicate::SignedLessEqual,
        _ => {
            return Err(semantic(format!(
                "unsupported LLVM integer comparison predicate {predicate}"
            )));
        }
    })
}

fn float_predicate(predicate: u64) -> Result<FloatPredicate, TranslateError> {
    Ok(match predicate {
        1 => FloatPredicate::OrderedEqual,
        2 => FloatPredicate::OrderedGreaterThan,
        3 => FloatPredicate::OrderedGreaterEqual,
        4 => FloatPredicate::OrderedLessThan,
        5 => FloatPredicate::OrderedLessEqual,
        6 => FloatPredicate::OrderedNotEqual,
        7 => FloatPredicate::Ordered,
        8 => FloatPredicate::Unordered,
        9 => FloatPredicate::UnorderedEqual,
        10 => FloatPredicate::UnorderedGreaterThan,
        11 => FloatPredicate::UnorderedGreaterEqual,
        12 => FloatPredicate::UnorderedLessThan,
        13 => FloatPredicate::UnorderedLessEqual,
        14 => FloatPredicate::UnorderedNotEqual,
        // Constant-valued comparisons would need a materialized constant
        // rather than an instruction; AIR does not emit them.
        _ => {
            return Err(semantic(format!(
                "unsupported LLVM floating comparison predicate {predicate}"
            )));
        }
    })
}

/// The `i1` result type of a comparison, keeping the operand's vector shape.
fn boolean_type(operand: usize, types: &[DecodedType]) -> Result<usize, TranslateError> {
    let scalar = types
        .iter()
        .position(|ty| matches!(ty, DecodedType::Integer(1)))
        .ok_or_else(|| semantic("LLVM comparison has no boolean type"))?;
    match types[operand] {
        DecodedType::Vector { lanes, .. } => types
            .iter()
            .position(
                |ty| matches!(ty, DecodedType::Vector { element, lanes: n } if *element == scalar && *n == lanes),
            )
            .ok_or_else(|| semantic("LLVM comparison has no boolean vector type")),
        _ => Ok(scalar),
    }
}

fn member_type(ty: &DecodedType, index: u32) -> Result<usize, TranslateError> {
    match ty {
        DecodedType::Struct { fields, .. } => fields
            .get(index as usize)
            .copied()
            .ok_or_else(|| semantic("LLVM aggregate index is out of range")),
        DecodedType::Array { element, count } if index < *count => Ok(*element),
        DecodedType::Vector { element, lanes } if index < *lanes => Ok(*element),
        _ => Err(semantic("LLVM aggregate index applies to a non-aggregate")),
    }
}

/// Decode an AIR intrinsic name into a Metal IR operation.
///
/// AIR names are `air.<operation>[.<type suffix>]*`, where the operation may
/// carry a `fast_` or `precise_` prefix selecting the math library's accuracy.
/// Matching on the operation rather than the full spelling keeps every vector
/// and scalar width on one lowering path.
///
/// FIXME (ACCURACY): `fast_` and `precise_` lower identically. SPIR-V has no
/// portable way to request Metal's relaxed accuracy, and the difference is
/// below the precision most shaders depend on.
fn air_intrinsic(name: &str, arguments: &[Operand]) -> Result<InstructionKind, TranslateError> {
    let unsupported = || semantic(format!("LLVM intrinsic {name} needs Metal IR lowering"));
    let mut parts = name.split('.');
    if parts.next() != Some("air") {
        return Err(unsupported());
    }
    let operation = parts.next().ok_or_else(unsupported)?;
    let operation = operation
        .strip_prefix("fast_")
        .or_else(|| operation.strip_prefix("precise_"))
        .unwrap_or(operation);
    // AIR spells an integer operation's signedness in its first type suffix,
    // for instance `air.min.u.i32` and `air.abs.s.v3i32`.
    let signedness = parts
        .clone()
        .next()
        .filter(|part| matches!(*part, "s" | "u"));
    if operation == "convert" {
        // `air.convert.<result kind>.<result type>.<source kind>.<source type>`
        let suffix: Vec<_> = parts.collect();
        let [result_kind, _, source_kind, _] = suffix.as_slice() else {
            return Err(unsupported());
        };
        let op = match (*result_kind, *source_kind) {
            ("f", "u") => ConvertOp::UnsignedToFloat,
            ("f", "s") => ConvertOp::SignedToFloat,
            ("u", "f") => ConvertOp::FloatToUnsigned,
            ("s", "f") => ConvertOp::FloatToSigned,
            // Metal spells `half(x)` and `float(h)` as a conversion between
            // two floating kinds; the type suffixes say which widths.
            ("f", "f") => ConvertOp::FloatResize,
            _ => return Err(unsupported()),
        };
        let [value] = arguments else {
            return Err(semantic(format!("LLVM intrinsic {name} takes one operand")));
        };
        return Ok(InstructionKind::Convert {
            op,
            value: value.clone(),
        });
    }
    if operation == "wg" && parts.clone().next() == Some("barrier") {
        let [memory, scope] = arguments else {
            return Err(semantic(format!(
                "LLVM intrinsic {name} takes two operands"
            )));
        };
        return Ok(InstructionKind::Barrier {
            scope: barrier_scope(scope, name)?,
            memory: barrier_memory(memory, name)?,
        });
    }
    if operation == "atomic" {
        return air_atomic(name, parts.collect::<Vec<_>>().as_slice(), arguments);
    }
    if let Some(kind) = air_texture_intrinsic(operation, arguments)? {
        return Ok(kind);
    }
    let unary = |op| match arguments {
        [value] => Ok(InstructionKind::FloatUnary {
            op,
            value: value.clone(),
        }),
        _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
    };
    let binary = |op| match arguments {
        [lhs, rhs] => Ok(InstructionKind::FloatBinary {
            op,
            lhs: lhs.clone(),
            rhs: rhs.clone(),
        }),
        _ => Err(semantic(format!(
            "LLVM intrinsic {name} takes two operands"
        ))),
    };
    if let Some(signedness) = signedness {
        let signed = signedness == "s";
        let integer_binary = |op| match arguments {
            [lhs, rhs] => Ok(InstructionKind::IntegerBinary {
                op,
                lhs: lhs.clone(),
                rhs: rhs.clone(),
            }),
            _ => Err(semantic(format!(
                "LLVM intrinsic {name} takes two operands"
            ))),
        };
        let integer_unary = |op| match arguments {
            [value] => Ok(InstructionKind::IntegerUnary {
                op,
                value: value.clone(),
            }),
            _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
        };
        match operation {
            "abs" => return integer_unary(IntegerUnaryOp::AbsoluteSigned),
            "sign" => return integer_unary(IntegerUnaryOp::SignSigned),
            "min" if signed => return integer_binary(IntegerBinaryOp::MinimumSigned),
            "min" => return integer_binary(IntegerBinaryOp::MinimumUnsigned),
            "max" if signed => return integer_binary(IntegerBinaryOp::MaximumSigned),
            "max" => return integer_binary(IntegerBinaryOp::MaximumUnsigned),
            "clamp" => {
                return match arguments {
                    [value, min, max] => Ok(InstructionKind::IntegerClamp {
                        signed,
                        value: value.clone(),
                        min: min.clone(),
                        max: max.clone(),
                    }),
                    _ => Err(semantic(format!(
                        "LLVM intrinsic {name} takes three operands"
                    ))),
                };
            }
            _ => {}
        }
    }
    match operation {
        // The second operand is LLVM's "zero is undefined" hint. This
        // lowering answers the width for zero whether or not the caller would
        // have accepted anything, so the hint changes nothing.
        "clz" => match arguments {
            [value] | [value, _] => Ok(InstructionKind::IntegerUnary {
                op: IntegerUnaryOp::CountLeadingZeros,
                value: value.clone(),
            }),
            _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
        },
        "ctz" => match arguments {
            [value] | [value, _] => Ok(InstructionKind::IntegerUnary {
                op: IntegerUnaryOp::CountTrailingZeros,
                value: value.clone(),
            }),
            _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
        },
        "popcount" => match arguments {
            [value] => Ok(InstructionKind::IntegerUnary {
                op: IntegerUnaryOp::PopulationCount,
                value: value.clone(),
            }),
            _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
        },
        "fabs" => unary(FloatUnaryOp::Absolute),
        "sign" => unary(FloatUnaryOp::Sign),
        "sqrt" => unary(FloatUnaryOp::Sqrt),
        "rsqrt" => unary(FloatUnaryOp::InverseSqrt),
        "exp" => unary(FloatUnaryOp::Exp),
        "exp2" => unary(FloatUnaryOp::Exp2),
        "log" => unary(FloatUnaryOp::Log),
        "log2" => unary(FloatUnaryOp::Log2),
        "sin" => unary(FloatUnaryOp::Sin),
        "cos" => unary(FloatUnaryOp::Cos),
        "tan" => unary(FloatUnaryOp::Tan),
        "asin" => unary(FloatUnaryOp::Asin),
        "acos" => unary(FloatUnaryOp::Acos),
        "atan" => unary(FloatUnaryOp::Atan),
        "floor" => unary(FloatUnaryOp::Floor),
        "ceil" => unary(FloatUnaryOp::Ceil),
        "rint" | "round" => unary(FloatUnaryOp::Round),
        "trunc" => unary(FloatUnaryOp::Truncate),
        "fract" => unary(FloatUnaryOp::Fract),
        "dfdx" => unary(FloatUnaryOp::DerivativeX),
        "dfdy" => unary(FloatUnaryOp::DerivativeY),
        "fwidth" => unary(FloatUnaryOp::DerivativeWidth),
        "fmin" => binary(FloatBinaryOp::Minimum),
        "fmax" => binary(FloatBinaryOp::Maximum),
        "pow" | "powr" => binary(FloatBinaryOp::Power),
        "atan2" => binary(FloatBinaryOp::Atan2),
        "step" => binary(FloatBinaryOp::Step),
        "fmod" => binary(FloatBinaryOp::Remainder),
        "dot" => match arguments {
            [lhs, rhs] => Ok(InstructionKind::FloatDot {
                lhs: lhs.clone(),
                rhs: rhs.clone(),
            }),
            _ => Err(semantic(format!(
                "LLVM intrinsic {name} takes two operands"
            ))),
        },
        "clamp" => match arguments {
            [value, min, max] => Ok(InstructionKind::FloatClamp {
                value: value.clone(),
                min: min.clone(),
                max: max.clone(),
            }),
            _ => Err(semantic(format!(
                "LLVM intrinsic {name} takes three operands"
            ))),
        },
        "mix" => match arguments {
            [first, second, weight] => Ok(InstructionKind::FloatMix {
                first: first.clone(),
                second: second.clone(),
                weight: weight.clone(),
            }),
            _ => Err(semantic(format!(
                "LLVM intrinsic {name} takes three operands"
            ))),
        },
        "any" => match arguments {
            [vector] => Ok(InstructionKind::Any {
                vector: vector.clone(),
            }),
            _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
        },
        "all" => match arguments {
            [vector] => Ok(InstructionKind::All {
                vector: vector.clone(),
            }),
            _ => Err(semantic(format!("LLVM intrinsic {name} takes one operand"))),
        },
        "discard_fragment" => Ok(InstructionKind::DiscardFragment),
        "fma" => match arguments {
            [first, second, addend] => Ok(InstructionKind::FloatFma {
                first: first.clone(),
                second: second.clone(),
                addend: addend.clone(),
            }),
            _ => Err(semantic(format!(
                "LLVM intrinsic {name} takes three operands"
            ))),
        },
        _ => Err(unsupported()),
    }
}

/// The constant an AIR flag or enumerator argument must be.
///
/// Metal spells all four of a barrier's and an atomic's qualifiers as
/// compile-time enumerators, so a run-time value here is a shader this
/// translator has misread rather than one it should approximate.
fn air_enumerator(operand: &Operand, name: &str, what: &str) -> Result<u64, TranslateError> {
    match operand {
        Operand::Integer { value, .. } => Ok(*value),
        _ => Err(semantic(format!(
            "LLVM intrinsic {name} has a run-time {what}, which Metal spells as a constant"
        ))),
    }
}

/// Metal's `mem_flags`, which say which address spaces a barrier orders.
fn barrier_memory(operand: &Operand, name: &str) -> Result<BarrierMemory, TranslateError> {
    let flags = air_enumerator(operand, name, "memory flag set")?;
    // `mem_none` is 0; the rest are one bit each. `mem_threadgroup_imageblock`
    // and `mem_object_data` are threadgroup memory as far as ordering goes.
    const DEVICE: u64 = 1;
    const THREADGROUP: u64 = 2;
    const TEXTURE: u64 = 4;
    const IMAGEBLOCK: u64 = 8;
    const OBJECT_DATA: u64 = 16;
    let known = DEVICE | THREADGROUP | TEXTURE | IMAGEBLOCK | OBJECT_DATA;
    if flags & !known != 0 {
        return Err(semantic(format!(
            "LLVM intrinsic {name} names unknown Metal memory flags {flags}"
        )));
    }
    Ok(BarrierMemory {
        device: flags & DEVICE != 0,
        threadgroup: flags & (THREADGROUP | IMAGEBLOCK | OBJECT_DATA) != 0,
        texture: flags & TEXTURE != 0,
    })
}

/// Metal's `memory_scope`, which says what a barrier makes wait and how far an
/// atomic's ordering reaches.
fn barrier_scope(operand: &Operand, name: &str) -> Result<BarrierScope, TranslateError> {
    match air_enumerator(operand, name, "scope")? {
        // `memory_scope_thread` orders one invocation against itself, which is
        // the narrowest scope SPIR-V has a spelling for below a subgroup.
        //
        // FIXME (ACCURACY): it is widened to the SIMD-group. A wider scope
        // costs a stronger barrier than Metal asked for, never a weaker one.
        0 | 1 => Ok(BarrierScope::Simdgroup),
        2 => Ok(BarrierScope::Threadgroup),
        3 => Ok(BarrierScope::Device),
        scope => Err(semantic(format!(
            "LLVM intrinsic {name} names unknown Metal memory scope {scope}"
        ))),
    }
}

/// `air.atomic.<space>.<operation>[.<signedness>].<type>` and its operands.
///
/// The pointer's own address space says which memory the operation is in, so
/// the `global`/`local` suffix is checked rather than believed.
fn air_atomic(
    name: &str,
    suffix: &[&str],
    arguments: &[Operand],
) -> Result<InstructionKind, TranslateError> {
    let unsupported = || semantic(format!("LLVM intrinsic {name} needs Metal IR lowering"));
    let [space, operation, rest @ ..] = suffix else {
        return Err(unsupported());
    };
    if !matches!(*space, "global" | "local") {
        return Err(unsupported());
    }
    let signed = matches!(rest.first(), Some(&"s"));
    let op = match *operation {
        "add" => AtomicOp::Add,
        "sub" => AtomicOp::Subtract,
        "min" if signed => AtomicOp::MinimumSigned,
        "min" => AtomicOp::MinimumUnsigned,
        "max" if signed => AtomicOp::MaximumSigned,
        "max" => AtomicOp::MaximumUnsigned,
        "and" => AtomicOp::And,
        "or" => AtomicOp::Or,
        "xor" => AtomicOp::Xor,
        "xchg" => AtomicOp::Exchange,
        _ => return Err(unsupported()),
    };
    let [pointer, value, order, scope] = arguments else {
        return Err(semantic(format!(
            "LLVM intrinsic {name} takes four operands"
        )));
    };
    Ok(InstructionKind::Atomic {
        op,
        pointer: pointer.clone(),
        value: value.clone(),
        order: memory_order(order, name)?,
        scope: barrier_scope(scope, name)?,
    })
}

/// Metal's `memory_order`, of which the language exposes two.
fn memory_order(operand: &Operand, name: &str) -> Result<MemoryOrder, TranslateError> {
    match air_enumerator(operand, name, "memory order")? {
        0 => Ok(MemoryOrder::Relaxed),
        // FIXME (ACCURACY): every other ordering is raised to sequential
        // consistency. Metal's own `memory_order` has only `relaxed` and
        // `seq_cst`, and a stronger order costs ordering, not correctness.
        _ => Ok(MemoryOrder::SequentiallyConsistent),
    }
}

fn aggregate_types(ty: &DecodedType) -> Result<Vec<usize>, TranslateError> {
    match ty {
        DecodedType::Vector { element, lanes } => Ok(vec![*element; *lanes as usize]),
        DecodedType::Array { element, count } => Ok(vec![*element; *count as usize]),
        DecodedType::Struct { fields, .. } => Ok(fields.clone()),
        _ => Err(semantic("LLVM aggregate constant has nonaggregate type")),
    }
}

fn emit(
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
    ty: Type,
    kind: InstructionKind,
) -> Operand {
    let result = ValueId(function.values.len() as u32);
    function.values.push(ty);
    instructions.push(Instruction {
        result: Some(result),
        kind,
    });
    Operand::Value(result)
}

fn zero_composite(ty: &Type) -> Result<Operand, TranslateError> {
    let members: Vec<&Type> = match ty {
        Type::Bool => return Ok(Operand::Bool(false)),
        Type::Int { .. } => {
            return Ok(Operand::Integer {
                ty: ty.clone(),
                value: 0,
            });
        }
        Type::Float { .. } => {
            return Ok(Operand::Float {
                ty: ty.clone(),
                bits: 0,
            });
        }
        Type::Vector { element, lanes } => vec![element.as_ref(); usize::from(*lanes)],
        Type::Array { element, count } => vec![element.as_ref(); *count as usize],
        Type::Struct(structure) => structure.fields.iter().map(|field| &field.ty).collect(),
        _ => return Err(semantic("LLVM zero constant type needs lowering")),
    };
    Ok(Operand::Composite {
        ty: ty.clone(),
        elements: members
            .into_iter()
            .map(zero_composite)
            .collect::<Result<_, _>>()?,
    })
}

/// AIR numbers its address spaces the way Metal names them.
fn address_space_of(space: u64) -> Result<AddressSpace, TranslateError> {
    Ok(match space {
        0 => AddressSpace::Thread,
        1 => AddressSpace::Device,
        2 => AddressSpace::Constant,
        3 => AddressSpace::Threadgroup,
        _ => return Err(semantic("unsupported AIR address space")),
    })
}

fn ir_type(index: usize, types: &[DecodedType]) -> Result<Type, TranslateError> {
    fn convert(
        index: usize,
        types: &[DecodedType],
        active: &mut Vec<usize>,
    ) -> Result<Type, TranslateError> {
        if active.contains(&index) {
            return Err(semantic(
                "recursive LLVM type needs explicit layout support",
            ));
        }
        active.push(index);
        let ty = match types
            .get(index)
            .ok_or_else(|| semantic("missing LLVM type"))?
        {
            DecodedType::Void => Type::Void,
            DecodedType::Integer(1) => Type::Bool,
            DecodedType::Integer(bits) => Type::Int {
                bits: *bits,
                signed: false,
            },
            DecodedType::Float(bits) => Type::Float { bits: *bits },
            DecodedType::Vector { element, lanes } => Type::Vector {
                element: Box::new(convert(*element, types, active)?),
                lanes: u8::try_from(*lanes).map_err(|_| semantic("LLVM vector too wide"))?,
            },
            DecodedType::Array { element, count } => Type::Array {
                element: Box::new(convert(*element, types, active)?),
                count: *count,
            },
            DecodedType::Pointer {
                pointee,
                address_space,
            } => Type::Pointer {
                space: address_space_of(*address_space)?,
                pointee: Box::new(convert(*pointee, types, active)?),
            },
            DecodedType::Struct { fields, packed } => {
                let mut result = Vec::new();
                let mut size = 0u32;
                let mut alignment = 1;
                for &field in fields {
                    let ty = convert(field, types, active)?;
                    let (field_size, field_align) = layout(&ty)?;
                    let field_align = if *packed { 1 } else { field_align };
                    alignment = alignment.max(field_align);
                    size = align(size, field_align)?;
                    result.push(crate::ir::StructField {
                        name: None,
                        ty,
                        offset: size,
                    });
                    size = size
                        .checked_add(field_size)
                        .ok_or_else(|| semantic("LLVM struct size overflow"))?;
                }
                Type::Struct(crate::ir::StructType {
                    fields: result,
                    size: align(size, alignment)?,
                    alignment,
                    packed: *packed,
                })
            }
            _ => return Err(semantic("LLVM type has no Metal value representation")),
        };
        active.pop();
        Ok(ty)
    }
    convert(index, types, &mut Vec::new())
}

fn align(size: u32, alignment: u32) -> Result<u32, TranslateError> {
    size.checked_add(alignment - 1)
        .map(|size| size / alignment * alignment)
        .ok_or_else(|| semantic("LLVM layout overflow"))
}

fn layout(ty: &Type) -> Result<(u32, u32), TranslateError> {
    match ty {
        Type::Int { bits, .. } | Type::Float { bits } if matches!(bits, 8 | 16 | 32 | 64) => {
            Ok((u32::from(*bits) / 8, u32::from(*bits) / 8))
        }
        Type::Vector { element, lanes } if matches!(lanes, 2..=4) => {
            let (size, _) = layout(element)?;
            let alignment = size * u32::from(*lanes).next_power_of_two();
            Ok((alignment, alignment))
        }
        Type::Array { element, count } => {
            let (size, alignment) = layout(element)?;
            Ok((
                size.checked_mul(*count)
                    .ok_or_else(|| semantic("LLVM array size overflow"))?,
                alignment,
            ))
        }
        Type::Struct(s) => Ok((s.size, s.alignment)),
        _ => Err(semantic("unsupported AIR type layout")),
    }
}

fn decode_constant(
    code: u64,
    fields: &[u64],
    ty: &DecodedType,
) -> Result<Constant, TranslateError> {
    match (code, fields, ty) {
        (2 | 3, [], DecodedType::Void | DecodedType::Function { .. } | DecodedType::Metadata) => {
            Err(semantic("LLVM constant has a nonvalue type"))
        }
        (2, [], _) => Ok(Constant::Null),
        (3, [], _) => Ok(Constant::Undef),
        (4, [encoded], DecodedType::Integer(bits @ 1..=64)) => {
            let value = decode_sign_rotated(*encoded) as u64;
            Ok(Constant::Integer(if *bits == 64 {
                value
            } else {
                value & ((1u64 << bits) - 1)
            }))
        }
        (6, [bits], DecodedType::Float(width @ (16 | 32 | 64)))
            if *width == 64 || *bits < (1u64 << width) =>
        {
            Ok(Constant::Float(*bits))
        }
        (7, ids, _) => {
            aggregate_types(ty)?;
            Ok(Constant::Aggregate(
                ids.iter()
                    .map(|id| {
                        usize::try_from(*id)
                            .map_err(|_| semantic("LLVM constant reference overflows"))
                    })
                    .collect::<Result<_, _>>()?,
            ))
        }
        (22, data, DecodedType::Vector { lanes: count, .. } | DecodedType::Array { count, .. })
            if data.len() == *count as usize =>
        {
            Ok(Constant::Data(data.to_vec()))
        }
        _ => Err(semantic(format!(
            "unsupported or malformed LLVM constant record {code} for {}",
            ty.description()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_spirv_cross_unsigned_find_msb_helper() {
        let value = Operand::u32(7);
        let kind = known_helper("_Z11spvFindUMSBIjET_S0_", std::slice::from_ref(&value))
            .expect("the helper is recognized")
            .unwrap();
        assert_eq!(
            kind,
            InstructionKind::IntegerUnary {
                op: IntegerUnaryOp::FindMostSignificantBitUnsigned,
                value,
            }
        );
    }

    // Encode owned, unabbreviated records to exercise the real bitstream
    // reader without an LLVM toolchain or proprietary shader fixtures.
    fn records_block(id: u32, records: &[(u64, &[u64])]) -> BitcodeBlock {
        use crate::air::fixtures::{bitcode_block, block, record};
        bitcode_block(block(
            id,
            records
                .iter()
                .map(|(code, fields)| record(*code, fields))
                .collect(),
        ))
    }

    #[test]
    fn decodes_both_constant_scopes_and_forward_aggregate_members() {
        let types = vec![
            DecodedType::Integer(32),
            DecodedType::Vector {
                element: 0,
                lanes: 2,
            },
        ];
        let mut module = records_block(8, &[(1, &[1])]);
        module.elements.push(BitcodeElement::Block(records_block(
            11,
            &[(1, &[0]), (2, &[])],
        )));
        let mut body = records_block(12, &[(1, &[1])]);
        body.elements.push(BitcodeElement::Block(records_block(
            11,
            &[(1, &[1]), (7, &[0, 3]), (1, &[0]), (4, &[2])],
        )));
        let table = ValueTable::decode(&module, &body, &types, &[1]).unwrap();
        assert_eq!(table.argument_start, 1);
        assert_eq!(table.values.len(), 4);
        assert_eq!(
            table.values[2].constant,
            Some(Constant::Aggregate(vec![0, 3]))
        );
        assert_eq!(table.shuffle_mask(2, &types).unwrap(), vec![0, 1]);
        body.elements.pop();
        body.elements.push(BitcodeElement::Block(records_block(
            11,
            &[(1, &[1]), (7, &[0, 99])],
        )));
        assert!(ValueTable::decode(&module, &body, &types, &[1]).is_err());
    }

    #[test]
    fn lowers_extractelement_36_98_using_values_not_record_pattern() {
        let types = vec![
            DecodedType::Integer(32),
            DecodedType::Vector {
                element: 0,
                lanes: 4,
            },
        ];
        let mut values = vec![
            Value {
                ty: 0,
                constant: Some(Constant::Null)
            };
            105
        ];
        values[69] = Value {
            ty: 1,
            constant: None,
        };
        let table = ValueTable {
            globals: Vec::new(),
            relative: true,
            values,
            argument_start: 69,
        };
        let body = records_block(12, &[(1, &[1]), (6, &[36, 98]), (10, &[1])]);
        let function = table
            .lower_body(
                &body,
                &types,
                &Signature {
                    names: &HashMap::new(),
                    name: "extract".into(),
                    parameter_types: &[1],
                    result_type: 0,
                    opaque_types: &HashMap::new(),
                },
            )
            .unwrap();
        assert_eq!(
            function.blocks[0].instructions[0].kind,
            InstructionKind::CompositeExtract {
                composite: Operand::Value(ValueId(0)),
                index: 0
            }
        );
        Module {
            globals: Vec::new(),
            functions: vec![function],
            entries: Vec::new(),
        }
        .verify()
        .unwrap();
    }

    /// LLVM's `switch` has no Metal IR form: it becomes one equality test
    /// per case, each in the block the test before it falls through to, with
    /// the phis of every target following the edge which now reaches it.
    #[test]
    fn a_switch_expands_into_a_chain_of_comparisons() {
        let types = vec![
            DecodedType::Integer(32),
            DecodedType::Integer(1),
            DecodedType::Void,
        ];
        let table = ValueTable {
            globals: Vec::new(),
            relative: true,
            values: vec![
                Value {
                    ty: 0,
                    constant: Some(Constant::Integer(7)),
                },
                Value {
                    ty: 0,
                    constant: Some(Constant::Integer(9)),
                },
            ],
            argument_start: 0,
        };
        // entry:  switch i32 7, label %block3 [ 7, %block1  9, %block2 ]
        // block1: br block3
        // block2: br block3
        // block3: %r = phi i32 [7, entry], [9, block1], [7, block2]; ret void
        let body = records_block(
            12,
            &[
                (1, &[4]),
                // SWITCH: [condition type, condition, default, (value, block)...]
                (12, &[0, 2, 3, 0, 1, 1, 2]),
                (11, &[3]),
                (11, &[3]),
                (16, &[0, 4, 0, 2, 1, 4, 2]),
                (10, &[]),
            ],
        );
        let function = table
            .lower_body(
                &body,
                &types,
                &Signature {
                    names: &HashMap::new(),
                    name: "choose".into(),
                    parameter_types: &[],
                    result_type: 2,
                    opaque_types: &HashMap::new(),
                },
            )
            .unwrap();
        // Two cases: the first test stays in the switch's block and the
        // second gets a block of its own.
        assert_eq!(function.blocks.len(), 5);
        assert!(matches!(
            function.blocks[0].instructions.last().map(|i| &i.kind),
            Some(InstructionKind::IntegerCompare {
                predicate: IntegerPredicate::Equal,
                ..
            })
        ));
        let InstructionKind::Phi { incomings } = &function.blocks[3].instructions[0].kind else {
            panic!("the default block keeps its phi");
        };
        // The edge from the switch block now arrives from the last test.
        assert!(
            incomings
                .iter()
                .any(|(from, _)| *from == function.blocks[4].id),
            "the default's phi follows the chain's last block"
        );
        Module {
            globals: Vec::new(),
            functions: vec![function],
            entries: Vec::new(),
        }
        .verify()
        .unwrap();
    }

    /// A body whose blocks are laid out so that a comparison names the phi
    /// which dominates it but appears later in the record stream.
    #[test]
    fn lowers_multiple_blocks_with_a_phi_and_a_forward_reference() {
        let types = vec![
            DecodedType::Integer(32),
            DecodedType::Integer(1),
            DecodedType::Void,
        ];
        let table = ValueTable {
            globals: Vec::new(),
            relative: true,
            values: vec![
                Value {
                    ty: 0,
                    constant: Some(Constant::Integer(0)),
                },
                Value {
                    ty: 0,
                    constant: Some(Constant::Integer(1)),
                },
            ],
            argument_start: 0,
        };
        // entry:  br block2
        // block1: %2 = icmp ult %3, 1; br %2, block2, block3
        // block2: %3 = phi i32 [0, entry], [1, block1]; br block1
        // block3: ret void
        let body = records_block(
            12,
            &[
                (1, &[4]),
                (11, &[2]),
                (28, &[4294967295, 0, 1, 36]),
                (11, &[2, 3, 1]),
                (16, &[0, 6, 0, 4, 1]),
                (11, &[1]),
                (10, &[]),
            ],
        );
        let function = table
            .lower_body(
                &body,
                &types,
                &Signature {
                    names: &HashMap::new(),
                    name: "loop".into(),
                    parameter_types: &[],
                    result_type: 2,
                    opaque_types: &HashMap::new(),
                },
            )
            .unwrap();
        assert_eq!(function.blocks.len(), 4);
        let InstructionKind::Phi { .. } = function.blocks[2].instructions[0].kind else {
            panic!("the third block starts with a phi");
        };
        // The comparison and the phi must name one SSA value, not two.
        let InstructionKind::IntegerCompare { lhs, .. } = &function.blocks[1].instructions[0].kind
        else {
            panic!("the second block starts with a comparison");
        };
        assert_eq!(
            Some(lhs.clone()),
            function.blocks[2].instructions[0]
                .result
                .map(Operand::Value)
        );
        Module {
            globals: Vec::new(),
            functions: vec![function],
            entries: Vec::new(),
        }
        .verify()
        .unwrap();
    }

    /// A value named before its definition carries its type inline; both uses
    /// must resolve to the one SSA definition.
    #[test]
    fn rejects_a_forward_reference_whose_declared_type_is_wrong() {
        let types = vec![
            DecodedType::Integer(32),
            DecodedType::Integer(1),
            DecodedType::Void,
        ];
        let table = ValueTable {
            globals: Vec::new(),
            relative: true,
            values: vec![Value {
                ty: 0,
                constant: Some(Constant::Integer(1)),
            }],
            argument_start: 0,
        };
        // The comparison declares its forward operand as `i1`, but the
        // instruction which defines it produces an `i32`.
        let body = records_block(
            12,
            &[
                (1, &[2]),
                (28, &[2, 1, 1, 36]),
                (11, &[1]),
                (2, &[2, 2, 0]),
                (10, &[]),
            ],
        );
        assert!(
            table
                .lower_body(
                    &body,
                    &types,
                    &Signature {
                        names: &HashMap::new(),
                        name: "bad".into(),
                        parameter_types: &[],
                        result_type: 2,
                        opaque_types: &HashMap::new(),
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn lowers_integer_resize_and_checks_its_direction() {
        let types = vec![DecodedType::Integer(32), DecodedType::Integer(64)];
        let make = || ValueTable {
            globals: Vec::new(),
            relative: true,
            values: vec![Value {
                ty: 0,
                constant: None,
            }],
            argument_start: 0,
        };
        let body = records_block(12, &[(1, &[1]), (3, &[1, 1, 1]), (10, &[1])]);
        let function = make()
            .lower_body(
                &body,
                &types,
                &Signature {
                    names: &HashMap::new(),
                    name: "extend".into(),
                    parameter_types: &[0],
                    result_type: 1,
                    opaque_types: &HashMap::new(),
                },
            )
            .unwrap();
        Module {
            globals: Vec::new(),
            functions: vec![function],
            entries: Vec::new(),
        }
        .verify()
        .unwrap();
        let body = records_block(12, &[(1, &[1]), (3, &[1, 1, 0]), (10, &[1])]);
        assert!(
            make()
                .lower_body(
                    &body,
                    &types,
                    &Signature {
                        names: &HashMap::new(),
                        name: "bad".into(),
                        parameter_types: &[0],
                        result_type: 1,
                        opaque_types: &HashMap::new(),
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn extraction_uses_constant_contents_and_rejects_bad_indices() {
        let types = vec![
            DecodedType::Integer(32),
            DecodedType::Vector {
                element: 0,
                lanes: 4,
            },
            DecodedType::Float(32),
        ];
        let make = |ty, constant| ValueTable {
            globals: Vec::new(),
            relative: true,
            argument_start: 0,
            values: vec![
                Value {
                    ty: 1,
                    constant: None,
                },
                Value {
                    ty,
                    constant: Some(constant),
                },
            ],
        };
        let body = records_block(12, &[(1, &[1]), (6, &[2, 1]), (10, &[1])]);
        let function = make(0, Constant::Integer(2))
            .lower_body(
                &body,
                &types,
                &Signature {
                    names: &HashMap::new(),
                    name: "lane2".into(),
                    parameter_types: &[1],
                    result_type: 0,
                    opaque_types: &HashMap::new(),
                },
            )
            .unwrap();
        assert!(matches!(
            function.blocks[0].instructions[0].kind,
            InstructionKind::CompositeExtract { index: 2, .. }
        ));
        assert!(
            make(0, Constant::Integer(4))
                .lower_body(
                    &body,
                    &types,
                    &Signature {
                        names: &HashMap::new(),
                        name: "bad".into(),
                        parameter_types: &[1],
                        result_type: 0,
                        opaque_types: &HashMap::new(),
                    }
                )
                .is_err()
        );
        assert!(
            make(2, Constant::Null)
                .lower_body(
                    &body,
                    &types,
                    &Signature {
                        names: &HashMap::new(),
                        name: "bad".into(),
                        parameter_types: &[1],
                        result_type: 0,
                        opaque_types: &HashMap::new(),
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn relative_references_include_module_constants() {
        let table = ValueTable {
            globals: Vec::new(),
            relative: true,
            values: vec![
                Value {
                    ty: 0,
                    constant: None
                };
                100
            ],
            argument_start: 0,
        };
        assert_eq!(table.operand_id(36).unwrap(), 64);
        assert_eq!(table.operand_id(98).unwrap(), 2);
        assert!(table.operand_id(101).is_err());
        assert!(table.operand_id(0).is_err());
        assert!(table.operand_id(1u64 << 32).is_err());
    }

    #[test]
    fn absolute_references_do_not_subtract() {
        let table = ValueTable {
            globals: Vec::new(),
            relative: false,
            values: vec![
                Value {
                    ty: 0,
                    constant: None
                };
                100
            ],
            argument_start: 0,
        };
        assert_eq!(table.operand_id(36).unwrap(), 36);
    }

    #[test]
    fn integer_constants_preserve_signed_bits() {
        assert_eq!(
            decode_constant(4, &[3], &DecodedType::Integer(32)).unwrap(),
            Constant::Integer(u32::MAX as u64)
        );
        assert_eq!(
            decode_constant(4, &[1], &DecodedType::Integer(64)).unwrap(),
            Constant::Integer(1u64 << 63)
        );
        assert!(decode_constant(4, &[2, 4], &DecodedType::Integer(32)).is_err());
    }
}

/// The bit of an LLVM call's marker field which says a function type precedes
/// the callee.
const CALL_EXPLICIT_TYPE: u32 = 15;
/// The bit which says fast-math flags precede it.
const CALL_FMF: u32 = 17;

/// A phi whose incoming values are resolved after every block is lowered.
struct PhiFixup {
    block: usize,
    instruction: usize,
    sources: Vec<(BlockId, usize)>,
}

/// LLVM record codes which end a basic block.
fn is_terminator(code: u64) -> bool {
    matches!(code, 10 | 11 | 12 | 13 | 15 | 31 | 39 | 55 | 56)
}

fn block_id(index: u64, count: usize) -> Result<BlockId, TranslateError> {
    let id = u32::try_from(index).map_err(|_| semantic("LLVM block index is too large"))?;
    if id as usize >= count {
        return Err(semantic("LLVM branch names a block which does not exist"));
    }
    Ok(BlockId(id))
}

/// Lower the AIR texture intrinsic family.
///
/// The operation name carries the access and the texture shape, for instance
/// `sample_texture_2d_array` or `read_texture_2d_ms`. Metal's optional
/// sampling arguments are passed as a present/absent flag followed by the
/// value; only a constant flag is accepted, because a runtime choice between
/// implicit and explicit level of detail has no SPIR-V form.
///
/// A `_grad` suffix is Metal's `gradient2d(dPdx, dPdy)`: the derivatives take
/// the place of the level-of-detail flag and value, and follow the array
/// index rather than preceding it.
fn air_texture_intrinsic(
    operation: &str,
    arguments: &[Operand],
) -> Result<Option<InstructionKind>, TranslateError> {
    let Some((access, shape)) = operation.split_once("_texture") else {
        return Ok(None);
    };
    let shape = shape.strip_prefix('_').unwrap_or(shape);
    let (shape, gradient) = match shape.strip_suffix("_grad") {
        Some(shape) => (shape, true),
        None => (shape, false),
    };
    let array = shape.ends_with("_array");
    let multisampled = shape.ends_with("_ms");
    let malformed = || {
        semantic(format!(
            "AIR intrinsic air.{operation} has unexpected operands"
        ))
    };
    match access {
        "sample" | "gather" => {
            // (texture, sampler, coordinate, [array index], offset flag,
            //  [offset], level flag, level). A cube texture takes no offset.
            let [texture, sampler, coordinate, rest @ ..] = arguments else {
                return Err(malformed());
            };
            let (array_index, rest) = if array {
                let [index, rest @ ..] = rest else {
                    return Err(malformed());
                };
                (Some(index.clone()), rest)
            } else {
                (None, rest)
            };
            let (gradient, rest) = match (gradient, rest) {
                (false, rest) => (None, rest),
                (true, [dx, dy, rest @ ..]) => (Some(Box::new((dx.clone(), dy.clone()))), rest),
                (true, _) => return Err(malformed()),
            };
            let (offset, rest) = match rest {
                [flag, offset, rest @ ..] if shape.starts_with("2d") || shape.starts_with("3d") => {
                    (optional(flag, offset)?, rest)
                }
                rest => (None, rest),
            };
            if access == "gather" {
                let [_component] = rest else {
                    return Err(malformed());
                };
                // FIXME (ACCURACY): the gathered component is a runtime
                // operand in AIR but a literal in SPIR-V. Lower the common
                // red-channel gather and reject any other selector.
                return Ok(Some(InstructionKind::TextureSample {
                    texture: texture.clone(),
                    sampler: sampler.clone(),
                    coordinate: coordinate.clone(),
                    array_index,
                    offset,
                    lod: None,
                    gradient: None,
                    gather: Some(0),
                }));
            }
            // A gradient sample carries no level-of-detail flag: the
            // derivatives are the level.
            let lod = match (gradient.is_some(), rest) {
                (true, []) => None,
                (false, [flag, lod]) => optional(flag, lod)?,
                _ => return Err(malformed()),
            };
            Ok(Some(InstructionKind::TextureSample {
                texture: texture.clone(),
                sampler: sampler.clone(),
                coordinate: coordinate.clone(),
                array_index,
                offset,
                lod,
                gradient,
                gather: None,
            }))
        }
        "read" => {
            // (texture, coordinate, [array index], level or sample index).
            let [texture, coordinate, rest @ ..] = arguments else {
                return Err(malformed());
            };
            let (array_index, rest) = if array {
                let [index, rest @ ..] = rest else {
                    return Err(malformed());
                };
                (Some(index.clone()), rest)
            } else {
                (None, rest)
            };
            let [selector] = rest else {
                return Err(malformed());
            };
            Ok(Some(InstructionKind::TextureRead {
                texture: texture.clone(),
                coordinate: coordinate.clone(),
                array_index,
                lod: (!multisampled).then(|| selector.clone()),
                sample: multisampled.then(|| selector.clone()),
            }))
        }
        "write" => {
            let [texture, coordinate, rest @ ..] = arguments else {
                return Err(malformed());
            };
            let (array_index, rest) = if array {
                let [index, rest @ ..] = rest else {
                    return Err(malformed());
                };
                (Some(index.clone()), rest)
            } else {
                (None, rest)
            };
            let [value, _level] = rest else {
                return Err(malformed());
            };
            Ok(Some(InstructionKind::TextureWrite {
                texture: texture.clone(),
                coordinate: coordinate.clone(),
                array_index,
                value: value.clone(),
            }))
        }
        "get_width" | "get_height" | "get_depth" => {
            let [texture, lod @ ..] = arguments else {
                return Err(malformed());
            };
            Ok(Some(InstructionKind::TextureQuerySize {
                texture: texture.clone(),
                lod: lod.first().cloned(),
                component: match access {
                    "get_width" => 0,
                    "get_height" => 1,
                    _ => 2,
                },
            }))
        }
        _ => Ok(None),
    }
}

/// AIR passes an optional sampling argument as a boolean flag and a value.
/// The flag must be a constant: SPIR-V chooses the sampling opcode at
/// translation time, not at run time.
fn optional(flag: &Operand, value: &Operand) -> Result<Option<Operand>, TranslateError> {
    match flag {
        Operand::Bool(false) => Ok(None),
        Operand::Bool(true) => Ok(Some(value.clone())),
        other => Err(semantic(format!(
            "AIR texture sampling option is not a constant flag: {other:?}"
        ))),
    }
}

/// A pointer whose LLVM type was widened by a bitcast. `pointee` is the type
/// the address really holds.
struct PointerAlias {
    pointee: usize,
    operand: Operand,
}

/// How a value loaded through a reinterpreted pointer is adapted to the type
/// the load asked for.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Adaptation {
    None,
    /// Metal pads a `float3` to sixteen bytes, and Apple's compiler reads
    /// such a member with a wider vector load.
    Widen,
    /// The same storage read as a different numeric type of equal width.
    Bitcast,
}

/// Decide how a pointer reinterpretation can be honoured without changing a
/// SPIR-V logical pointer's pointee type.
fn reinterpretation(
    types: &[DecodedType],
    from: usize,
    to: usize,
) -> Result<Adaptation, TranslateError> {
    if from == to {
        return Ok(Adaptation::None);
    }
    let shape = |index: usize| match &types[index] {
        DecodedType::Vector { element, lanes } => (*element, *lanes),
        _ => (index, 1),
    };
    let (source, source_lanes) = shape(from);
    let (target, target_lanes) = shape(to);
    if source == target && source_lanes < target_lanes {
        return Ok(Adaptation::Widen);
    }
    let width = |index: usize| match types[index] {
        DecodedType::Integer(bits) | DecodedType::Float(bits) => Some(bits),
        _ => None,
    };
    if source_lanes == target_lanes && width(source).is_some() && width(source) == width(target) {
        return Ok(Adaptation::Bitcast);
    }
    Err(semantic(format!(
        "LLVM pointer cast needs explicit buffer reinterpretation: {} -> {}",
        types[from].description(),
        types[to].description(),
    )))
}

/// Adapt a value loaded through a reinterpreted pointer to the requested type.
///
/// FIXME (ACCURACY): lanes added by a widening load are undefined rather than
/// the bytes Metal would read from a `float3`'s padding. Shaders discard
/// them, and SPIR-V's logical addressing cannot name that padding.
fn widen(
    value: Operand,
    from: usize,
    to: usize,
    types: &[DecodedType],
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
) -> Result<Operand, TranslateError> {
    let target = ir_type(to, types)?;
    match reinterpretation(types, from, to)? {
        Adaptation::None => Ok(value),
        Adaptation::Bitcast => Ok(emit(
            function,
            instructions,
            target,
            InstructionKind::Convert {
                op: ConvertOp::Bitcast,
                value,
            },
        )),
        Adaptation::Widen => {
            let lanes = |index: usize| match types[index] {
                DecodedType::Vector { lanes, .. } => lanes,
                _ => 1,
            };
            let components = (0..lanes(to))
                .map(|lane| if lane < lanes(from) { lane } else { u32::MAX })
                .collect();
            let source = ir_type(from, types)?;
            Ok(emit(
                function,
                instructions,
                target,
                InstructionKind::VectorShuffle {
                    first: value,
                    second: Operand::Undef(source),
                    components,
                },
            ))
        }
    }
}

/// Adapt a value to the type the address it is stored through really holds.
///
/// The mirror of [`widen`]: where a load of a padded `float3` reads four lanes
/// and drops one, a store of four writes the three the storage has.
fn narrow(
    value: Operand,
    from: usize,
    to: usize,
    types: &[DecodedType],
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
) -> Result<Operand, TranslateError> {
    if from == to {
        return Ok(value);
    }
    let target = ir_type(to, types)?;
    // `reinterpretation` is asked in the direction the storage was declared
    // in, which is the one whose widening rule holds.
    match reinterpretation(types, to, from)? {
        Adaptation::None => Ok(value),
        Adaptation::Bitcast => Ok(emit(
            function,
            instructions,
            target,
            InstructionKind::Convert {
                op: ConvertOp::Bitcast,
                value,
            },
        )),
        Adaptation::Widen => {
            let lanes = |index: usize| match types[index] {
                DecodedType::Vector { lanes, .. } => lanes,
                _ => 1,
            };
            // A shuffle produces a vector; storage of one lane is a scalar and
            // takes the first component instead.
            if lanes(to) == 1 {
                return Ok(emit(
                    function,
                    instructions,
                    target,
                    InstructionKind::CompositeExtract {
                        composite: value,
                        index: 0,
                    },
                ));
            }
            let components = (0..lanes(to)).collect();
            let source = ir_type(from, types)?;
            Ok(emit(
                function,
                instructions,
                target,
                InstructionKind::VectorShuffle {
                    first: value,
                    second: Operand::Undef(source),
                    components,
                },
            ))
        }
    }
}

/// What a lowered function body needs beyond its own records: its LLVM
/// signature, the names its calls resolve to, and the Metal types of any
/// opaque arguments.
pub(super) struct Signature<'a> {
    pub names: &'a HashMap<usize, String>,
    pub name: String,
    pub parameter_types: &'a [usize],
    pub result_type: usize,
    pub opaque_types: &'a HashMap<u32, Type>,
}

/// Record the Metal IR operand an LLVM value lowered to.
///
/// A value used before the record stream defined it already has a reserved
/// Metal IR value, because the use needed a type. Point the defining
/// instruction at that reserved value so both refer to one SSA definition.
fn define(
    forward: &mut HashMap<usize, (ValueId, usize)>,
    id: usize,
    value: Operand,
    operands: &mut HashMap<usize, Operand>,
    function: &Function,
    instructions: &mut [Instruction],
) -> Result<(), TranslateError> {
    let Some((reserved, _)) = forward.remove(&id) else {
        operands.insert(id, value);
        return Ok(());
    };
    let Operand::Value(defined) = value else {
        return Err(semantic(
            "LLVM forward reference names a value with no instruction",
        ));
    };
    if function.values.get(reserved.0 as usize) != function.values.get(defined.0 as usize) {
        return Err(semantic(
            "LLVM forward reference declares the wrong result type",
        ));
    }
    let instruction = instructions
        .iter_mut()
        .rev()
        .find(|instruction| instruction.result == Some(defined))
        .ok_or_else(|| semantic("LLVM forward reference has no defining instruction"))?;
    instruction.result = Some(reserved);
    operands.insert(id, Operand::Value(reserved));
    Ok(())
}
