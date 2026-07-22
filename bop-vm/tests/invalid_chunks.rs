use std::rc::Rc;

use bop::{BopHost, BopLimits, Value};
use bop_vm::chunk::{
    Chunk, CodeOffset, ConstIdx, ConstructFieldsIdx, EnumConstructShape, EnumIdx, FnDef, FnIdx,
    Instr, InterpIdx, InterpPart, InterpRecipe, NameIdx, NamespaceIdx, NamespaceRef, PatternIdx,
    PatternRecipe, SlotIdx, StructIdx, UseIdx,
};
use bop_vm::{Vm, execute};

#[derive(Default)]
struct SilentHost;

impl BopHost for SilentHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, bop::BopError>> {
        None
    }
}

fn chunk_with(instr: Instr) -> Chunk {
    Chunk {
        code: vec![instr, Instr::Halt],
        lines: vec![7, 0],
        ..Chunk::new()
    }
}

fn execution_error(chunk: Chunk) -> bop::BopError {
    execute(chunk, &mut SilentHost, &BopLimits::standard())
        .expect_err("malformed bytecode must be rejected")
}

#[test]
fn execute_accepts_a_valid_hand_built_chunk() {
    let chunk = Chunk {
        code: vec![Instr::Halt],
        lines: vec![1],
        ..Chunk::new()
    };

    execute(chunk, &mut SilentHost, &BopLimits::standard()).unwrap();
}

#[test]
fn execute_rejects_mismatched_code_and_line_tables() {
    let error = execution_error(Chunk {
        code: vec![Instr::Halt],
        lines: vec![],
        ..Chunk::new()
    });

    assert_eq!(error.line, Some(0));
    assert!(error.message.contains("1 instructions but 0 source lines"));
}

#[test]
fn execute_rejects_every_index_pool_without_panicking() {
    let cases = [
        (Instr::LoadConst(ConstIdx(0)), "constant 0"),
        (Instr::LoadVar(NameIdx(0)), "name 0"),
        (Instr::StringInterp(InterpIdx(0)), "interpolation 0"),
        (Instr::DefineFn(FnIdx(0)), "function 0"),
        (Instr::DefineStruct(StructIdx(0)), "struct definition 0"),
        (Instr::DefineEnum(EnumIdx(0)), "enum definition 0"),
        (Instr::Use(UseIdx(0)), "use specification 0"),
        (
            Instr::ConstructStruct {
                namespace: Some(NamespaceIdx::new(0)),
                type_name: NameIdx(0),
                count: 0,
            },
            "namespace reference 0",
        ),
        (
            Instr::MatchFail {
                pattern: PatternIdx(0),
                on_fail: CodeOffset(1),
            },
            "pattern 0",
        ),
    ];

    for (instr, expected) in cases {
        let error = execution_error(chunk_with(instr));
        assert_eq!(error.line, Some(7));
        assert!(
            error.message.contains(expected),
            "expected `{expected}` in `{}`",
            error.message
        );
    }
}

#[test]
fn execute_rejects_invalid_interpolation_parts() {
    let mut chunk = chunk_with(Instr::StringInterp(InterpIdx(0)));
    chunk.interps.push(InterpRecipe {
        parts: Rc::from([InterpPart::Name(NameIdx(0))]),
    });

    let error = execution_error(chunk);
    assert!(error.message.contains("interpolation 0 references name 0"));
}

#[test]
fn execute_rejects_invalid_construction_field_recipe_pool() {
    let mut chunk = chunk_with(Instr::ValidateStructConstruct {
        namespace: None,
        type_name: NameIdx(0),
        fields: ConstructFieldsIdx(0),
    });
    chunk.names.push("Point".into());

    let error = execution_error(chunk);
    assert!(error.message.contains("construction field recipe 0"));
}

#[test]
fn execute_rejects_top_level_local_slots_and_out_of_stream_jumps() {
    let slot_error = execution_error(chunk_with(Instr::LoadLocal(SlotIdx(0))));
    assert!(slot_error.message.contains("local slot 0"));

    let jump_error = execution_error(chunk_with(Instr::Jump(CodeOffset(2))));
    assert!(jump_error.message.contains("jump target 2"));
}

