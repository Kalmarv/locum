//! Expressions: addresses and values in `table.txt` and `do`. Numbers are decimal unless written `0x…` (hex) or
//! with a `.` or an exponent (a float); `[x]` reads the pointer at `x`; `+` and `-` add and subtract; a name is a
//! table symbol, an action's buffer, or `result` (what the last call returned); `$p` is an action's parameter;
//! `{v}` reads the table value `v`. Cheat Engine paths read the same, with their hex offsets written `0x…`:
//! `[[[[Base]+0x100]+0]+0x68]+0x70`.

use std::fmt;

/// A number: integers are 64 bits wide and wrap; floats are doubles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(u64),
    Float(f64),
}

impl Num {
    pub fn int(self) -> Result<u64, String> {
        match self {
            Num::Int(v) => Ok(v),
            Num::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e18 => Ok(f as i64 as u64),
            Num::Float(f) => Err(format!("{f} is not a whole number")),
        }
    }

    pub fn float(self) -> f64 {
        match self {
            Num::Int(v) => v as i64 as f64,
            Num::Float(f) => f,
        }
    }
}

impl fmt::Display for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Num::Int(v) => write!(f, "{} ({v:#x})", *v as i64),
            Num::Float(x) => write!(f, "{x}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Num(Num),
    Name(String),
    Param(String),
    Value(String),
    Deref(Box<Expr>),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
}

