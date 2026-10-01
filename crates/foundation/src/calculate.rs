//! Calculate.framework's `CalculateExpression`, the expression evaluator
//! Spotlight and Calculator's programmer mode hand a line of text to.
//!
//! The grammar is the one Calculator writes: numbers, the infix operators
//! `+ - * / % ^`, `AND OR XOR NOR << >>`, parentheses, and named functions
//! called as `name(x)` or `name(x, y)`. With [`flags::INTEGER`] every value
//! is an unsigned 64-bit integer and arithmetic wraps, as the programmer
//! keypad's does; otherwise values are doubles.
//!
//! The private framework is undocumented; the entry point's shape is the
//! caller's: `Boolean CalculateExpression(const char *expression,
//! int precision, int flags, char *answer, size_t length)`.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_int};

mod flags {
    /// Unsigned 64-bit integer arithmetic.
    pub const INTEGER: i32 = 0x2;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Value {
    Integer(u64),
    Real(f64),
}

impl Value {
    fn real(self) -> f64 {
        match self {
            Value::Integer(v) => v as f64,
            Value::Real(v) => v,
        }
    }

    fn integer(self) -> u64 {
        match self {
            Value::Integer(v) => v,
            // Negative values wrap, as a 64-bit register holds them.
            Value::Real(v) if v < 0.0 => (v as i64) as u64,
            Value::Real(v) => v as u64,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(String),
    Word(String),
    Symbol(&'static str),
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    const SYMBOLS: [&str; 11] = ["<<", ">>", "+", "-", "*", "/", "%", "^", "(", ")", ","];
    let mut tokens = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
            continue;
        }
        if let Some(symbol) = SYMBOLS.iter().find(|s| rest.starts_with(**s)) {
            tokens.push(Token::Symbol(symbol));
            rest = &rest[symbol.len()..];
            continue;
        }
        if c.is_ascii_digit() || c == '.' {
            let hex = rest.starts_with("0x") || rest.starts_with("0X");
            let mut end = if hex { 2 } else { 0 };
            let bytes = rest.as_bytes();
            while end < bytes.len() {
                let b = bytes[end];
                let exponent_sign = !hex
                    && (b == b'+' || b == b'-')
                    && end > 0
                    && matches!(bytes[end - 1], b'e' | b'E');
                let ok = match hex {
                    true => b.is_ascii_hexdigit(),
                    false => {
                        b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || exponent_sign
                    }
                };
                if !ok {
                    break;
                }
                end += 1;
            }
            tokens.push(Token::Number(rest[..end].to_string()));
            rest = &rest[end..];
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let end = rest
                .char_indices()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                .map_or(rest.len(), |(at, _)| at);
            tokens.push(Token::Word(rest[..end].to_string()));
            rest = &rest[end..];
            continue;
        }
        if c == '!' {
            tokens.push(Token::Word("!".to_string()));
            rest = &rest[1..];
            continue;
        }
        return Err(format!("unexpected {c:?}"));
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    integer: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(s)) if *s == symbol) {
            self.at += 1;
            return true;
        }
        false
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(word)) {
            self.at += 1;
            return true;
        }
        false
    }

    /// Lowest precedence: `OR`, `NOR` and `XOR`.
    fn or(&mut self) -> Result<Value, String> {
        let mut left = self.and()?;
        loop {
            let op = if self.eat_word("OR") {
                "OR"
            } else if self.eat_word("NOR") {
                "NOR"
            } else if self.eat_word("XOR") {
                "XOR"
            } else {
                return Ok(left);
            };
            let right = self.and()?;
            let (a, b) = (left.integer(), right.integer());
            left = Value::Integer(match op {
                "OR" => a | b,
                "NOR" => !(a | b),
                _ => a ^ b,
            });
        }
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut left = self.shift()?;
        while self.eat_word("AND") {
            let right = self.shift()?;
            left = Value::Integer(left.integer() & right.integer());
        }
        Ok(left)
    }

    fn shift(&mut self) -> Result<Value, String> {
        let mut left = self.sum()?;
        loop {
            let left_shift = if self.eat_symbol("<<") {
                true
            } else if self.eat_symbol(">>") {
                false
            } else {
                return Ok(left);
            };
            let by = self.sum()?.integer();
            let value = left.integer();
            left = Value::Integer(match (left_shift, by >= 64) {
                (_, true) => 0,
                (true, false) => value << by,
                (false, false) => value >> by,
            });
        }
    }

    fn sum(&mut self) -> Result<Value, String> {
        let mut left = self.product()?;
        loop {
            let add = if self.eat_symbol("+") {
                true
            } else if self.eat_symbol("-") {
                false
            } else {
                return Ok(left);
            };
            let right = self.product()?;
            left = match self.integer {
                true => Value::Integer(match add {
                    true => left.integer().wrapping_add(right.integer()),
                    false => left.integer().wrapping_sub(right.integer()),
                }),
                false => Value::Real(match add {
                    true => left.real() + right.real(),
                    false => left.real() - right.real(),
                }),
            };
        }
    }

    fn product(&mut self) -> Result<Value, String> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat_symbol("*") {
                '*'
            } else if self.eat_symbol("/") {
                '/'
            } else if self.eat_symbol("%") {
                '%'
            } else {
                return Ok(left);
            };
            let right = self.unary()?;
            left = match self.integer {
                true => {
                    let (a, b) = (left.integer(), right.integer());
                    if op != '*' && b == 0 {
                        return Err("divide by zero".into());
                    }
                    Value::Integer(match op {
                        '*' => a.wrapping_mul(b),
                        '/' => a / b,
                        _ => a % b,
                    })
                }
                false => {
                    let (a, b) = (left.real(), right.real());
                    if op != '*' && b == 0.0 {
                        return Err("divide by zero".into());
                    }
                    Value::Real(match op {
                        '*' => a * b,
                        '/' => a / b,
                        _ => a % b,
                    })
                }
            };
        }
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.eat_symbol("-") {
            let value = self.unary()?;
            return Ok(match value {
                Value::Integer(v) => Value::Integer(v.wrapping_neg()),
                Value::Real(v) => Value::Real(-v),
            });
        }
        if self.eat_symbol("+") {
            return self.unary();
        }
        self.power()
    }

    /// `^`, right associative.
    fn power(&mut self) -> Result<Value, String> {
        let base = self.postfix()?;
        if !self.eat_symbol("^") {
            return Ok(base);
        }
        let exponent = self.unary()?;
        Ok(match self.integer {
            true => Value::Integer(
                base.integer()
                    .wrapping_pow(exponent.integer().min(u32::MAX as u64) as u32),
            ),
            false => Value::Real(base.real().powf(exponent.real())),
        })
    }

    fn postfix(&mut self) -> Result<Value, String> {
        let mut value = self.primary()?;
        while self.eat_word("!") {
            value = factorial(value, self.integer)?;
        }
        Ok(value)
    }

    fn primary(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Token::Number(text)) => self.number(&text),
            Some(Token::Symbol("(")) => {
                let value = self.or()?;
                if !self.eat_symbol(")") {
                    return Err("missing )".into());
                }
                Ok(value)
            }
            Some(Token::Word(name)) => {
                if self.eat_symbol("(") {
                    let mut arguments = vec![self.or()?];
                    while self.eat_symbol(",") {
                        arguments.push(self.or()?);
                    }
                    if !self.eat_symbol(")") {
                        return Err("missing )".into());
                    }
                    return call(&name, &arguments, self.integer);
                }
                constant(&name, self.integer)
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }

    fn number(&self, text: &str) -> Result<Value, String> {
        if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            return u64::from_str_radix(hex, 16)
                .map(Value::Integer)
                .map_err(|e| e.to_string());
        }
        match self.integer {
            // A fraction typed in integer mode is cut to its whole part.
            true => match text.parse::<u64>() {
                Ok(v) => Ok(Value::Integer(v)),
                Err(_) => text
                    .parse::<f64>()
                    .map(|v| Value::Integer(v as u64))
                    .map_err(|e| e.to_string()),
            },
            false => text
                .parse::<f64>()
                .map(Value::Real)
                .map_err(|e| e.to_string()),
        }
    }
}