#[test]
fn execute_rejects_invalid_namespace_references() {
    let mut construct = chunk_with(Instr::ConstructStruct {
        namespace: Some(NamespaceIdx::new(0)),
        type_name: NameIdx(1),
        count: 0,
    });
    construct.names = vec!["module".into(), "Point".into()];
    construct
        .namespace_refs
        .push(NamespaceRef::from_slot(NameIdx(0), SlotIdx(0)));
    let slot_error = execution_error(construct);
    assert!(slot_error.message.contains("local slot 0"));

    let mut struct_preflight = chunk_with(Instr::ValidateStructConstruct {
        namespace: Some(NamespaceIdx::new(0)),
        type_name: NameIdx(0),
        fields: ConstructFieldsIdx(0),
    });
    struct_preflight.names.push("Point".into());
    struct_preflight
        .namespace_refs
        .push(NamespaceRef::from_name(NameIdx(1)));
    struct_preflight.construct_fields.push(vec![]);
    let preflight_name_error = execution_error(struct_preflight);
    assert!(preflight_name_error.message.contains("name 1"));

    let mut enum_preflight = chunk_with(Instr::ValidateEnumConstruct {
        namespace: Some(NamespaceIdx::new(0)),
        type_name: NameIdx(1),
        variant: NameIdx(2),
        shape: EnumConstructShape::Unit,
        fields: ConstructFieldsIdx(0),
    });
    enum_preflight.names = vec!["module".into(), "Maybe".into(), "Some".into()];
    enum_preflight
        .namespace_refs
        .push(NamespaceRef::from_slot(NameIdx(0), SlotIdx(0)));
    enum_preflight.construct_fields.push(vec![]);
    let preflight_slot_error = execution_error(enum_preflight);
    assert!(preflight_slot_error.message.contains("local slot 0"));

    let mut pattern = chunk_with(Instr::MatchFail {
        pattern: PatternIdx(0),
        on_fail: CodeOffset(1),
    });
    pattern.patterns.push(PatternRecipe {
        pattern: Rc::new(bop::parser::Pattern::Wildcard),
        namespaces: vec![("module".into(), NamespaceRef::from_name(NameIdx(0)))],
    });
    let name_error = execution_error(pattern);
    assert!(name_error.message.contains("pattern 0 references name 0"));
}

#[test]
fn execute_recursively_validates_function_chunks_and_capture_metadata() {
    let child = Chunk {
        code: vec![Instr::LoadConst(ConstIdx(0)), Instr::ReturnNone],
        lines: vec![11, 0],
        slot_count: 1,
        ..Chunk::new()
    };
    let function = FnDef {
        name: "broken".into(),
        params: vec!["x".into()],
        chunk: Rc::new(child),
        slot_count: 1,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let mut outer = chunk_with(Instr::DefineFn(FnIdx(0)));
    outer.functions.push(function);

    let error = execution_error(outer);
    assert_eq!(error.line, Some(11));
    assert!(error.message.contains("nested function 0 `broken`"));
    assert!(error.message.contains("constant 0"));
}

#[test]
fn shared_function_chunk_dags_are_validated_once_without_recursive_traversal() {
    let leaf = Rc::new(Chunk {
        code: vec![Instr::ReturnNone],
        lines: vec![0],
        ..Chunk::new()
    });
    let mut shared = leaf;
    for depth in 0..24 {
        let function = |name: &str| FnDef {
            name: format!("{name}_{depth}"),
            params: vec![],
            chunk: Rc::clone(&shared),
            slot_count: 0,
            capture_names: vec![],
            capture_sources: vec![],
        };
        shared = Rc::new(Chunk {
            code: vec![Instr::ReturnNone],
            lines: vec![0],
            functions: vec![function("left"), function("right")],
            ..Chunk::new()
        });
    }

    let top = Chunk {
        code: vec![Instr::Halt],
        lines: vec![0],
        functions: vec![FnDef {
            name: "root".into(),
            params: vec![],
            chunk: shared,
            slot_count: 0,
            capture_names: vec![],
            capture_sources: vec![],
        }],
        ..Chunk::new()
    };

    execute(top, &mut SilentHost, &BopLimits::standard()).unwrap();
}

#[test]
fn execute_rejects_sparse_or_absurd_function_slot_metadata() {
    let child = Chunk {
        code: vec![Instr::ReturnNone],
        lines: vec![0],
        slot_count: u32::MAX,
        ..Chunk::new()
    };
    let function = FnDef {
        name: "oversized".into(),
        params: vec![],
        chunk: Rc::new(child),
        slot_count: u32::MAX,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let mut outer = chunk_with(Instr::DefineFn(FnIdx(0)));
    outer.functions.push(function);

    let error = execution_error(outer);
    assert!(error.message.contains("only 0 are densely declared"));
}

#[test]
fn shared_chunks_are_validated_for_each_distinct_parameter_layout() {
    let child = Rc::new(Chunk {
        code: vec![Instr::ReturnNone],
        lines: vec![0],
        slot_count: 1,
        ..Chunk::new()
    });
    let function = |name: &str, params: Vec<String>| FnDef {
        name: name.into(),
        params,
        chunk: Rc::clone(&child),
        slot_count: 1,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let mut outer = chunk_with(Instr::Halt);
    // LIFO traversal sees `valid` first. Pointer-only memoization would then
    // skip the invalid zero-parameter layout for the same Rc allocation.
    outer.functions = vec![
        function("invalid", vec![]),
        function("valid", vec!["value".into()]),
    ];

    let error = execution_error(outer);
    assert!(error.message.contains("only 0 are densely declared"));
}

#[test]
fn trusted_vm_api_checks_every_fused_local_slot_instruction() {
    let instructions = [
        Instr::AddLocals(SlotIdx(0), SlotIdx(0)),
        Instr::LtLocals(SlotIdx(0), SlotIdx(0)),
        Instr::IncLocalInt(SlotIdx(0), 1),
        Instr::LoadLocalAddInt(SlotIdx(0), 1),
        Instr::LtLocalInt(SlotIdx(0), 1),
    ];

    for instr in instructions {
        let mut host = SilentHost;
        let error = Vm::new(chunk_with(instr), &mut host, BopLimits::standard())
            .run()
            .expect_err("invalid fused slot must return an error");
        assert_eq!(error.line, Some(7));
        assert_eq!(error.message, "VM: local slot out of range");
    }
}