/// A name an expression uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ref<'a> {
    /// `name`: a symbol, buffer, value's address or `result`.
    Name(&'a str),
    /// `$name`
    Param(&'a str),
    /// `{name}`
    Value(&'a str),
}

/// What an expression's names mean where it is evaluated.
pub trait Scope {
    fn name(&self, name: &str) -> Result<u64, String>;
    fn param(&self, name: &str) -> Result<Num, String>;
    fn value(&self, name: &str) -> Result<Num, String>;
    fn pointer(&self, address: u64) -> Result<u64, String>;
}

impl Expr {
    pub fn parse(text: &str) -> Result<Expr, String> {
        let mut p = Parser {
            s: text.as_bytes(),
            at: 0,
        };
        let e = p.sum().map_err(|e| format!("{:?}: {e}", text.trim()))?;
        p.space();
        if p.at != p.s.len() {
            return Err(format!("{text:?}: unexpected {:?}", &text[p.at..]));
        }
        Ok(e)
    }

    /// Every name it uses, in order.
    pub fn refs(&self) -> Vec<Ref<'_>> {
        fn walk<'a>(e: &'a Expr, out: &mut Vec<Ref<'a>>) {
            match e {
                Expr::Num(_) => {}
                Expr::Name(n) => out.push(Ref::Name(n)),
                Expr::Param(n) => out.push(Ref::Param(n)),
                Expr::Value(n) => out.push(Ref::Value(n)),
                Expr::Deref(e) | Expr::Neg(e) => walk(e, out),
                Expr::Add(a, b) | Expr::Sub(a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out
    }

    pub fn eval(&self, scope: &dyn Scope) -> Result<Num, String> {
        Ok(match self {
            Expr::Num(n) => *n,
            Expr::Name(n) => Num::Int(scope.name(n)?),
            Expr::Param(n) => scope.param(n)?,
            Expr::Value(n) => scope.value(n)?,
            Expr::Deref(e) => Num::Int(scope.pointer(e.eval(scope)?.int()?)?),
            Expr::Neg(e) => match e.eval(scope)? {
                Num::Int(v) => Num::Int(v.wrapping_neg()),
                Num::Float(f) => Num::Float(-f),
            },
            Expr::Add(a, b) => arith(a.eval(scope)?, b.eval(scope)?, u64::wrapping_add, |x, y| x + y),
            Expr::Sub(a, b) => arith(a.eval(scope)?, b.eval(scope)?, u64::wrapping_sub, |x, y| x - y),
        })
    }
}

fn arith(a: Num, b: Num, int: fn(u64, u64) -> u64, float: fn(f64, f64) -> f64) -> Num {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => Num::Int(int(x, y)),
        (a, b) => Num::Float(float(a.float(), b.float())),
    }
}

/// A number written in `do` or `table.txt`.
pub fn number(text: &str) -> Result<Num, String> {
    let t = text.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let n = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        Num::Int(u64::from_str_radix(hex, 16).map_err(|e| format!("{text:?}: {e}"))?)
    } else if body.contains(['.', 'e', 'E']) || body == "inf" || body == "nan" {
        Num::Float(body.parse::<f64>().map_err(|e| format!("{text:?}: {e}"))?)
    } else {
        Num::Int(body.parse::<u64>().map_err(|e| format!("{text:?}: {e}"))?)
    };
    Ok(match (neg, n) {
        (false, n) => n,
        (true, Num::Int(v)) => Num::Int(v.wrapping_neg()),
        (true, Num::Float(f)) => Num::Float(-f),
    })
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.at < self.s.len() && self.s[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.space();
        self.s.get(self.at).copied()
    }

    fn sum(&mut self) -> Result<Expr, String> {
        let mut e = self.term()?;
        loop {
            match self.peek() {
                Some(b'+') => {
                    self.at += 1;
                    e = Expr::Add(Box::new(e), Box::new(self.term()?));
                }
                Some(b'-') => {
                    self.at += 1;
                    e = Expr::Sub(Box::new(e), Box::new(self.term()?));
                }
                _ => return Ok(e),
            }
        }
    }

    fn word(&mut self) -> &str {
        let start = self.at;
        while self.at < self.s.len()
            && (self.s[self.at].is_ascii_alphanumeric() || b"_.:".contains(&self.s[self.at]))
        {
            self.at += 1;
        }
        std::str::from_utf8(&self.s[start..self.at]).unwrap_or("")
    }

    fn term(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(b'[') => {
                self.at += 1;
                let e = self.sum()?;
                if self.peek() != Some(b']') {
                    return Err("missing ]".into());
                }
                self.at += 1;
                Ok(Expr::Deref(Box::new(e)))
            }
            Some(b'(') => {
                self.at += 1;
                let e = self.sum()?;
                if self.peek() != Some(b')') {
                    return Err("missing )".into());
                }
                self.at += 1;
                Ok(e)
            }
            Some(b'-') => {
                self.at += 1;
                Ok(Expr::Neg(Box::new(self.term()?)))
            }
            Some(b'$') => {
                self.at += 1;
                let w = self.word().to_string();
                if w.is_empty() {
                    return Err("$ needs a parameter name".into());
                }
                Ok(Expr::Param(w))
            }
            Some(b'{') => {
                self.at += 1;
                self.space();
                let w = self.word().to_string();
                if self.peek() != Some(b'}') || w.is_empty() {
                    return Err("{ needs a value name and }".into());
                }
                self.at += 1;
                Ok(Expr::Value(w))
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.at;
                // a float's exponent sign belongs to the number, not to a following + or -
                while self.at < self.s.len() {
                    let c = self.s[self.at];
                    let exp_sign = (c == b'+' || c == b'-')
                        && matches!(self.s[self.at - 1], b'e' | b'E')
                        && !self.s[start..self.at].starts_with(b"0x");
                    if c.is_ascii_alphanumeric() || c == b'.' || exp_sign {
                        self.at += 1;
                    } else {
                        break;
                    }
                }
                Ok(Expr::Num(number(
                    std::str::from_utf8(&self.s[start..self.at]).unwrap_or(""),
                )?))
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => Ok(Expr::Name(self.word().to_string())),
            Some(c) => Err(format!("unexpected {:?}", c as char)),
            None => Err("expression ends early".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Mem(HashMap<u64, u64>);
    impl Scope for Mem {
        fn name(&self, name: &str) -> Result<u64, String> {
            match name {
                "World" => Ok(0x1000),
                _ => Err(format!("no {name}")),
            }
        }
        fn param(&self, name: &str) -> Result<Num, String> {
            match name {
                "id" => Ok(Num::Int(10030001)),
                _ => Err(format!("no ${name}")),
            }
        }
        fn value(&self, _: &str) -> Result<Num, String> {
            Ok(Num::Float(1.5))
        }
        fn pointer(&self, address: u64) -> Result<u64, String> {
            self.0
                .get(&address)
                .copied()
                .ok_or(format!("unmapped {address:#x}"))
        }
    }

    #[test]
    fn cheat_engine_paths_follow_pointers() {
        let mem = Mem(HashMap::from([
            (0x1000, 0x2000),
            (0x2000 + 0x100, 0x3000),
            (0x3000, 0x4000),
        ]));
        let e = Expr::parse("[[[World]+0x100]+0]+0x6C0").unwrap();
        assert_eq!(e.eval(&mem).unwrap(), Num::Int(0x46C0));
    }

    #[test]
    fn numbers_are_decimal_unless_hex_or_float() {
        assert_eq!(number("1000").unwrap(), Num::Int(1000));
        assert_eq!(number("0x10").unwrap(), Num::Int(16));
        assert_eq!(number("-1").unwrap(), Num::Int(u64::MAX));
        assert_eq!(number("2.5").unwrap(), Num::Float(2.5));
        let mem = Mem(HashMap::new());
        assert_eq!(
            Expr::parse("$id-1000").unwrap().eval(&mem).unwrap(),
            Num::Int(10029001)
        );
        assert_eq!(
            Expr::parse("{x}+1e-3").unwrap().eval(&mem).unwrap(),
            Num::Float(1.501)
        );
        assert_eq!(
            Expr::parse("-(2+3)").unwrap().eval(&mem).unwrap(),
            Num::Int(5u64.wrapping_neg())
        );
    }

    #[test]
    fn bad_expressions_are_refused() {
        assert!(Expr::parse("[World+1").is_err());
        assert!(Expr::parse("World 1").is_err());
        assert!(Expr::parse("0xZZ").is_err());
        let mem = Mem(HashMap::new());
        assert!(Expr::parse("[World]").unwrap().eval(&mem).is_err());
    }

    #[test]
    fn names_are_listed_in_order() {
        let e = Expr::parse("[[Base]+$off]-{v}+result").unwrap();
        assert_eq!(
            e.refs(),
            vec![
                Ref::Name("Base"),
                Ref::Param("off"),
                Ref::Value("v"),
                Ref::Name("result")
            ]
        );
    }
}
