//! Human-readable rendering of a [`Chunk`] for debugging and tests.
//!
//! The output is stable-ish — assertions in tests match against it
//! directly. Each instruction renders to a single line with its
//! operand resolved inline (constants, names, jump targets). Nested
//! functions are recursively rendered after the main body.

#[cfg(feature = "no_std")]
use alloc::{format, string::{String, ToString}, vec::Vec};

use bop::lexer::StringPart;

use crate::chunk::{Chunk, Constant, Instr};

/// Render a chunk as a string. One line per instruction; nested
/// function bodies are indented and follow the main body.
pub fn disassemble(chunk: &Chunk) -> String {
    let mut out = String::new();
    render_chunk(chunk, &mut out, 0);
    out
}

fn render_chunk(chunk: &Chunk, out: &mut String, indent: usize) {
    let pad = "  ".repeat(indent);
    let width = chunk.code.len().saturating_sub(1).to_string().len().max(1);
    for (i, instr) in chunk.code.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&format!("{:>width$}: {}\n", i, render_instr(chunk, instr), width = width));
    }

    for (i, f) in chunk.functions.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&format!(
            "\n{}fn #{} {}({}):\n",
            pad,
            i,
            f.name,
            f.params.join(", ")
        ));
        render_chunk(&f.chunk, out, indent + 1);
    }
}

