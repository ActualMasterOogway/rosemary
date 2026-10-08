//! Metal's standard library: math functions and texture methods.

use super::*;
use crate::ir::{
    self as ir, ConvertOp, FloatBinaryOp, FloatPredicate, FloatUnaryOp, IntegerBinaryOp,
    IntegerUnaryOp, TextureDimension, TextureType,
};

impl Lowerer<'_, '_> {
    pub fn call(&mut self, name: &str, template: Option<&Ty>, args: &[Expr], line: usize) -> Result<Value> {
        if name == "as_type" {
            let [arg] = args else {
                return Err(syntax_error(line, "as_type takes one argument"));
            };
            let target = template.ok_or_else(|| syntax_error(line, "as_type needs a type"))?;
            let value = self.rvalue(arg)?;
            return self.bitcast(value, target, line);
        }
        if matches!(name, "static_cast") {
            let [arg] = args else {
                return Err(syntax_error(line, "static_cast takes one argument"));
            };
            let target = template.ok_or_else(|| syntax_error(line, "static_cast needs a type"))?;
            let value = self.rvalue(arg)?;
            return self.convert(value, target, line);
        }
        let values = args
            .iter()
            .map(|arg| self.rvalue(arg))
            .collect::<Result<Vec<_>>>()?;
        let unary_float = |name: &str| -> Option<FloatUnaryOp> {
            Some(match name {
                "sqrt" => FloatUnaryOp::Sqrt,
                "rsqrt" => FloatUnaryOp::InverseSqrt,
                "exp" => FloatUnaryOp::Exp,
                "exp2" => FloatUnaryOp::Exp2,
                "log" => FloatUnaryOp::Log,
                "log2" => FloatUnaryOp::Log2,
                "sin" => FloatUnaryOp::Sin,
                "cos" => FloatUnaryOp::Cos,
                "tan" => FloatUnaryOp::Tan,
                "asin" => FloatUnaryOp::Asin,
                "acos" => FloatUnaryOp::Acos,
                "atan" => FloatUnaryOp::Atan,
                "floor" => FloatUnaryOp::Floor,
                "ceil" => FloatUnaryOp::Ceil,
                "round" => FloatUnaryOp::Round,
                "rint" => FloatUnaryOp::RoundEven,
                "trunc" => FloatUnaryOp::Truncate,
                "fract" => FloatUnaryOp::Fract,
                "dfdx" => FloatUnaryOp::DerivativeX,
                "dfdy" => FloatUnaryOp::DerivativeY,
                "fwidth" => FloatUnaryOp::DerivativeWidth,
                _ => return None,
            })
        };
        if let Some(op) = unary_float(name) {
            let [value] = values.as_slice() else {
                return Err(syntax_error(line, format!("{name} takes one argument")));
            };
            if matches!(op, FloatUnaryOp::DerivativeX | FloatUnaryOp::DerivativeY | FloatUnaryOp::DerivativeWidth)
                && self.stage != Stage::Fragment
            {
                return Err(syntax_error(line, format!("{name} outside a fragment function")));
            }
            let value = self.floating(value.clone(), line)?;
            return Ok(self.float_unary(op, value));
        }
        match (name, values.as_slice()) {
            ("abs", [value]) => {
                let value = value.clone();
                match value.ty.scalar() {
                    Some(Scalar::Float { .. }) => Ok(self.float_unary(FloatUnaryOp::Absolute, value)),
                    Some(Scalar::Int { signed: true, .. }) => Ok(self.int_unary(IntegerUnaryOp::AbsoluteSigned, value)),
                    Some(Scalar::Int { .. }) => Ok(value),
                    _ => Err(syntax_error(line, "abs of a non-numeric value")),
                }
            }
            ("sign", [value]) => {
                let value = value.clone();
                match value.ty.scalar() {
                    Some(Scalar::Float { .. }) => Ok(self.float_unary(FloatUnaryOp::Sign, value)),
                    Some(Scalar::Int { signed: true, .. }) => Ok(self.int_unary(IntegerUnaryOp::SignSigned, value)),
                    _ => Err(syntax_error(line, "sign of an unsupported type")),
                }
            }
            ("popcount", [value]) => {
                let value = self.integer(value.clone(), line)?;
                Ok(self.int_unary(IntegerUnaryOp::PopulationCount, value))
            }
            ("clz", [value]) => {
                let value = self.integer(value.clone(), line)?;
                Ok(self.int_unary(IntegerUnaryOp::CountLeadingZeros, value))
            }
            ("ctz", [value]) => {
                let value = self.integer(value.clone(), line)?;
                Ok(self.int_unary(IntegerUnaryOp::CountTrailingZeros, value))
            }
            ("saturate", [value]) => {
                let value = self.floating(value.clone(), line)?;
                let (scalar, lanes) = (value.ty.scalar().unwrap(), value.ty.lanes().unwrap());
                let zero = self.constant_of(scalar, lanes, Number::Float(0.0));
                let one = self.constant_of(scalar, lanes, Number::Float(1.0));
                let op = self.emit(
                    self.context.ir(&value.ty),
                    InstructionKind::FloatClamp {
                        value: value.op,
                        min: zero,
                        max: one,
                    },
                );
                Ok(Value { op, ty: value.ty })
            }
            ("min" | "max", [a, b]) => {
                let ty = self.common(&a.ty, &b.ty, line)?;
                let a = self.convert(a.clone(), &ty, line)?;
                let b = self.convert(b.clone(), &ty, line)?;
                let ir = self.context.ir(&ty);
                let max = name == "max";
                let op = match ty.scalar() {
                    Some(Scalar::Float { .. }) => self.emit(
                        ir,
                        InstructionKind::FloatBinary {
                            op: if max { FloatBinaryOp::Maximum } else { FloatBinaryOp::Minimum },
                            lhs: a.op,
                            rhs: b.op,
                        },
                    ),
                    Some(Scalar::Int { signed, .. }) => self.emit(
                        ir,
                        InstructionKind::IntegerBinary {
                            op: match (max, signed) {
                                (true, true) => IntegerBinaryOp::MaximumSigned,
                                (true, false) => IntegerBinaryOp::MaximumUnsigned,
                                (false, true) => IntegerBinaryOp::MinimumSigned,
                                (false, false) => IntegerBinaryOp::MinimumUnsigned,
                            },
                            lhs: a.op,
                            rhs: b.op,
                        },
                    ),
                    _ => return Err(syntax_error(line, format!("{name} of bools"))),
                };
                Ok(Value { op, ty })
            }
            ("clamp", [x, lo, hi]) => {
                let ty = self.common(&x.ty, &lo.ty, line)?;
                let ty = self.common(&ty, &hi.ty, line)?;
                let x = self.convert(x.clone(), &ty, line)?;
                let lo = self.convert(lo.clone(), &ty, line)?;
                let hi = self.convert(hi.clone(), &ty, line)?;
                let ir = self.context.ir(&ty);
                let op = match ty.scalar() {
                    Some(Scalar::Float { .. }) => self.emit(
                        ir,
                        InstructionKind::FloatClamp {
                            value: x.op,
                            min: lo.op,
                            max: hi.op,
                        },
                    ),
                    Some(Scalar::Int { signed, .. }) => self.emit(
                        ir,
                        InstructionKind::IntegerClamp {
                            signed,
                            value: x.op,
                            min: lo.op,
                            max: hi.op,
                        },
                    ),
                    _ => return Err(syntax_error(line, "clamp of bools")),
                };
                Ok(Value { op, ty })
            }
            ("fma" | "mix", [a, b, c]) => {
                let ty = self.float_common(&[a, b, c], line)?;
                let a = self.convert(a.clone(), &ty, line)?;
                let b = self.convert(b.clone(), &ty, line)?;
                let c = self.convert(c.clone(), &ty, line)?;
                let kind = if name == "fma" {
                    InstructionKind::FloatFma {
                        first: a.op,
                        second: b.op,
                        addend: c.op,
                    }
                } else {
                    InstructionKind::FloatMix {
                        first: a.op,
                        second: b.op,
                        weight: c.op,
                    }
                };
                let op = self.emit(self.context.ir(&ty), kind);
                Ok(Value { op, ty })
            }
            ("pow" | "powr" | "atan2" | "step" | "fmod", [a, b]) => {
                let ty = self.float_common(&[a, b], line)?;
                let a = self.convert(a.clone(), &ty, line)?;
                let b = self.convert(b.clone(), &ty, line)?;
                let op = match name {
                    "pow" | "powr" => FloatBinaryOp::Power,
                    "atan2" => FloatBinaryOp::Atan2,
                    "step" => FloatBinaryOp::Step,
                    _ => FloatBinaryOp::Remainder,
                };
                let op = self.emit(
                    self.context.ir(&ty),
                    InstructionKind::FloatBinary {
                        op,
                        lhs: a.op,
                        rhs: b.op,
                    },
                );
                Ok(Value { op, ty })
            }
            ("smoothstep", [e0, e1, x]) => {
                let ty = self.float_common(&[e0, e1, x], line)?;
                let e0 = self.convert(e0.clone(), &ty, line)?;
                let e1 = self.convert(e1.clone(), &ty, line)?;
                let x = self.convert(x.clone(), &ty, line)?;
                let span = self.arithmetic("-", e1, e0.clone(), line)?;
                let offset = self.arithmetic("-", x, e0, line)?;
                let ratio = self.arithmetic("/", offset, span, line)?;
                let (scalar, lanes) = (ty.scalar().unwrap(), ty.lanes().unwrap());
                let zero = self.constant_of(scalar, lanes, Number::Float(0.0));
                let one = self.constant_of(scalar, lanes, Number::Float(1.0));
                let t = Value {
                    op: self.emit(
                        self.context.ir(&ty),
                        InstructionKind::FloatClamp {
                            value: ratio.op,
                            min: zero,
                            max: one,
                        },
                    ),
                    ty: ty.clone(),
                };
                let two_t = self.arithmetic("*", t.clone(), self.float_constant(scalar, 2.0), line)?;
                let three = self.float_constant(scalar, 3.0);
                let tail = self.arithmetic("-", three, two_t, line)?;
                let square = self.arithmetic("*", t.clone(), t, line)?;
                self.arithmetic("*", square, tail, line)
            }
            ("dot", [a, b]) => {
                let ty = self.float_common(&[a, b], line)?;
                let Ty::Vector(scalar, lanes) = ty else {
                    // dot of scalars is their product.
                    let a = self.convert(a.clone(), &ty, line)?;
                    let b = self.convert(b.clone(), &ty, line)?;
                    return self.arithmetic("*", a, b, line);
                };
                // The IR's dot is single precision.
                let wide = Ty::Vector(Scalar::FLOAT, lanes);
                let a = self.convert(a.clone(), &wide, line)?;
                let b = self.convert(b.clone(), &wide, line)?;
                let op = self.emit(
                    Type::Float { bits: 32 },
                    InstructionKind::FloatDot {
                        lhs: a.op,
                        rhs: b.op,
                    },
                );
                self.convert(
                    Value {
                        op,
                        ty: Ty::Scalar(Scalar::FLOAT),
                    },
                    &Ty::Scalar(scalar),
                    line,
                )
            }
            ("length", [v]) => {
                let v = self.floating(v.clone(), line)?;
                if v.ty.lanes() == Some(1) {
                    return Ok(self.float_unary(FloatUnaryOp::Absolute, v));
                }
                let squared = self.call_values("dot", &[v.clone(), v], line)?;
                Ok(self.float_unary(FloatUnaryOp::Sqrt, squared))
            }
            ("distance", [a, b]) => {
                let ty = self.float_common(&[a, b], line)?;
                let a = self.convert(a.clone(), &ty, line)?;
                let b = self.convert(b.clone(), &ty, line)?;
                let difference = self.arithmetic("-", a, b, line)?;
                self.call_values("length", &[difference], line)
            }
            ("normalize", [v]) => {
                let v = self.floating(v.clone(), line)?;
                let squared = self.call_values("dot", &[v.clone(), v.clone()], line)?;
                let inverse = self.float_unary(FloatUnaryOp::InverseSqrt, squared);
                self.arithmetic("*", v, inverse, line)
            }
            ("cross", [a, b]) => {
                let ty = self.float_common(&[a, b], line)?;
                if ty.lanes() != Some(3) {
                    return Err(syntax_error(line, "cross of vectors not of three lanes"));
                }
                let a = self.convert(a.clone(), &ty, line)?;
                let b = self.convert(b.clone(), &ty, line)?;
                let a1 = self.swizzle(a.clone(), &[1, 2, 0], line)?;
                let b1 = self.swizzle(b.clone(), &[2, 0, 1], line)?;
                let a2 = self.swizzle(a, &[2, 0, 1], line)?;
                let b2 = self.swizzle(b, &[1, 2, 0], line)?;
                let first = self.arithmetic("*", a1, b1, line)?;
                let second = self.arithmetic("*", a2, b2, line)?;
                self.arithmetic("-", first, second, line)
            }
            ("reflect", [i, n]) => {
                let ty = self.float_common(&[i, n], line)?;
                let i = self.convert(i.clone(), &ty, line)?;
                let n = self.convert(n.clone(), &ty, line)?;
                let d = self.call_values("dot", &[n.clone(), i.clone()], line)?;
                let scalar = ty.scalar().unwrap();
                let twice = self.arithmetic("*", d, self.float_constant(scalar, 2.0), line)?;
                let scaled = self.arithmetic("*", n, twice, line)?;
                self.arithmetic("-", i, scaled, line)
            }
            ("select", [a, b, c]) => {
                let ty = if a.ty == b.ty { a.ty.clone() } else { self.common(&a.ty, &b.ty, line)? };
                let a = self.convert(a.clone(), &ty, line)?;
                let b = self.convert(b.clone(), &ty, line)?;
                let lanes = c.ty.lanes().ok_or_else(|| syntax_error(line, "select's condition"))?;
                if lanes != 1 && Some(lanes) != ty.lanes() {
                    return Err(syntax_error(line, "select's condition of the wrong shape"));
                }
                let c = self.convert(c.clone(), &Ty::of(Scalar::Bool, lanes), line)?;
                let op = self.emit(
                    self.context.ir(&ty),
                    InstructionKind::Select {
                        condition: c.op,
                        on_true: b.op,
                        on_false: a.op,
                    },
                );
                Ok(Value { op, ty })
            }
            ("all" | "any", [v]) => {
                let lanes = v.ty.lanes().ok_or_else(|| syntax_error(line, format!("{name} of a non-vector")))?;
                let v = self.convert(v.clone(), &Ty::of(Scalar::Bool, lanes), line)?;
                if lanes == 1 {
                    return Ok(v);
                }
                let kind = if name == "all" {
                    InstructionKind::All { vector: v.op }
                } else {
                    InstructionKind::Any { vector: v.op }
                };
                let op = self.emit(Type::Bool, kind);
                Ok(Value {
                    op,
                    ty: Ty::Scalar(Scalar::Bool),
                })
            }
            ("isnan" | "isinf" | "isfinite", [v]) => {
                let v = self.floating(v.clone(), line)?;
                let lanes = v.ty.lanes().unwrap();
                let bool_ty = Ty::of(Scalar::Bool, lanes);
                let (lhs, rhs, predicate) = match name {
                    "isnan" => (v.op.clone(), v.op, FloatPredicate::Unordered),
                    _ => {
                        let magnitude = self.float_unary(FloatUnaryOp::Absolute, v.clone());
                        let infinity = self.constant_of(v.ty.scalar().unwrap(), lanes, Number::Float(f64::INFINITY));
                        let predicate = if name == "isinf" {
                            FloatPredicate::OrderedEqual
                        } else {
                            FloatPredicate::OrderedLessThan
                        };
                        (magnitude.op, infinity, predicate)
                    }
                };
                let op = self.emit(
                    self.context.ir(&bool_ty),
                    InstructionKind::FloatCompare { predicate, lhs, rhs },
                );
                Ok(Value { op, ty: bool_ty })
            }
            _ => Err(syntax_error(
                line,
                format!("unsupported function {name} with {} argument(s)", args.len()),
            )),
        }
    }