fn factorial(value: Value, integer: bool) -> Result<Value, String> {
    let n = value.real();
    if n < 0.0 || n.fract() != 0.0 {
        return Err("factorial of a non-integer".into());
    }
    match integer {
        true => Ok(Value::Integer(
            (1..=value.integer()).fold(1u64, |a, b| a.wrapping_mul(b)),
        )),
        false => {
            let mut product = 1.0f64;
            let mut k = 2.0;
            while k <= n {
                product *= k;
                if product.is_infinite() {
                    break;
                }
                k += 1.0;
            }
            Ok(Value::Real(product))
        }
    }
}

fn constant(name: &str, integer: bool) -> Result<Value, String> {
    let value = match name.to_ascii_lowercase().as_str() {
        "pi" => std::f64::consts::PI,
        "e" => std::f64::consts::E,
        _ => return Err(format!("unknown name {name:?}")),
    };
    Ok(match integer {
        true => Value::Integer(value as u64),
        false => Value::Real(value),
    })
}

fn call(name: &str, arguments: &[Value], integer: bool) -> Result<Value, String> {
    let one = |f: fn(f64) -> f64| -> Result<Value, String> {
        match arguments {
            [x] => Ok(Value::Real(f(x.real()))),
            _ => Err(format!("{name} takes one argument")),
        }
    };
    let bits = |f: fn(u64) -> u64| -> Result<Value, String> {
        match arguments {
            [x] => Ok(Value::Integer(f(x.integer()))),
            _ => Err(format!("{name} takes one argument")),
        }
    };
    let result = match name.to_ascii_lowercase().as_str() {
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        "asin" => one(f64::asin),
        "acos" => one(f64::acos),
        "atan" => one(f64::atan),
        "sinh" => one(f64::sinh),
        "cosh" => one(f64::cosh),
        "tanh" => one(f64::tanh),
        "asinh" => one(f64::asinh),
        "acosh" => one(f64::acosh),
        "atanh" => one(f64::atanh),
        "ln" => one(f64::ln),
        "log" | "log10" => one(f64::log10),
        "log2" => one(f64::log2),
        "exp" => one(f64::exp),
        "sqrt" => one(f64::sqrt),
        "abs" => one(f64::abs),
        "fact" => match arguments {
            [x] => factorial(*x, integer),
            _ => Err("fact takes one argument".into()),
        },
        "rol" => bits(|x| x.rotate_left(1)),
        "ror" => bits(|x| x.rotate_right(1)),
        "ones" => bits(|x| !x),
        "twos" => bits(u64::wrapping_neg),
        _ => Err(format!("unknown function {name:?}")),
    }?;
    Ok(match (integer, result) {
        (true, Value::Real(v)) => Value::Integer(Value::Real(v).integer()),
        (_, value) => value,
    })
}

