//! The rules that change what is drawn, shared by locum in the game (`overrides.txt`) and by replay. One rule per
//! line, `#` starts a comment:
//!
//! - `skip vs=<hash>`: draws with that shader bound are not drawn (`skip cs=<hash>`: dispatches are not run);
//! - `solid vs=<hash> R G B A`: draws with that shader bound get a pixel shader writing that colour to every target;
//! - `shader ps=<hash> FILE`: that shader is replaced by the bytecode in FILE (relative to the rules' folder);
//! - `constant ps b3 c0 X Y Z W`: the draw sees constant 0 of its ps b3 buffer (counted from the first constant
//!   bound) as X Y Z W; a value is a float or `0x` and 8 hex digits (raw bits).
//!
//! Any rule can end with conditions, `if vs=<hash> ps=<hash>`, all of which must hold; `vs=<hash>` after `skip`,
//! `solid` and `shader` is such a condition too. Any stage can be named: vs hs ds gs ps cs. Stages are numbered in
//! that order (as in the capture's `draws` table).

use std::path::Path;

/// The stages by number.
pub const STAGES: [&str; 6] = ["vs", "hs", "ds", "gs", "ps", "cs"];
pub const PIXEL: usize = 4;
pub const COMPUTE: usize = 5;
/// Constant-buffer slots a stage has, and constants (16 bytes) a buffer can hold.
pub const CB_SLOTS: u32 = 14;
pub const CONSTANTS: u32 = 4096;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Skip,
    /// A pixel shader writing one colour.
    Solid([f32; 4]),
    /// A shader of the rule's stage, from bytecode.
    Shader(Vec<u8>),
    Constant {
        slot: u32,
        index: u32,
        bits: [u32; 4],
    },
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub text: String,
    /// The stage the rule changes (pixel for `solid`).
    pub stage: usize,
    /// Shaders that must be bound, by stage.
    pub when: Vec<(usize, u64)>,
    pub action: Action,
}

pub fn stage_named(name: &str) -> Option<usize> {
    STAGES.iter().position(|s| *s == name)
}

/// `vs=0123456789abcdef`.
fn condition(word: &str) -> Result<(usize, u64), String> {
    let (stage, hash) = word
        .split_once('=')
        .ok_or_else(|| format!("{word:?} is not stage=hash"))?;
    let stage = stage_named(stage).ok_or_else(|| format!("unknown stage {stage:?}"))?;
    let hash = u64::from_str_radix(hash, 16).map_err(|_| format!("{hash:?} is not a hex hash"))?;
    Ok((stage, hash))
}

fn value_bits(word: &str) -> Result<u32, String> {
    if let Some(hex) = word.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).map_err(|_| format!("{word:?} is not 0x and hex digits"));
    }
    word.parse::<f32>()
        .map(f32::to_bits)
        .map_err(|_| format!("{word:?} is not a number"))
}

fn numbered(word: Option<&str>, prefix: char, what: &str) -> Result<u32, String> {
    word.and_then(|w| w.strip_prefix(prefix))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("expected {what} ({prefix}N)"))
}

