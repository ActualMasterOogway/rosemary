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
}

impl ValueTable {
    pub(super) fn decode(
        module: &BitcodeBlock,
        body: &BitcodeBlock,
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
        };
        for record in module.elements.iter().filter_map(BitcodeElement::as_record) {
            match record.id {
                8 => {
                    let fields = record_values(record);
                    let ty =
                        type_reference(fields.get(if *version == 2 { 2 } else { 0 }), types.len())?;
                    table.values.push(Value { ty, constant: None });
                }
                // These also allocate value IDs. Reject until their pointer
                // types and initializers can be represented, never skip them.
                7 | 9 | 14 | 18 => {
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
            .elements
            .iter()
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
        let encoded =
            u32::try_from(encoded).map_err(|_| semantic("LLVM value reference exceeds 32 bits"))?;
        let next = u32::try_from(self.values.len())
            .map_err(|_| semantic("LLVM value table exceeds 32 bits"))?;
        let id = if self.relative {
            next.wrapping_sub(encoded)
        } else {
            encoded
        } as usize;
        if id >= self.values.len() {
            return Err(semantic(format!(
                "LLVM value reference {encoded} resolves to undefined value {id} at {next}"
            )));
        }
        Ok(id)
    }

    pub(super) fn lower_body(
        mut self,
        body: &BitcodeBlock,
        types: &[DecodedType],
        names: &HashMap<usize, String>,
        name: String,
        parameter_types: &[usize],
        result_type: usize,
    ) -> Result<Function, TranslateError> {
        let declarations = body
            .elements
            .iter()
            .filter_map(BitcodeElement::as_record)
            .filter(|r| r.id == 1)
            .count();
        if declarations != 1
            || !body
                .elements
                .iter()
                .filter_map(BitcodeElement::as_record)
                .next()
                .is_some_and(|r| r.id == 1)
        {
            return Err(semantic(
                "LLVM function must begin with one DECLAREBLOCKS record",
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
            let ty = ir_type(ty, types)?;
            let value = ValueId(function.values.len() as u32);
            function.values.push(ty.clone());
            function.parameters.push(Parameter {
                value,
                name: format!("arg{index}"),
                ty,
            });
            operands.insert(self.argument_start + index, Operand::Value(value));
        }
        let mut instructions = Vec::new();
        let mut terminator = None;
        let first_result = self.values.len();
        for record in body.elements.iter().filter_map(BitcodeElement::as_record) {
            let fields = record_values(record);
            if record.id == 1 {
                if fields != [1] {
                    return Err(semantic(
                        "LLVM body requires control-flow lowering for multiple blocks",
                    ));
                }
                continue;
            }
            if terminator.is_some() {
                return Err(semantic("LLVM instruction after terminator"));
            }
            let mut operand = |encoded| -> Result<(usize, Operand), TranslateError> {
                let id = self.operand_id(encoded)?;
                let op = self.materialize(
                    id,
                    types,
                    &mut function,
                    &mut instructions,
                    &mut operands,
                    &mut Vec::new(),
                )?;
                Ok((self.values[id].ty, op))
            };
            let (ty, kind) = match (record.id, fields.as_slice()) {
                (6, [vector, index]) => {
                    let (ty, composite) = operand(*vector)?;
                    let DecodedType::Vector { element, lanes } = types[ty] else {
                        return Err(semantic("extractelement requires vector"));
                    };
                    let index = self.integer(self.operand_id(*index)?, types)?;
                    if index >= u64::from(lanes) {
                        return Err(semantic("extractelement lane is out of range"));
                    }
                    (
                        element,
                        InstructionKind::CompositeExtract {
                            composite,
                            index: index as u32,
                        },
                    )
                }
                (7, [vector, object, index]) => {
                    let (ty, composite) = operand(*vector)?;
                    let (object_ty, object) = operand(*object)?;
                    let DecodedType::Vector { element, lanes } = types[ty] else {
                        return Err(semantic("insertelement requires vector"));
                    };
                    let index = self.integer(self.operand_id(*index)?, types)?;
                    if index >= u64::from(lanes) || object_ty != element {
                        return Err(semantic("insertelement lane or element type mismatch"));
                    }
                    (
                        ty,
                        InstructionKind::CompositeInsert {
                            composite,
                            object,
                            index: index as u32,
                        },
                    )
                }
                (8, [first, second, mask]) => {
                    let (ty, first) = operand(*first)?;
                    let (second_ty, second) = operand(*second)?;
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
                (3, [value, target, opcode @ (0 | 1)]) => {
                    let (source, value) = operand(*value)?;
                    let target = type_reference(Some(target), types.len())?;
                    let (DecodedType::Integer(from), DecodedType::Integer(to)) =
                        (&types[source], &types[target])
                    else {
                        return Err(semantic("LLVM integer cast requires scalar integers"));
                    };
                    if (*opcode == 0 && to >= from) || (*opcode == 1 && to <= from) {
                        return Err(semantic("LLVM integer cast has invalid widths"));
                    }
                    (target, InstructionKind::IntegerResize { value })
                }
                (30, [pointer, indices @ ..]) if !indices.is_empty() => {
                    let (pointer_ty, pointer) = operand(*pointer)?;
                    let DecodedType::Pointer {
                        mut pointee,
                        address_space,
                    } = types[pointer_ty]
                    else {
                        return Err(semantic("LLVM GEP requires pointer"));
                    };
                    let mut decoded = Vec::new();
                    for (position, &index) in indices.iter().enumerate() {
                        let (index_ty, value) = operand(index)?;
                        if !matches!(types[index_ty], DecodedType::Integer(_)) {
                            return Err(semantic("LLVM GEP index is not integer"));
                        }
                        if position > 0 {
                            pointee = match &types[pointee] {
                                DecodedType::Struct { fields, .. } => {
                                    let field = self.integer(self.operand_id(index)?, types)?;
                                    *fields
                                        .get(field as usize)
                                        .ok_or_else(|| semantic("LLVM GEP field out of bounds"))?
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
                (20, [pointer, alignment, volatile]) if *volatile == 0 && *alignment <= 32 => {
                    let (ty, pointer) = operand(*pointer)?;
                    let DecodedType::Pointer { pointee, .. } = types[ty] else {
                        return Err(semantic("LLVM load requires pointer"));
                    };
                    (pointee, InstructionKind::Load { pointer })
                }
                (2, [lhs, rhs, opcode] | [lhs, rhs, opcode, _]) => {
                    let (ty, lhs) = operand(*lhs)?;
                    let (rhs_ty, rhs) = operand(*rhs)?;
                    if ty != rhs_ty {
                        return Err(semantic("LLVM binary operand type mismatch"));
                    }
                    let scalar = match types[ty] {
                        DecodedType::Vector { element, .. } => &types[element],
                        _ => &types[ty],
                    };
                    let kind = match (scalar, opcode) {
                        (DecodedType::Float(_), 0) => InstructionKind::FloatAdd { lhs, rhs },
                        (DecodedType::Float(_), 1) => InstructionKind::FloatSubtract { lhs, rhs },
                        (DecodedType::Float(_), 2) => InstructionKind::FloatMultiply { lhs, rhs },
                        (DecodedType::Float(_), 4) => InstructionKind::FloatDiv { lhs, rhs },
                        _ => {
                            return Err(semantic(format!(
                                "unsupported LLVM binary opcode {opcode}"
                            )));
                        }
                    };
                    (ty, kind)
                }
                (34, [_, cc, callee, args @ ..]) if *cc == 1 => {
                    let callee = self.operand_id(*callee)?;
                    let signature = match types[self.values[callee].ty] {
                        DecodedType::Pointer { pointee, .. } => pointee,
                        _ => self.values[callee].ty,
                    };
                    let DecodedType::Function { result, parameters } = &types[signature] else {
                        return Err(semantic("LLVM call target is not a function"));
                    };
                    if args.len() != parameters.len() {
                        return Err(semantic("LLVM call arity mismatch"));
                    }
                    let mut actual = Vec::new();
                    for (&arg, &expected) in args.iter().zip(parameters) {
                        let (ty, value) = operand(arg)?;
                        if ty != expected {
                            return Err(semantic("LLVM call argument type mismatch"));
                        }
                        actual.push(value);
                    }
                    let intrinsic = names
                        .get(&callee)
                        .ok_or_else(|| semantic("LLVM call target has no name"))?;
                    let kind = match (intrinsic.as_str(), actual.as_slice()) {
                        ("air.convert.f.f32.u.i32", [value]) => InstructionKind::ConvertUToF {
                            value: value.clone(),
                        },
                        ("air.dot.v3f32" | "air.dot.v4f32", [lhs, rhs]) => {
                            InstructionKind::FloatDot {
                                lhs: lhs.clone(),
                                rhs: rhs.clone(),
                            }
                        }
                        ("air.fast_clamp.f32", [value, min, max]) => InstructionKind::FloatClamp {
                            value: value.clone(),
                            min: min.clone(),
                            max: max.clone(),
                        },
                        ("air.fast_fabs.f32", [value]) => InstructionKind::FloatAbs {
                            value: value.clone(),
                        },
                        ("air.fast_rsqrt.f32", [value]) => InstructionKind::FloatReciprocalSqrt {
                            value: value.clone(),
                        },
                        _ => {
                            return Err(semantic(format!(
                                "LLVM intrinsic {intrinsic} needs Metal IR lowering"
                            )));
                        }
                    };
                    (*result, kind)
                }
                (28, [lhs, rhs, 3]) => {
                    let (ty, lhs) = operand(*lhs)?;
                    let (rhs_ty, rhs) = operand(*rhs)?;
                    if ty != rhs_ty {
                        return Err(semantic("LLVM comparison type mismatch"));
                    }
                    let scalar = types
                        .iter()
                        .position(|t| matches!(t, DecodedType::Integer(1)))
                        .ok_or_else(|| semantic("LLVM comparison has no boolean type"))?;
                    let result = match types[ty] {
                        DecodedType::Float(_) => scalar,
                        DecodedType::Vector { element, lanes } if matches!(types[element], DecodedType::Float(_)) => types.iter().position(|t| matches!(t, DecodedType::Vector { element, lanes: n } if *element == scalar && *n == lanes)).ok_or_else(|| semantic("LLVM comparison has no boolean vector type"))?,
                        _ => return Err(semantic("LLVM floating comparison requires floating operands")),
                    };
                    (
                        result,
                        InstructionKind::FloatOrderedGreaterEqual { lhs, rhs },
                    )
                }
                (29, [on_true, on_false, condition]) => {
                    let (ty, on_true) = operand(*on_true)?;
                    let (false_ty, on_false) = operand(*on_false)?;
                    let (_, condition) = operand(*condition)?;
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
                (10, [value]) => {
                    let (ty, value) = operand(*value)?;
                    if ty != result_type {
                        return Err(semantic("LLVM return type mismatch"));
                    }
                    terminator = Some(Terminator::Return(Some(value)));
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
            operands.insert(self.values.len(), value);
            self.values.push(Value { ty, constant: None });
        }
        function.blocks.push(Block {
            id: BlockId(0),
            name: "entry".into(),
            instructions,
            terminator: terminator.ok_or_else(|| semantic("LLVM body has no terminator"))?,
        });
        Ok(function)
    }

    fn integer(&self, id: usize, types: &[DecodedType]) -> Result<u64, TranslateError> {
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

    fn materialize(
        &self,
        id: usize,
        types: &[DecodedType],
        function: &mut Function,
        instructions: &mut Vec<Instruction>,
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
                let mut composite = Operand::Undef(ty.clone());
                for (index, &id) in ids.iter().enumerate() {
                    let object =
                        self.materialize(id, types, function, instructions, operands, active)?;
                    composite = emit(
                        function,
                        instructions,
                        ty.clone(),
                        InstructionKind::CompositeInsert {
                            composite,
                            object,
                            index: index as u32,
                        },
                    );
                }
                composite
            }
            Constant::Data(data) => {
                let member_types = aggregate_types(&types[value.ty])?;
                let mut composite = Operand::Undef(ty.clone());
                for (index, (&bits, member)) in data.iter().zip(member_types).enumerate() {
                    let member = ir_type(member, types)?;
                    let object = match member {
                        Type::Int { .. } => Operand::Integer {
                            ty: member,
                            value: bits,
                        },
                        Type::Float { .. } => Operand::Float { ty: member, bits },
                        _ => return Err(semantic("LLVM DATA requires numeric scalar elements")),
                    };
                    composite = emit(
                        function,
                        instructions,
                        ty.clone(),
                        InstructionKind::CompositeInsert {
                            composite,
                            object,
                            index: index as u32,
                        },
                    );
                }
                composite
            }
            Constant::Null => zero_composite(&ty, function, instructions)?,
        };
        active.pop();
        operands.insert(id, result.clone());
        Ok(result)
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

fn zero_composite(
    ty: &Type,
    function: &mut Function,
    instructions: &mut Vec<Instruction>,
) -> Result<Operand, TranslateError> {
    let members = match ty {
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
        Type::Struct(s) => s.fields.iter().map(|f| &f.ty).collect(),
        _ => return Err(semantic("LLVM zero constant type needs lowering")),
    };
    let mut composite = Operand::Undef(ty.clone());
    for (index, member) in members.into_iter().enumerate() {
        let object = zero_composite(member, function, instructions)?;
        composite = emit(
            function,
            instructions,
            ty.clone(),
            InstructionKind::CompositeInsert {
                composite,
                object,
                index: index as u32,
            },
        );
    }
    Ok(composite)
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
                space: match address_space {
                    0 => AddressSpace::Thread,
                    1 => AddressSpace::Device,
                    2 => AddressSpace::Constant,
                    3 => AddressSpace::Threadgroup,
                    _ => return Err(semantic("unsupported AIR address space")),
                },
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

    // Encode owned, unabbreviated records to exercise the real bitstream
    // reader without an LLVM toolchain or proprietary shader fixtures.
    fn records_block(id: u32, records: &[(u64, &[u64])]) -> BitcodeBlock {
        fn fixed(bits: &mut Vec<bool>, value: u64, width: usize) {
            bits.extend((0..width).map(|bit| value & (1 << bit) != 0));
        }
        fn vbr(bits: &mut Vec<bool>, mut value: u64, width: usize) {
            loop {
                let mut chunk = value & ((1 << (width - 1)) - 1);
                value >>= width - 1;
                if value != 0 {
                    chunk |= 1 << (width - 1);
                }
                fixed(bits, chunk, width);
                if value == 0 {
                    break;
                }
            }
        }
        fn words(bits: &mut Vec<bool>) {
            while !bits.len().is_multiple_of(32) {
                bits.push(false);
            }
        }
        let mut body = Vec::new();
        for (code, fields) in records {
            fixed(&mut body, 3, 3);
            vbr(&mut body, *code, 6);
            vbr(&mut body, fields.len() as u64, 6);
            for field in *fields {
                vbr(&mut body, *field, 6);
            }
        }
        fixed(&mut body, 0, 3);
        words(&mut body);
        let mut bits = Vec::new();
        fixed(&mut bits, 1, 2);
        vbr(&mut bits, u64::from(id), 8);
        vbr(&mut bits, 3, 4);
        words(&mut bits);
        fixed(&mut bits, (body.len() / 32) as u64, 32);
        bits.extend(body);
        let mut bytes = b"BC\xc0\xde".to_vec();
        bytes.extend(bits.chunks(8).map(|chunk| {
            chunk
                .iter()
                .enumerate()
                .fold(0u8, |byte, (bit, set)| byte | (u8::from(*set) << bit))
        }));
        Bitcode::new(&bytes).unwrap().elements[0]
            .as_block()
            .unwrap()
            .clone()
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
            relative: true,
            values,
            argument_start: 69,
        };
        let body = records_block(12, &[(1, &[1]), (6, &[36, 98]), (10, &[1])]);
        let function = table
            .lower_body(&body, &types, &HashMap::new(), "extract".into(), &[1], 0)
            .unwrap();
        assert_eq!(
            function.blocks[0].instructions[0].kind,
            InstructionKind::CompositeExtract {
                composite: Operand::Value(ValueId(0)),
                index: 0
            }
        );
        Module {
            functions: vec![function],
            entries: Vec::new(),
        }
        .verify()
        .unwrap();
    }

    #[test]
    fn lowers_integer_resize_and_checks_its_direction() {
        let types = vec![DecodedType::Integer(32), DecodedType::Integer(64)];
        let make = || ValueTable {
            relative: true,
            values: vec![Value {
                ty: 0,
                constant: None,
            }],
            argument_start: 0,
        };
        let body = records_block(12, &[(1, &[1]), (3, &[1, 1, 1]), (10, &[1])]);
        let function = make()
            .lower_body(&body, &types, &HashMap::new(), "extend".into(), &[0], 1)
            .unwrap();
        Module {
            functions: vec![function],
            entries: Vec::new(),
        }
        .verify()
        .unwrap();
        let body = records_block(12, &[(1, &[1]), (3, &[1, 1, 0]), (10, &[1])]);
        assert!(
            make()
                .lower_body(&body, &types, &HashMap::new(), "bad".into(), &[0], 1)
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
            .lower_body(&body, &types, &HashMap::new(), "lane2".into(), &[1], 0)
            .unwrap();
        assert!(matches!(
            function.blocks[0].instructions[0].kind,
            InstructionKind::CompositeExtract { index: 2, .. }
        ));
        assert!(
            make(0, Constant::Integer(4))
                .lower_body(&body, &types, &HashMap::new(), "bad".into(), &[1], 0)
                .is_err()
        );
        assert!(
            make(2, Constant::Null)
                .lower_body(&body, &types, &HashMap::new(), "bad".into(), &[1], 0)
                .is_err()
        );
    }

    #[test]
    fn relative_references_include_module_constants() {
        let table = ValueTable {
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
