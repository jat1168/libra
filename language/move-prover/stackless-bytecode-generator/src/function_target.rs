// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    annotations::Annotations,
    borrow_analysis, lifetime_analysis, livevar_analysis, packref_analysis, reaching_def_analysis,
    stackless_bytecode::{AttrId, Bytecode, SpecBlockId},
    writeback_analysis,
};
use itertools::Itertools;
use spec_lang::{
    ast::Spec,
    env::{FunId, FunctionEnv, GlobalEnv, Loc, ModuleEnv, StructId, TypeParameter},
    symbol::{Symbol, SymbolPool},
    ty::{Type, TypeDisplayContext},
};
use std::{cell::RefCell, collections::BTreeMap, fmt};
use vm::file_format::CodeOffset;

/// A FunctionTarget is a drop-in replacement for a FunctionEnv which allows to rewrite
/// and analyze bytecode and parameter/local types. It encapsulates a FunctionEnv and information
/// which can be rewritten using the `FunctionTargetsHolder` data structure.
pub struct FunctionTarget<'env> {
    pub func_env: &'env FunctionEnv<'env>,
    pub data: &'env FunctionTargetData,
    pub name_to_index: BTreeMap<Symbol, usize>,

    // Used for debugging and testing, containing any attached annotation formatters.
    annotation_formatters: RefCell<Vec<Box<AnnotationFormatter>>>,
}

/// Holds the owned data belonging to a FunctionTarget, which can be rewritten using
/// the `FunctionTargetsHolder::rewrite` method.
#[derive(Debug)]
pub struct FunctionTargetData {
    pub code: Vec<Bytecode>,
    pub local_types: Vec<Type>,
    pub return_types: Vec<Type>,
    pub ref_param_map: BTreeMap<usize, usize>,
    pub acquires_global_resources: Vec<StructId>,
    pub locations: BTreeMap<AttrId, Loc>,
    pub annotations: Annotations,

    /// Map of spec block ids as given by the source, to the code offset in the original
    /// bytecode. Those spec block's content is found at
    /// `func_env.get_specification_on_impl(code_offset)`.
    pub given_spec_blocks: BTreeMap<SpecBlockId, CodeOffset>,

    /// Map of spec block ids to generated by transformations, to the generated conditions.
    pub generated_spec_blocks: BTreeMap<SpecBlockId, Spec>,
}