/// One rule; `dir` is where a `shader` rule's file is looked for.
pub fn parse(line: &str, dir: &Path) -> Result<Rule, String> {
    let (body, conditions) = match line.split_once(" if ") {
        Some((b, c)) => (
            b,
            c.split_whitespace()
                .map(condition)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => (line, Vec::new()),
    };
    let mut words = body.split_whitespace();
    let verb = words.next().ok_or("empty rule")?;
    let mut when = conditions;
    let (stage, action) = match verb {
        "skip" | "solid" | "shader" => {
            let first = condition(words.next().ok_or("expected stage=hash")?)?;
            when.insert(0, first);
            match verb {
                "skip" => (first.0, Action::Skip),
                "solid" => {
                    let mut c = [0f32; 4];
                    for v in &mut c {
                        let w = words.next().ok_or("solid needs R G B A")?;
                        *v = w.parse().map_err(|_| format!("{w:?} is not a number"))?;
                    }
                    (PIXEL, Action::Solid(c))
                }
                _ => {
                    let file = words.next().ok_or("shader needs a file")?;
                    let code = std::fs::read(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
                    (first.0, Action::Shader(code))
                }
            }
        }
        "constant" => {
            let stage = words
                .next()
                .and_then(stage_named)
                .ok_or("constant needs a stage")?;
            let slot = numbered(words.next(), 'b', "a constant buffer slot")?;
            let index = numbered(words.next(), 'c', "a constant")?;
            let mut bits = [0u32; 4];
            for b in &mut bits {
                *b = value_bits(words.next().ok_or("constant needs X Y Z W")?)?;
            }
            if slot >= CB_SLOTS || index >= CONSTANTS {
                return Err("slot or constant out of range".into());
            }
            (stage, Action::Constant { slot, index, bits })
        }
        other => return Err(format!("unknown rule {other:?}")),
    };
    if let Some(extra) = words.next() {
        return Err(format!("unexpected {extra:?}"));
    }
    Ok(Rule {
        text: line.to_string(),
        stage,
        when,
        action,
    })
}

/// Every rule in `text`, and for each line that is not one, its number (from 1) and why.
pub fn parse_all(text: &str, dir: &Path) -> (Vec<Rule>, Vec<(usize, String)>) {
    let (mut rules, mut errors) = (Vec::new(), Vec::new());
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match parse(line, dir) {
            Ok(r) => rules.push(r),
            Err(e) => errors.push((n + 1, e)),
        }
    }
    (rules, errors)
}

impl Rule {
    /// Whether the rule is for a dispatch (`compute`) or a draw.
    pub fn applies(&self, compute: bool) -> bool {
        compute == (self.stage == COMPUTE)
    }

    /// Whether its conditions hold, given the hash of the shader bound at each stage.
    pub fn holds(&self, mut hash: impl FnMut(usize) -> Option<u64>) -> bool {
        self.when.iter().all(|&(stage, h)| hash(stage) == Some(h))
    }
}

/// HLSL (shader model 5) for a pixel shader writing `colour` to all eight targets.
pub fn solid_source(colour: &[f32; 4]) -> String {
    let [r, g, b, a] = colour;
    let targets: String = (0..8).map(|i| format!("float4 c{i} : SV_Target{i}; ")).collect();
    let fill: String = (0..8).map(|i| format!("o.c{i} = k; ")).collect();
    format!(
        "struct O {{ {targets}}}; O main() {{ float4 k = float4({r:?}, {g:?}, {b:?}, {a:?}); O o; {fill}return o; }}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule(line: &str) -> Result<Rule, String> {
        parse(line, &PathBuf::from("."))
    }

    #[test]
    fn rules_parse() {
        let r = rule("skip vs=00000000000000ff if ps=10").unwrap();
        assert_eq!(r.action, Action::Skip);
        assert_eq!(r.when, vec![(0, 0xff), (4, 0x10)]);
        let r = rule("constant vs b11 c0 4 4 0x3f800000 -1.5").unwrap();
        assert_eq!(
            r.action,
            Action::Constant {
                slot: 11,
                index: 0,
                bits: [
                    4f32.to_bits(),
                    4f32.to_bits(),
                    1f32.to_bits(),
                    (-1.5f32).to_bits()
                ]
            }
        );
        assert_eq!(
            rule("solid vs=1 1 0 1 1").unwrap().action,
            Action::Solid([1.0, 0.0, 1.0, 1.0])
        );
        assert!(rule("constant vs b14 c0 0 0 0 0").is_err());
        assert!(rule("skip vs").is_err());
        assert!(rule("frobnicate").is_err());
    }

    #[test]
    fn conditions() {
        let r = rule("skip cs=ab if vs=1").unwrap();
        assert!(r.applies(true) && !r.applies(false));
        assert!(r.holds(|s| Some(if s == COMPUTE { 0xab } else { 1 })));
        assert!(!r.holds(|_| None));
    }
}