/// Evaluate `text` as Calculate.framework does.
fn evaluate(text: &str, flags: i32) -> Result<Value, String> {
    let integer = flags & flags::INTEGER != 0;
    let mut parser = Parser {
        tokens: tokenize(text)?,
        at: 0,
        integer,
    };
    let value = parser.or()?;
    if parser.at != parser.tokens.len() {
        return Err(format!("unexpected {:?}", parser.peek()));
    }
    Ok(value)
}

/// `value` with at most `precision` significant digits, in plain notation
/// where that stays short and in scientific notation where it would not.
fn format(value: Value, precision: i32) -> String {
    match value {
        Value::Integer(v) => v.to_string(),
        Value::Real(v) if v.is_nan() => "NaN".to_string(),
        Value::Real(v) if v.is_infinite() => {
            if v > 0.0 {
                "Inf".to_string()
            } else {
                "-Inf".to_string()
            }
        }
        Value::Real(v) => {
            let digits = precision.clamp(1, 17) as usize;
            let scientific = format!("{:.*e}", digits - 1, v);
            let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
            let exponent: i32 = exponent.parse().unwrap_or(0);
            if exponent < -5 || exponent >= digits as i32 {
                let mantissa = trim_fraction(mantissa);
                return format!("{mantissa}e{exponent}");
            }
            let places = (digits as i32 - 1 - exponent).max(0) as usize;
            trim_fraction(&format!("{v:.places$}")).to_string()
        }
    }
}