fn render_instr(chunk: &Chunk, instr: &Instr) -> String {
    match instr {
        Instr::LoadConst(idx) => {
            format!("LoadConst {}", render_constant(chunk.constant(*idx)))
        }
        Instr::LoadNone => "LoadNone".to_string(),
        Instr::LoadTrue => "LoadTrue".to_string(),
        Instr::LoadFalse => "LoadFalse".to_string(),

        Instr::LoadVar(n) => format!("LoadVar {}", chunk.name(*n)),
        Instr::DefineLocal(n) => format!("DefineLocal {}", chunk.name(*n)),
        Instr::StoreVar(n) => format!("StoreVar {}", chunk.name(*n)),

        Instr::LoadLocal(s) => format!("LoadLocal @{}", s.0),
        Instr::StoreLocal(s) => format!("StoreLocal @{}", s.0),

        Instr::AddLocals(a, b) => format!("AddLocals @{}, @{}", a.0, b.0),
        Instr::LtLocals(a, b) => format!("LtLocals @{}, @{}", a.0, b.0),
        Instr::IncLocalInt(s, k) => format!("IncLocalInt @{}, {}", s.0, k),
        Instr::LoadLocalAddInt(s, k) => format!("LoadLocalAddInt @{}, {}", s.0, k),
        Instr::LtLocalInt(s, k) => format!("LtLocalInt @{}, {}", s.0, k),

        Instr::PushScope => "PushScope".to_string(),
        Instr::PopScope => "PopScope".to_string(),

        Instr::Pop => "Pop".to_string(),
        Instr::Dup => "Dup".to_string(),
        Instr::Dup2 => "Dup2".to_string(),

        Instr::Add => "Add".to_string(),
        Instr::Sub => "Sub".to_string(),
        Instr::Mul => "Mul".to_string(),
        Instr::Div => "Div".to_string(),
        Instr::Rem => "Rem".to_string(),
        Instr::Eq => "Eq".to_string(),
        Instr::NotEq => "NotEq".to_string(),
        Instr::Lt => "Lt".to_string(),
        Instr::Gt => "Gt".to_string(),
        Instr::LtEq => "LtEq".to_string(),
        Instr::GtEq => "GtEq".to_string(),

        Instr::Neg => "Neg".to_string(),
        Instr::Not => "Not".to_string(),

        Instr::TruthyToBool => "TruthyToBool".to_string(),

        Instr::GetIndex => "GetIndex".to_string(),
        Instr::SetIndex => "SetIndex".to_string(),

        Instr::StringInterp(idx) => {
            let recipe = chunk.interp(*idx);
            let parts: Vec<String> = recipe
                .parts
                .iter()
                .map(|p| match p {
                    StringPart::Literal(s) => format!("{:?}", s),
                    StringPart::Variable(name) => format!("${}", name),
                })
                .collect();
            format!("StringInterp [{}]", parts.join(", "))
        }

        Instr::MakeArray(n) => format!("MakeArray {}", n),
        Instr::MakeDict(n) => format!("MakeDict {}", n),

        Instr::Call { name, argc } => {
            format!("Call {}/{}", chunk.name(*name), argc)
        }
        Instr::CallValue { argc } => format!("CallValue /{}", argc),
        Instr::CallMethod {
            method,
            argc,
            assign_back_to,
        } => {
            let name = chunk.name(*method);
            match assign_back_to {
                Some(crate::chunk::AssignBack::Name(var)) => format!(
                    "CallMethod .{}/{} (back to {})",
                    name,
                    argc,
                    chunk.name(*var)
                ),
                Some(crate::chunk::AssignBack::Slot(slot)) => format!(
                    "CallMethod .{}/{} (back to @{})",
                    name, argc, slot.0
                ),
                None => format!("CallMethod .{}/{}", name, argc),
            }
        }

        Instr::DefineFn(idx) => {
            format!("DefineFn #{} ({})", idx.0, chunk.function(*idx).name)
        }
        Instr::MakeLambda(idx) => {
            format!("MakeLambda #{} ({})", idx.0, chunk.function(*idx).name)
        }
        Instr::Return => "Return".to_string(),
        Instr::ReturnNone => "ReturnNone".to_string(),

        Instr::MakeIter => "MakeIter".to_string(),
        Instr::IterNext { target } => format!("IterNext -> {}", target.0),
        Instr::MakeRepeatCount => "MakeRepeatCount".to_string(),
        Instr::RepeatNext { target } => format!("RepeatNext -> {}", target.0),

        Instr::Jump(t) => format!("Jump -> {}", t.0),
        Instr::JumpIfFalse(t) => format!("JumpIfFalse -> {}", t.0),
        Instr::JumpIfFalsePeek(t) => format!("JumpIfFalsePeek -> {}", t.0),
        Instr::JumpIfTruePeek(t) => format!("JumpIfTruePeek -> {}", t.0),

        Instr::Use(idx) => {
            let spec = chunk.use_spec(*idx);
            let items = match &spec.items {
                Some(list) => format!(".{{{}}}", list.join(", ")),
                None => String::new(),
            };
            let alias = match &spec.alias {
                Some(a) => format!(" as {}", a),
                None => String::new(),
            };
            format!("Use {}{}{}", spec.path, items, alias)
        }

        Instr::DefineStruct(idx) => {
            let def = chunk.struct_def(*idx);
            format!("DefineStruct {} {{ {} }}", def.name, def.fields.join(", "))
        }
        Instr::DefineEnum(idx) => {
            let def = chunk.enum_def(*idx);
            format!(
                "DefineEnum {} [{} variants]",
                def.name,
                def.variants.len()
            )
        }
        Instr::DefineMethod {
            type_name,
            method_name,
            fn_idx,
        } => {
            format!(
                "DefineMethod {}::{} (#{})",
                chunk.name(*type_name),
                chunk.name(*method_name),
                fn_idx.0,
            )
        }
        Instr::ConstructStruct {
            namespace,
            type_name,
            count,
        } => {
            let ns_prefix = match namespace {
                Some(ns) => format!("{}.", chunk.name(*ns)),
                None => String::new(),
            };
            format!(
                "ConstructStruct {}{}/{}",
                ns_prefix,
                chunk.name(*type_name),
                count
            )
        }
        Instr::ConstructEnum {
            namespace,
            type_name,
            variant,
            shape,
        } => {
            use crate::chunk::EnumConstructShape as S;
            let shape_str = match shape {
                S::Unit => "Unit".to_string(),
                S::Tuple(n) => format!("Tuple({})", n),
                S::Struct(n) => format!("Struct({})", n),
            };
            let ns_prefix = match namespace {
                Some(ns) => format!("{}.", chunk.name(*ns)),
                None => String::new(),
            };
            format!(
                "ConstructEnum {}{}::{} {}",
                ns_prefix,
                chunk.name(*type_name),
                chunk.name(*variant),
                shape_str,
            )
        }
        Instr::FieldGet(n) => format!("FieldGet .{}", chunk.name(*n)),
        Instr::FieldSet(n) => format!("FieldSet .{}", chunk.name(*n)),

        Instr::MatchFail { pattern, on_fail } => {
            format!("MatchFail pat#{} -> {}", pattern.0, on_fail.0)
        }
        Instr::MatchExhausted => "MatchExhausted".to_string(),

        Instr::TryUnwrap => "TryUnwrap".to_string(),

        Instr::Halt => "Halt".to_string(),
    }
}

fn render_constant(c: &Constant) -> String {
    match c {
        Constant::Int(n) => format!("{}", n),
        Constant::Number(n) => {
            if *n == (*n as i64 as f64) && n.is_finite() {
                format!("{}", *n as i64)
            } else {
                format!("{}", n)
            }
        }
        Constant::Str(s) => format!("{:?}", s),
    }
}