impl<'env> FunctionTarget<'env> {
    pub fn new(
        func_env: &'env FunctionEnv<'env>,
        data: &'env FunctionTargetData,
    ) -> FunctionTarget<'env> {
        let name_to_index = (0..func_env.get_local_count())
            .map(|idx| (func_env.get_local_name(idx), idx))
            .collect();
        FunctionTarget {
            func_env,
            data,
            name_to_index,
            annotation_formatters: RefCell::new(vec![]),
        }
    }

    /// Returns the name of this function.
    pub fn get_name(&self) -> Symbol {
        self.func_env.get_name()
    }

    /// Gets the id of this function.
    pub fn get_id(&self) -> FunId {
        self.func_env.get_id()
    }

    /// Shortcut for accessing the symbol pool.
    pub fn symbol_pool(&self) -> &SymbolPool {
        self.func_env.module_env.symbol_pool()
    }

    /// Shortcut for accessing the module env of this function.
    pub fn module_env(&self) -> &ModuleEnv {
        &self.func_env.module_env
    }

    /// Shortcut for accessing the global env of this function.
    pub fn global_env(&self) -> &GlobalEnv {
        self.func_env.module_env.env
    }

    /// Returns the location of this function.
    pub fn get_loc(&self) -> Loc {
        self.func_env.get_loc()
    }

    /// Returns the location of the bytecode at the given offset.
    pub fn get_bytecode_loc(&self, attr_id: AttrId) -> Loc {
        if let Some(loc) = self.data.locations.get(&attr_id) {
            loc.clone()
        } else {
            self.get_loc()
        }
    }

    /// Returns true if this function is native.
    pub fn is_native(&self) -> bool {
        self.func_env.is_native()
    }

    /// Returns true if this function is public.
    pub fn is_public(&self) -> bool {
        self.func_env.is_public()
    }

    /// Returns true if this function mutates any references (i.e. has &mut parameters).
    pub fn is_mutating(&self) -> bool {
        self.func_env.is_mutating()
    }

    /// Returns the type parameters associated with this function.
    pub fn get_type_parameters(&self) -> Vec<TypeParameter> {
        self.func_env.get_type_parameters()
    }

    /// Returns return type at given index.
    pub fn get_return_type(&self, idx: usize) -> &Type {
        &self.data.return_types[idx]
    }

    /// Returns return types of this function.
    pub fn get_return_types(&self) -> &[Type] {
        &self.data.return_types
    }

    /// Returns the number of return values of this function.
    pub fn get_return_count(&self) -> usize {
        self.data.return_types.len()
    }

    pub fn get_parameter_count(&self) -> usize {
        self.func_env.get_parameter_count()
    }

    /// Get the name to be used for a local. If the local is an argument, use that for naming,
    /// otherwise generate a unique name.
    pub fn get_local_name(&self, idx: usize) -> Symbol {
        self.func_env.get_local_name(idx)
    }

    /// Get the index corresponding to a local name
    pub fn get_local_index(&self, name: Symbol) -> Option<&usize> {
        self.name_to_index.get(&name)
    }

    /// Gets the number of locals of this function, including parameters.
    pub fn get_local_count(&self) -> usize {
        self.data.local_types.len()
    }

    /// Gets the number of user declared locals of this function, excluding locals which have
    /// been introduced by transformations.
    pub fn get_user_local_count(&self) -> usize {
        self.func_env.get_local_count()
    }

    /// Gets the type of the local at index. This must use an index in the range as determined by
    /// `get_local_count`.
    pub fn get_local_type(&self, idx: usize) -> &Type {
        &self.data.local_types[idx]
    }

    /// Returns specification associated with this function.
    pub fn get_spec(&'env self) -> &'env Spec {
        self.func_env.get_spec()
    }

    /// Returns specification conditions associated with this function at spec block id.
    pub fn get_spec_on_impl(&'env self, block_id: SpecBlockId) -> &'env Spec {
        if let Some(code_offset) = self.data.given_spec_blocks.get(&block_id) {
            self.func_env
                .get_spec()
                .on_impl
                .get(code_offset)
                .expect("given spec block defined")
        } else {
            self.data
                .generated_spec_blocks
                .get(&block_id)
                .expect("generated spec block defined")
        }
    }

    /// Returns the value of a boolean pragma for this function. This first looks up a
    /// pragma in this function, then the enclosing module, and finally uses the provided default.
    /// property
    pub fn is_pragma_true(&self, name: &str, default: impl FnOnce() -> bool) -> bool {
        self.func_env.is_pragma_true(name, default)
    }

    /// Gets the bytecode.
    pub fn get_bytecode(&self) -> &[Bytecode] {
        &self.data.code
    }

    /// Gets annotations.
    pub fn get_annotations(&self) -> &Annotations {
        &self.data.annotations
    }

    /// Gets acquired resources
    pub fn get_acquires_global_resources(&self) -> &[StructId] {
        &self.data.acquires_global_resources
    }

    /// Gets index of return parameter for a reference input parameter
    pub fn get_return_index(&self, idx: usize) -> Option<&usize> {
        self.data.ref_param_map.get(&idx)
    }

    pub fn call_ends_lifetime(&self) -> bool {
        self.is_public() && self.get_return_types().iter().all(|ty| !ty.is_reference())
    }
}

// =================================================================================================
// Formatting

/// A function which is called to display the value of an annotation for a given function target
/// at the given code offset. The function is passed the function target and the code offset, and
/// is expected to pick the annotation of its respective type from the function target and for
/// the given code offset. It should return None if there is no relevant annotation.
pub type AnnotationFormatter = dyn Fn(&FunctionTarget<'_>, CodeOffset) -> Option<String>;

impl<'env> FunctionTarget<'env> {
    /// Register a formatter. Each function target processor which introduces new annotations
    /// should register a formatter in order to get is value printed when a function target
    /// is displayed for debugging or testing.
    pub fn register_annotation_formatter(&self, formatter: Box<AnnotationFormatter>) {
        self.annotation_formatters.borrow_mut().push(formatter);
    }

    /// Tests use this function to register all relevant annotation formatters. Extend this with
    /// new formatters relevant for tests.
    pub fn register_annotation_formatters_for_test(&self) {
        self.register_annotation_formatter(Box::new(livevar_analysis::format_livevar_annotation));
        self.register_annotation_formatter(Box::new(borrow_analysis::format_borrow_annotation));
        self.register_annotation_formatter(Box::new(
            writeback_analysis::format_writeback_annotation,
        ));
        self.register_annotation_formatter(Box::new(packref_analysis::format_packref_annotation));
        self.register_annotation_formatter(Box::new(lifetime_analysis::format_lifetime_annotation));
        self.register_annotation_formatter(Box::new(
            reaching_def_analysis::format_reaching_def_annotation,
        ));
    }
}

impl<'env> fmt::Display for FunctionTarget<'env> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}fun {}::{}",
            if self.is_public() { "pub " } else { "" },
            self.func_env
                .module_env
                .get_name()
                .display(self.symbol_pool()),
            self.get_name().display(self.symbol_pool())
        )?;
        let tparams = &self.get_type_parameters();
        if !tparams.is_empty() {
            write!(f, "<")?;
            for (i, TypeParameter(name, _)) in tparams.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", name.display(self.symbol_pool()))?;
            }
            write!(f, ">")?;
        }
        let tctx = TypeDisplayContext::WithEnv {
            env: self.global_env(),
            type_param_names: None,
        };
        write!(f, "(")?;
        for i in 0..self.get_parameter_count() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(
                f,
                "{}: {}",
                self.get_local_name(i).display(self.symbol_pool()),
                self.get_local_type(i).display(&tctx)
            )?;
        }
        write!(f, ")")?;
        if self.get_return_count() > 0 {
            write!(f, ": ")?;
            if self.get_return_count() > 1 {
                write!(f, "(")?;
            }
            for i in 0..self.get_return_count() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", self.get_return_type(i).display(&tctx))?;
            }
            if self.get_return_count() > 1 {
                write!(f, ")")?;
            }
        }
        writeln!(f, " {{")?;
        for i in self.get_parameter_count()..self.get_local_count() {
            writeln!(
                f,
                "    var {}: {}",
                self.get_local_name(i).display(self.symbol_pool()),
                self.get_local_type(i).display(&tctx)
            )?;
        }
        for (offset, code) in self.get_bytecode().iter().enumerate() {
            let annotations = self
                .annotation_formatters
                .borrow()
                .iter()
                .filter_map(|f| f(self, offset as CodeOffset))
                .map(|s| format!("    // {}", s))
                .join("\n");
            if !annotations.is_empty() {
                writeln!(f, "{}", annotations)?;
            }
            writeln!(f, "    {}", code.display(self))?;
        }
        writeln!(f, "}}")?;
        Ok(())
    }
}