    fn call_values(&mut self, name: &str, values: &[Value], line: usize) -> Result<Value> {
        // Re-enter `call` with values already evaluated, by binding them to
        // names nothing in the source can spell.
        self.scopes.push(HashMap::new());
        let args = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let name = format!(" arg{index}");
                self.bind(&name, Binding::Value(value.clone()));
                Expr {
                    kind: ExprKind::Ident(name),
                    line,
                }
            })
            .collect::<Vec<_>>();
        let result = self.call(name, None, &args, line);
        self.scopes.pop();
        result
    }

    fn float_constant(&self, scalar: Scalar, value: f64) -> Value {
        Value {
            op: number_operand(scalar, Number::Float(value)),
            ty: Ty::Scalar(scalar),
        }
    }

    /// A value as floating point: an integer argument to a float function is
    /// converted, as C++ overload resolution would.
    fn floating(&mut self, value: Value, line: usize) -> Result<Value> {
        match value.ty {
            Ty::Scalar(Scalar::Float { .. }) | Ty::Vector(Scalar::Float { .. }, _) => Ok(value),
            Ty::Scalar(_) => self.convert(value, &Ty::Scalar(Scalar::FLOAT), line),
            Ty::Vector(_, lanes) => self.convert(value, &Ty::Vector(Scalar::FLOAT, lanes), line),
            _ => Err(syntax_error(line, "a math function of a non-numeric value")),
        }
    }

    fn integer(&mut self, value: Value, line: usize) -> Result<Value> {
        if value.ty.scalar().is_some_and(Scalar::is_integer) {
            Ok(value)
        } else {
            Err(syntax_error(line, "an integer function of a non-integer"))
        }
    }

    /// The floating type several arguments meet at.
    fn float_common(&self, values: &[&Value], line: usize) -> Result<Ty> {
        let mut ty = values[0].ty.clone();
        for value in &values[1..] {
            ty = self.common(&ty, &value.ty, line)?;
        }
        Ok(match ty {
            Ty::Scalar(Scalar::Float { .. }) | Ty::Vector(Scalar::Float { .. }, _) => ty,
            Ty::Scalar(_) => Ty::Scalar(Scalar::FLOAT),
            Ty::Vector(_, lanes) => Ty::Vector(Scalar::FLOAT, lanes),
            _ => return Err(syntax_error(line, "a math function of a non-numeric value")),
        })
    }

    fn float_unary(&mut self, op: FloatUnaryOp, value: Value) -> Value {
        let result = self.emit(
            self.context.ir(&value.ty),
            InstructionKind::FloatUnary { op, value: value.op },
        );
        Value {
            op: result,
            ty: value.ty,
        }
    }

    fn int_unary(&mut self, op: IntegerUnaryOp, value: Value) -> Value {
        let result = self.emit(
            self.context.ir(&value.ty),
            InstructionKind::IntegerUnary { op, value: value.op },
        );
        Value {
            op: result,
            ty: value.ty,
        }
    }

    /// `as_type<T>(x)`: the same bits as another type of the same size.
    fn bitcast(&mut self, value: Value, target: &Ty, line: usize) -> Result<Value> {
        let bits = |ty: &Ty| Some(u32::from(ty.scalar()?.bits()) * u32::from(ty.lanes()?));
        if bits(&value.ty).is_none() || bits(&value.ty) != bits(target) {
            return Err(syntax_error(line, "as_type between types of different sizes"));
        }
        let (from, to) = (self.context.ir(&value.ty), self.context.ir(target));
        if from == to {
            return Ok(Value {
                op: value.op,
                ty: target.clone(),
            });
        }
        let op = self.emit(
            to,
            InstructionKind::Convert {
                op: ConvertOp::Bitcast,
                value: value.op,
            },
        );
        Ok(Value {
            op,
            ty: target.clone(),
        })
    }

    /// `texture.method(...)`.
    pub fn texture_method(&mut self, expr: &Expr) -> Result<Value> {
        let line = expr.line;
        let ExprKind::Method { object, name, args } = &expr.kind else {
            unreachable!("a method call");
        };
        let texture = self.rvalue(object)?;
        let Ty::Texture(texture_type) = texture.ty.clone() else {
            return Err(syntax_error(line, format!("method {name} of something not a texture")));
        };
        match name.as_str() {
            "sample" | "sample_compare" | "gather" => {
                self.sample(texture.op, &texture_type, name == "sample_compare", name == "gather", args, line)
            }
            "read" => self.read(texture.op, &texture_type, args, line),
            "write" => {
                self.write(texture.op, &texture_type, args, line)?;
                Ok(Value {
                    op: Operand::Bool(false),
                    ty: Ty::Void,
                })
            }
            "get_width" | "get_height" | "get_depth" => {
                let component = match name.as_str() {
                    "get_width" => 0,
                    "get_height" => 1,
                    _ => 2,
                };
                let lod = match args.as_slice() {
                    [] => None,
                    [lod] => {
                        let value = self.rvalue(lod)?;
                        Some(self.convert(value, &Ty::Scalar(Scalar::UINT), line)?.op)
                    }
                    _ => return Err(syntax_error(line, format!("{name} takes at most a level"))),
                };
                let op = self.emit(
                    Type::u32(),
                    InstructionKind::TextureQuerySize {
                        texture: texture.op,
                        lod,
                        component,
                    },
                );
                Ok(Value {
                    op,
                    ty: Ty::Scalar(Scalar::UINT),
                })
            }
            _ => Err(syntax_error(line, format!("unsupported texture method {name}"))),
        }
    }

    fn coordinate(&mut self, texture: &TextureType, arg: &Expr, scalar: Scalar, line: usize) -> Result<Operand> {
        let lanes = ir::coordinate_lanes(texture);
        let value = self.rvalue(arg)?;
        if value.ty.lanes() != Some(lanes) {
            return Err(syntax_error(line, "a texture coordinate of the wrong length"));
        }
        Ok(self.convert(value, &Ty::of(scalar, lanes), line)?.op)
    }

    fn sample(
        &mut self,
        texture: Operand,
        texture_type: &TextureType,
        compare: bool,
        gather: bool,
        args: &[Expr],
        line: usize,
    ) -> Result<Value> {
        if texture_type.access != Access::Sample {
            return Err(syntax_error(line, "sampling a texture without access::sample"));
        }
        let mut args = args.iter();
        let sampler = self.rvalue(args.next().ok_or_else(|| syntax_error(line, "sample needs a sampler"))?)?;
        if sampler.ty != Ty::Sampler {
            return Err(syntax_error(line, "sample's first argument must be a sampler"));
        }
        let coordinate = self.coordinate(
            texture_type,
            args.next().ok_or_else(|| syntax_error(line, "sample needs a coordinate"))?,
            Scalar::FLOAT,
            line,
        )?;
        let array_index = if texture_type.array {
            let index = self.rvalue(args.next().ok_or_else(|| syntax_error(line, "an array sample needs a slice"))?)?;
            Some(self.convert(index, &Ty::Scalar(Scalar::UINT), line)?.op)
        } else {
            None
        };
        let reference = if compare {
            let value = self.rvalue(args.next().ok_or_else(|| syntax_error(line, "sample_compare needs a reference"))?)?;
            Some(self.convert(value, &Ty::Scalar(Scalar::FLOAT), line)?.op)
        } else {
            None
        };
        let (mut lod, mut bias, mut gradient, mut offset, mut component) = (None, None, None, None, None);
        for arg in args {
            match &arg.kind {
                ExprKind::Call { name, args: inner, .. } if name == "level" || name == "bias" => {
                    let [value] = inner.as_slice() else {
                        return Err(syntax_error(line, format!("{name}() takes one value")));
                    };
                    let value = self.rvalue(value)?;
                    let value = self.convert(value, &Ty::Scalar(Scalar::FLOAT), line)?.op;
                    if name == "level" {
                        lod = Some(value);
                    } else {
                        bias = Some(value);
                    }
                }
                ExprKind::Call { name, args: inner, .. } if name.starts_with("gradient") => {
                    let [dx, dy] = inner.as_slice() else {
                        return Err(syntax_error(line, format!("{name}() takes two values")));
                    };
                    let lanes = match texture_type.dimension {
                        TextureDimension::D1 => 1,
                        TextureDimension::D2 => 2,
                        _ => 3,
                    };
                    let ty = Ty::of(Scalar::FLOAT, lanes);
                    let dx = self.rvalue(dx)?;
                    let dx = self.convert(dx, &ty, line)?.op;
                    let dy = self.rvalue(dy)?;
                    let dy = self.convert(dy, &ty, line)?.op;
                    gradient = Some(Box::new((dx, dy)));
                }
                ExprKind::Call { name, .. } if name == "min_lod_clamp" => {
                    return Err(syntax_error(line, "min_lod_clamp is not supported"));
                }
                ExprKind::Ident(name) if gather && name.starts_with("component") => {
                    return Err(syntax_error(line, "a gather component is not supported"));
                }
                _ if gather && component.is_none() && offset.is_some() => {
                    component = Some(self.context.const_index(arg)?);
                }
                _ => {
                    let value = self.rvalue(arg)?;
                    let lanes = value.ty.lanes().unwrap_or(0);
                    let value = self.convert(value, &Ty::of(Scalar::INT, lanes), line)?;
                    if !is_constant_operand(&value.op) {
                        return Err(syntax_error(line, "a sampling offset must be constant"));
                    }
                    offset = Some(value.op);
                }
            }
        }
        if lod.is_none() && gradient.is_none() && self.stage != Stage::Fragment && !gather {
            return Err(syntax_error(line, "an implicit level of detail outside a fragment function"));
        }
        let texel = Ty::of(
            match *texture_type.texel {
                Type::Float { bits } => Scalar::Float { bits },
                Type::Int { bits, .. } => Scalar::Int {
                    signed: texture_type.signed,
                    bits,
                },
                _ => return Err(syntax_error(line, "an unsupported texel type")),
            },
            4,
        );
        let result_ty = if compare {
            Ty::Scalar(Scalar::FLOAT)
        } else {
            texel.clone()
        };
        let op = self.emit(
            self.context.ir(&result_ty),
            InstructionKind::TextureSample {
                texture,
                sampler: sampler.op,
                coordinate,
                array_index,
                offset,
                lod,
                gradient,
                bias,
                gather: if gather { Some(component.unwrap_or(0)) } else { None },
                compare: reference,
            },
        );
        let value = Value { op, ty: result_ty };
        // A depth texture samples as one value.
        if texture_type.depth && !compare && !gather {
            return self.swizzle(value, &[0], line);
        }
        Ok(value)
    }

    fn read(&mut self, texture: Operand, texture_type: &TextureType, args: &[Expr], line: usize) -> Result<Value> {
        let mut args = args.iter();
        let coordinate = self.coordinate(
            texture_type,
            args.next().ok_or_else(|| syntax_error(line, "read needs a coordinate"))?,
            Scalar::UINT,
            line,
        )?;
        let mut next_uint = |lowerer: &mut Self, what: &str| -> Result<Option<Operand>> {
            match args.next() {
                Some(arg) => {
                    let value = lowerer.rvalue(arg)?;
                    let _ = what;
                    Ok(Some(lowerer.convert(value, &Ty::Scalar(Scalar::UINT), line)?.op))
                }
                None => Ok(None),
            }
        };
        let array_index = if texture_type.array { next_uint(self, "slice")? } else { None };
        let (sample, lod) = if texture_type.multisampled {
            (next_uint(self, "sample")?, None)
        } else {
            (None, next_uint(self, "level")?)
        };
        let texel = Ty::of(
            match *texture_type.texel {
                Type::Float { bits } => Scalar::Float { bits },
                Type::Int { bits, .. } => Scalar::Int {
                    signed: texture_type.signed,
                    bits,
                },
                _ => return Err(syntax_error(line, "an unsupported texel type")),
            },
            4,
        );
        let op = self.emit(
            self.context.ir(&texel),
            InstructionKind::TextureRead {
                texture,
                coordinate,
                array_index,
                lod,
                sample,
            },
        );
        let value = Value { op, ty: texel };
        if texture_type.depth {
            return self.swizzle(value, &[0], line);
        }
        Ok(value)
    }

    fn write(&mut self, texture: Operand, texture_type: &TextureType, args: &[Expr], line: usize) -> Result<()> {
        if texture_type.access == Access::Sample || texture_type.access == Access::Read {
            return Err(syntax_error(line, "writing a texture without write access"));
        }
        let [value, coordinate, rest @ ..] = args else {
            return Err(syntax_error(line, "write needs a value and a coordinate"));
        };
        let texel = Ty::of(
            match *texture_type.texel {
                Type::Float { bits } => Scalar::Float { bits },
                Type::Int { bits, .. } => Scalar::Int {
                    signed: texture_type.signed,
                    bits,
                },
                _ => return Err(syntax_error(line, "an unsupported texel type")),
            },
            4,
        );
        let value = self.rvalue(value)?;
        let value = self.convert(value, &texel, line)?.op;
        let coordinate = self.coordinate(texture_type, coordinate, Scalar::UINT, line)?;
        let array_index = match (texture_type.array, rest) {
            (true, [slice, ..]) => {
                let slice = self.rvalue(slice)?;
                Some(self.convert(slice, &Ty::Scalar(Scalar::UINT), line)?.op)
            }
            (true, []) => return Err(syntax_error(line, "an array write needs a slice")),
            (false, []) => None,
            (false, [level]) if matches!(self.context.const_number(level), Some(Number::Int(0))) => None,
            _ => return Err(syntax_error(line, "a write to a level other than zero")),
        };
        self.emit_void(InstructionKind::TextureWrite {
            texture,
            coordinate,
            array_index,
            value,
        });
        Ok(())
    }
}

fn is_constant_operand(op: &Operand) -> bool {
    super::expr::is_constant(op)
}