fn trim_fraction(text: &str) -> &str {
    match text.contains('.') {
        true => text.trim_end_matches('0').trim_end_matches('.'),
        false => text,
    }
}

/// `Boolean CalculateExpression(const char *expression, int precision,
/// int flags, char *answer, size_t length)`: true with the result written
/// to `answer`, or false for an expression that does not evaluate.
///
/// # Safety
///
/// `expression` is a C string, and `answer` is `length` writable bytes.
unsafe extern "C" fn CalculateExpression(
    expression: *const c_char,
    precision: c_int,
    flags: c_int,
    answer: *mut c_char,
    length: usize,
) -> u8 {
    if expression.is_null() || answer.is_null() || length == 0 {
        return 0;
    }
    // SAFETY: the caller's C string.
    let text = unsafe { std::ffi::CStr::from_ptr(expression) }.to_string_lossy();
    let Ok(value) = evaluate(&text, flags) else {
        return 0;
    };
    let result = format(value, precision);
    let bytes = result.as_bytes();
    if bytes.len() >= length {
        return 0;
    }
    // SAFETY: `length` bytes are writable, and the result and its NUL fit.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), answer as *mut u8, bytes.len());
        *answer.add(bytes.len()) = 0;
    }
    1
}

exports::exports! {
    functions {
        "CalculateExpression" => CalculateExpression,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn integer(text: &str) -> String {
        format(evaluate(text, flags::INTEGER).expect(text), 16)
    }

    fn real(text: &str) -> String {
        format(evaluate(text, 0).expect(text), 16)
    }

    #[test]
    fn programmer_expressions_are_unsigned_64_bit() {
        assert_eq!(integer("18 + 3 "), "21");
        assert_eq!(integer("6 AND 3 "), "2");
        assert_eq!(integer("6 OR 3 "), "7");
        assert_eq!(integer("6 XOR 3 "), "5");
        assert_eq!(integer("6 NOR 3 "), (!7u64).to_string());
        assert_eq!(integer("(6 << 1) "), "12");
        assert_eq!(integer("(12 >> 1) << 2 "), "24");
        assert_eq!(integer("3 - 6 "), (3u64.wrapping_sub(6)).to_string());
        assert_eq!(integer("7 / 2 "), "3");
        assert!(evaluate("1 / 0 ", flags::INTEGER).is_err());
    }

    #[test]
    fn precedence_is_c_like() {
        assert_eq!(integer("1 + 2 * 3 "), "7");
        assert_eq!(integer("1 OR 2 AND 3 "), "3");
        assert_eq!(real("2 ^ 3 ^ 2"), "512");
        assert_eq!(real("-2 ^ 2"), "-4");
    }

    #[test]
    fn reals_round_to_the_precision() {
        assert_eq!(real("1 / 3"), "0.3333333333333333");
        assert_eq!(real("0.1 + 0.2"), "0.3");
        assert_eq!(real("sqrt(2) * sqrt(2)"), "2");
        assert_eq!(real("2 ^ 64"), "1.844674407370955e19");
        assert_eq!(real("fact(5)"), "120");
        assert_eq!(real("5!"), "120");
    }
}
