use std::fmt::Debug;
use std::{fs, cell};
use std::path::{Path, PathBuf};
use cairo_lang_casm::cell_expression::{CellExpression, CellOperator};
use itertools::Itertools;

use std::collections::HashMap;

use cairo_lang_casm::builder::{
    CasmBuilderAuxiliaryInfo,
    RetBranchDesc,
    StatementDesc,
    ExprDesc,
    VarDesc,
    AssertDesc,
    Var,
};
use cairo_lang_casm::instructions::InstructionBody;
use cairo_lang_casm::operand::{ DerefOrImmediate, ResOperand, CellRef };
use cairo_lang_sierra_to_casm::compiler::CairoProgram;
use cairo_lang_utils::bigint::BigIntAsHex;

pub fn lean_code_file_name(func_name: &str) -> String {
    String::from(func_name) + "_code.lean"
}

pub fn lean_soundness_file_name(func_name: &str) -> String {
    String::from(func_name) + "_soundness.lean"
}

pub fn lean_verification_path(test_path: &Path) -> PathBuf {
    test_path.parent().unwrap().join("verification")
}

pub fn lean_file_path(lean_path: &Path, file_name: &str) -> PathBuf {
    lean_path.join(file_name)
}

fn make_subscript(idx: usize) -> String {
    let mut idx = idx;
    let mut res = String::new();
    if idx == 0 {
        return String::from("");
    }
    while idx != 0 {
        res.insert(0, "₀₁₂₃₄₅₆₇₈₉".chars().nth(idx % 10).unwrap());
        idx /= 10;
    }
    res
}

struct RetArgs {
    branch_info: Vec<(usize, Option<usize>)>,
    arg_names: Vec<String>,
    branch_id_pos: Option<usize>,
    num_implicit_ret_args: usize,
}

impl RetArgs {

    fn get_arg_name_at_pos(branch_desc: &Vec<RetBranchDesc>, pos: usize) -> String {

        if branch_desc.len() == 0 {
            return String::from("ρ");
        }

        let mut arg_name: String = String::new();

        for branch in branch_desc {
            if let Some(name) = branch.get_expr_at_pos(pos) {
                if name.is_empty() {
                    return String::from("ρ");
                }
                if arg_name.is_empty() {
                    arg_name = name;
                } else {
                    if arg_name.find(&name).is_some() {
                        arg_name = name;
                    } else if name.find(&arg_name).is_some() {
                        continue;
                    } else {
                        return String::from("ρ");
                    }
                }
            } else {
                return String::from("ρ");
            };
        }

        if arg_name.is_empty() { String::from("ρ") } else { arg_name }

    }

    fn set_arg_names(&mut self, branch_desc: &Vec<RetBranchDesc>) {
        if self.branch_info.len() == 0 {
            return;
        }
        let arg_num = self.branch_info[0].0;
        self.arg_names = (0..arg_num).map(
            |pos| {
                if let Some(branch_id_pos) = self.branch_id_pos {
                    if pos == branch_id_pos {
                        String::from("branch_id")
                    } else if pos < branch_id_pos {
                        RetArgs::get_arg_name_at_pos(branch_desc, pos)
                    } else {
                        RetArgs::get_arg_name_at_pos(branch_desc, pos - 1)
                    }
                } else {
                    RetArgs::get_arg_name_at_pos(branch_desc, pos)
                }
            }).collect();

        // Duplicate names should be assigned a subscript.
        let mut name_counts: HashMap<&String, usize> = HashMap::new();
        self.arg_names = self.arg_names.iter().map(
            |name| {
                let count = name_counts.entry(name).or_insert(0);
                *count += 1;
                name.clone() + &make_subscript(*count - 1)
            }).collect();
    }

    pub fn set_implicit_ret_arg_num(&mut self) {
        if 0 == self.branch_info.len() {
            self.num_implicit_ret_args = 0;
            return;
        }

        if let Some(branch_id_pos) = self.branch_id_pos {
            self.num_implicit_ret_args = branch_id_pos;
            return;
        }

        // List of variable names which are assumed to be implicit arguments.
        let implicit_args = vec!["range_check"];

        if let Some(pos) = self.arg_names.iter().position(
            |name| { !implicit_args.contains(&name.as_str()) }
        ) {
            self.num_implicit_ret_args = pos;
        } else {
            self.num_implicit_ret_args = 0;
        }
    }
}

/// Identifies the statements at the end of the function which copy the return variables to the end
/// of the stack. Returns a vector of tuples where the first element of the tuple is the number of statements in
/// the branch return block and the second is the position of the branch ID return argument inside that block.
fn get_ret_blocks(aux_info: &CasmBuilderAuxiliaryInfo, cairo_program: &CairoProgram) -> Vec<(usize, Option<usize>)> {
    let mut count = 0;
    let mut branch_id_pos: Option<usize> = None;
    let mut branch_counts: Vec<(usize, Option<usize>)> = Vec::new();

    let set_branch =
        |branch_counts: &mut Vec<(usize, Option<usize>)>, count: &mut usize, branch_id_pos: &mut Option<usize>| {
            if *count != 0 {
                match *branch_id_pos {
                    Some(pos) => branch_counts.push((*count, Some(*count - pos - 1))),
                    None => branch_counts.push((*count, None)),
                };
                *count = 0;
                *branch_id_pos = None;
            }
        };

    for pos in (aux_info.core_libfunc_instr_num..cairo_program.instructions.len()).rev() {

        let branch_num = aux_info.return_args.len() - 1 - branch_counts.len();

        // A branch return block ends with a jmp or return.
        match &cairo_program.instructions[pos].body {
            InstructionBody::Ret(_) | InstructionBody::Jump(_) | InstructionBody::AddAp(_) => {
                set_branch(&mut branch_counts, &mut count, &mut branch_id_pos);
            },
            InstructionBody::AssertEq(assert_eq) => {
                match &assert_eq.b {
                    ResOperand::Immediate(value) => {
                        if *value == BigIntAsHex::from(branch_num) {
                            branch_id_pos = Some(count);
                        }
                    },
                    _ => { }
                }
                count += 1;
             },
            _ => { count += 1; }
        }
    }

    set_branch(&mut branch_counts, &mut count, &mut branch_id_pos);

    branch_counts.reverse();
    branch_counts
}

fn check_ret_blocks(ret_blocks: &Vec<(usize, Option<usize>)>, branch_num: usize) {
    assert!(branch_num == ret_blocks.len(), "branch_num != ret_blocks.len()");

    if branch_num == 0 {
        return;
    }

    // check that all branches have the same number of statements
    let mut count = 0;
    for (num, _) in ret_blocks {
        if count == 0 {
            count = *num;
        } else {
            assert!(count == *num, "count != *num");
        }
    }
    // check that all branches have the same branch ID position.
    let branch_id_pos = ret_blocks[0].1;
    for (_, pos) in ret_blocks {
        match pos {
            Some(pos) => {
                match branch_id_pos {
                    Some(branch_id_pos) => assert!(branch_id_pos == *pos, "branch_id_pos != *pos"),
                    None => {
                        assert!(false, "only some branches with branch ID position");
                    }
                }
            },
            None => { assert!(branch_id_pos == None, "only some branches with branch ID position"); }
        }
    }
}

/// Rebinding information for variables.
#[derive(Clone)]
struct VarRebind {
    /// Number of rebindings for each variable. 0 is for the first variable,
    /// 1 for the bnext one, etc.
    vars: HashMap<String, usize>,
}

impl VarRebind {
    pub fn new() -> Self {
        Self { vars: HashMap::new() }
    }

    pub fn get_var_name(&self, name: &str) -> String {
        let count = self.vars.get(name).unwrap_or(&0);
        if *count == 0 {
            name.to_string()
        } else {
            format!("{}{}", name, make_subscript(*count))
        }
    }

    pub fn rebind(&mut self, name: &str) -> usize {
        *self.vars.entry(name.to_string())
        .and_modify(|c| *c += 1)
        .or_insert(0)
    }

    pub fn get_rebind_name(&mut self, name: &str) -> String {
        self.rebind(name);
        self.get_var_name(name)
    }

    pub fn replace_var_names_in_expr(&self, expr: &str) -> String {
        let pat = &[' ', '+', '-', '*', '/', '(', ')'];
        expr.split_inclusive(pat)
            .map(|s| {
                if let Some(name) = s.split(pat).next() {
                    s.replace(name, &self.get_var_name(name))
                } else {
                    s.to_string()
                }
            })
            .join("")
    }
}

/// Expression formatting.

fn ref_to_lean(cell_ref: &CellRef, paren: bool) -> String {
    let reg = cell_ref.register;
    if cell_ref.offset == 0 {
        format!("σ.{reg}")
    } else if paren {
        ref_to_lean(cell_ref, false)
    } else if cell_ref.offset < 0 {
        format!("σ.{reg} - {offset}", offset = -cell_ref.offset)
    } else {
        format!("σ.{reg} + {offset}", offset = cell_ref.offset)
    }
}

fn cell_expr_to_lean(expr: &CellExpression, paren: bool) -> String {

    if paren {
        return match expr {
            // Always adds parentheses, so avoid adding double parentheses.
            CellExpression::Immediate(_) => cell_expr_to_lean(expr, false),
            _ => format!("({})", cell_expr_to_lean(expr, false)),
        };
    }

    match expr {
        CellExpression::Deref(cell_ref) => {
            format!("mem {ref_str}", ref_str = ref_to_lean(cell_ref, true))
        },
        CellExpression::DoubleDeref(cell_ref, offset) => {
            if *offset == 0 {
                format!("mem (mem {ref_str})", ref_str = ref_to_lean(cell_ref, true))
            } else if *offset < 0 {
                format!("mem ((mem {ref_str}) - {offset})", ref_str = ref_to_lean(cell_ref, true), offset = -*offset)
            } else {
                format!("mem ((mem {ref_str}) + {offset})", ref_str = ref_to_lean(cell_ref, true), offset = *offset)
            }
        },
        CellExpression::Immediate(value) => {
            format!("({value} : F)")
        },
        CellExpression::BinOp { op, a, b } => {
            let op_str = match op {
                CellOperator::Add => "+",
                CellOperator::Sub => "-",
                CellOperator::Mul => "*",
                CellOperator::Div => "/",
            };
            format!("({a_str} {op_str} {b_str})", a_str = ref_to_lean(a, true), b_str = deref_or_imm_to_lean(b, true))
        }
    }
}

fn deref_or_imm_to_lean(expr: &DerefOrImmediate, paren: bool) -> String {
    match expr {
        DerefOrImmediate::Deref(cell_ref) => ref_to_lean(cell_ref, paren),
        DerefOrImmediate::Immediate(value) => format!("({} : F)", value.value),
    }
}

/// All the information needed to generate the Lean code for a single function.
struct LeanFuncInfo<'a> {
    func_name: String,
    aux_info: &'a CasmBuilderAuxiliaryInfo,
    ret_args: RetArgs,
}

impl<'a> LeanFuncInfo<'a> {

        pub fn new(
            func_name: String,
            aux_info: &'a CasmBuilderAuxiliaryInfo,
            cairo_program: &CairoProgram
        ) -> Self {
            let ret_args = LeanFuncInfo::make_ret_args(&aux_info, cairo_program);
            Self { func_name, aux_info, ret_args }
        }

        pub fn is_range_check_var(&self, name: &str) -> bool {
            name == "range_check"
        }

        pub fn make_ret_args(aux_info: &CasmBuilderAuxiliaryInfo, cairo_program: &CairoProgram) -> RetArgs {
            let ret_blocks = get_ret_blocks(aux_info, cairo_program);
            check_ret_blocks(&ret_blocks, aux_info.return_args.len());

            let branch_id_pos = if 0 < ret_blocks.len() { ret_blocks[0].1 } else { None };
            let mut ret_args = RetArgs {
                branch_info: ret_blocks,
                arg_names: Vec::new(),
                branch_id_pos: branch_id_pos,
                num_implicit_ret_args: 0, // will be set later
            };
            ret_args.set_arg_names(&aux_info.return_args);
            ret_args.set_implicit_ret_arg_num();
            ret_args
        }

        pub fn has_args(&self) -> bool {
            0 < self.aux_info.args.len()
        }

        /// Returns the list of all argument names.
        pub fn get_arg_names(&self) -> Vec<String> {
            self.aux_info.args.iter().map(
                |v| -> String {
                    if let Some(arg) = self.aux_info.var_names.get(&v.0) {
                        String::from(arg)
                    } else {
                        String::from("")
                    }
                }
            ).collect()
        }

        /// Returns all arguments with the Lean expression which defines their value.
        fn get_arg_names_and_expr(&self) -> Vec<(String, String)> {
            self.aux_info.args.iter().map(
                |v| -> (String, String) {
                    if let Some(arg) = self.aux_info.var_names.get(&v.0) {
                        (String::from(arg), cell_expr_to_lean(&v.1, false))
                    } else {
                        ("".into(), "".into())
                    }
                }
            ).collect()
        }

        pub fn has_range_check_arg(&self) -> bool {
            self.get_arg_names().iter().any(|a| self.is_range_check_var(a))
        }

        pub fn get_range_check_arg_name(&self) -> Option<String> {
            for (var, expr) in self.aux_info.args.iter() {
                if let Some(arg) = self.aux_info.var_names.get(var) {
                    if self.is_range_check_var(arg) {
                        return Some(arg.into());
                    }
                }
            }
            None
        }

        pub fn get_arg_expr(&self, name: &str, paren: bool) -> Option<String> {
            for (var, expr) in self.aux_info.args.iter() {
                if let Some(arg) = self.aux_info.var_names.get(var) {
                    if arg == name {
                        return Some(cell_expr_to_lean(expr, paren));
                    }
                }
            }
            None
        }

        pub fn implicit_ret_arg_num(&self) -> usize {
            self.ret_args.num_implicit_ret_args
        }

        pub fn explicit_ret_arg_num(&self) -> usize {
            self.ret_args.arg_names.len() - self.ret_args.num_implicit_ret_args
        }

        /// Returns the list of all return argument names, including the implicit ones.
        pub fn get_all_ret_arg_names(&self) -> Vec<String> {
            self.ret_args.arg_names.iter().map(
                |s| { if s.starts_with("ρ") { s.to_string() } else { "ρ_".to_string() + s } }
            ).collect()
        }

        /// The explicit return arguments are determined in two ways:
        /// 1. If there is a branch ID return argument, it is the first explicit return argument.
        /// 2. If there is no branch ID return argument, we skip specific argument names as implicit
        ///    (such as 'range_check').
        pub fn get_explicit_ret_arg_names(&self) -> Vec<String> {
            self.ret_args.arg_names[self.ret_args.num_implicit_ret_args..].iter().map(
                |s| { if s.starts_with("ρ") { s.to_string() } else { "ρ_".to_string() + s } }
            ).collect()
        }

        /// return the offset from the final ap of the given return argument. The last return argument is
        /// at offset 1, the second to last at offset 2, etc.
        pub fn get_ret_arg_offset(&self, name: &str) -> Option<usize> {
            match self.ret_args.arg_names.iter().position(|s| s == name) {
                Some(pos) => Some(self.ret_args.arg_names.len() - pos),
                _ => None
            }
        }

        // True if this expression represents a range check: x = [<range check pointer>]
        pub fn is_range_check(&self, expr: &ExprDesc) -> bool {
            expr.op == "*()" &&
            if let Some(str) = self.aux_info.var_names.get(&expr.var_a) {
                self.is_range_check_var(str)
            } else { false }
        }

        // True if this expression increments the range check pointer.
        pub fn is_range_check_inc(&self, expr: &ExprDesc) -> bool {
            expr.op == "++" &&
            if let Some(str) = self.aux_info.var_names.get(&expr.var_a) {
                self.is_range_check_var(str)
            } else { false }
        }

}

// Rebinding variables and assignments

fn rebind_var_assignment(
    var: &VarDesc,
    lean_info: &LeanFuncInfo,
    rebind: &mut VarRebind,
) -> (String, Option<String>) {
    // Must rewrite variable names with the rebind subscripts before rebinding the lhs.
    let expr = if let Some(expr) = &var.expr {
        Some(rebind.replace_var_names_in_expr(&expr.expr))
    } else { None };

    (rebind.get_rebind_name(&var.name), expr)
}

fn rebind_assignment(
    assignment: &AssertDesc,
    lean_info: &LeanFuncInfo,
    rebind: &mut VarRebind,
) -> (String, String) {
    // Must rewrite the rhs before the lhs.
    let rhs_str = rebind.replace_var_names_in_expr(&assignment.expr.expr);
    (rebind.get_rebind_name(&assignment.lhs), rhs_str)
}

// Soundness generation functions

fn generate_soundness_prelude(lean_info: &LeanFuncInfo) -> Vec<String> {
    let mut prelude: Vec<String> = Vec::new();
    prelude.push(String::from("import starkware.cairo.lean.semantics.soundness.prelude"));
    prelude.push(format!("import .{}", lean_code_file_name(&lean_info.func_name[..])));
    prelude.push(String::from("open tactic"));
    prelude.push(String::from(""));
    prelude.push(String::from("variables {F : Type} [field F] [decidable_eq F]"));
    prelude.push(String::from("variable  mem : F → F"));
    prelude.push(String::from("variable  σ : register_state F"));
    prelude.push(String::from(""));

    prelude
}

fn generate_consts(lean_info: &LeanFuncInfo) -> Vec<String> {
    let mut consts: Vec<String> = Vec::new();

    for const_def in &lean_info.aux_info.consts {

        // let expr = const_def.expr.replace("::", "_");
        if let CellExpression::Immediate(value) = &const_def.value {
            consts.push(format!(
                "def {const_name} : F := ({value_str} : F) -- {expr}",
                const_name = const_def.name,
                value_str = value.to_string(),
                expr = const_def.expr,
            ));
        } else {
            consts.push(format!(
                "def {const_name} : F := {expr}",
                const_name = const_def.name,
                expr = const_def.expr,
            ));
        }
    }

    consts
}

trait LeanGenerator {

    /// Return a generator for a branch.
    fn branch(&self) -> Box<dyn LeanGenerator>;
    /// Integrates the Lean code generated by a branch generator into the
    /// main Lean code generator.
    fn append_branch(&mut self, branch: &Box<dyn LeanGenerator>);

    /// Returns the Lean code generated by this generator.
    fn get_lean(&self) -> &Vec<String>;

    fn generate_statement(
        &mut self,
        func_name: &str,
        lean_info: &LeanFuncInfo,
        indent: usize,
    );

    fn generate_intro(
        &mut self,
        func_name: &str,
        lean_info: &LeanFuncInfo,
        indent: usize,
    );

    fn generate_temp_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    );

    fn generate_local_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    );

    fn generate_let(
        &mut self,
        assign: &AssertDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    );

    fn generate_assert(
        &mut self,
        assign: &AssertDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    );

    /// Returns the indentation after the branch intro.
    fn generate_branch_intro(
        &mut self,
        cond_var: &(String, Var),
        is_eq: bool,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) -> usize;

    /// Returns the indentation after the branch close.
    fn generate_branch_close(
        &mut self,
        is_eq: bool,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) -> usize;

    fn generate_return_args(
        &mut self,
        branch_id: usize,
        ret_arg_names: &Vec<String>,
        ret_exprs: &Vec<String>,
        lean_info: &LeanFuncInfo,
        rebind: &VarRebind,
        indent: usize,
    );

}

struct AutoSpecs {
    specs: Vec<String>,
}

impl AutoSpecs {
    pub fn new() -> AutoSpecs {
        AutoSpecs {
            specs: Vec::new(),
        }
    }

    fn push(&mut self, indent: usize, str: &str) {
        self.specs.push(" ".repeat(indent) + str);
    }

    fn continue_specs(&mut self) {
        if let Some(last) = self.specs.last_mut() {
            if !last.ends_with("=") && !last.ends_with(',') && !last.ends_with('∨') && !last.ends_with('(') {
                last.push_str(" ∧")
            }
        }
    }

    fn push_spec(&mut self, indent: usize, str: &str) {
        if !str.starts_with(')') {
            self.continue_specs();
        }
        self.specs.push(" ".repeat(indent) + str);
    }

    fn append_spec(&mut self, more_specs: &Vec<String>) {
        self.continue_specs();
        self.specs.append(&mut more_specs.clone());
    }

    fn generate_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        let (var_name, expr) = rebind_var_assignment(var, lean_info, rebind);
        self.push_spec(
            indent,
            &format!(
                "∃ {var_name} : F,{expr}",
                expr=if let Some(expr_str) = &expr {
                    format!(" {var_name} = {expr_str}")
                } else { String::new() }
            ));
    }
}

impl LeanGenerator for AutoSpecs {

    /// Return a generator for a branch.
    fn branch(&self) -> Box<dyn LeanGenerator> {
        Box::new(AutoSpecs::new())
    }

    fn append_branch(&mut self, branch: &Box<dyn LeanGenerator>) {
        self.append_spec(branch.get_lean());
    }

    fn get_lean(&self) -> &Vec<String> {
        &self.specs
    }

    fn generate_statement(
        &mut self,
        func_name: &str,
        lean_info: &LeanFuncInfo,
        indent: usize,
    ) {
        let args_str =
            lean_info.get_arg_names().iter().chain(
                lean_info.get_explicit_ret_arg_names().iter()
            ).join(" ");

        self.push(
            indent,
            &format!("def auto_spec_{func_name} (mem : F → F) (κ : ℕ) ({args_str} : F) : Prop :="));
    }

    fn generate_intro(
        &mut self,
        func_name: &str,
        lean_info: &LeanFuncInfo,
        indent: usize,
    ) {
        // Does nothing here.
    }

    fn generate_temp_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        self.generate_var(var, lean_info, rebind, indent);
    }

    fn generate_local_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        self.generate_var(var, lean_info, rebind, indent);
    }

    fn generate_let(
        &mut self,
        assign: &AssertDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        // The range check increment is not part of the specification.
        if lean_info.is_range_check_inc(&assign.expr) {
            return;
        }
        let (lhs, rhs) = rebind_assignment(assign, lean_info, rebind);
        self.push_spec(indent, &format!("∃ {lhs} : F, {lhs} = {rhs}"));
    }

    fn generate_assert(
        &mut self,
        assert: &AssertDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        if lean_info.is_range_check(&assert.expr) {
            self.push_spec(
                indent,
                &format!("is_range_checked (rc_bound F) {var_name}",
                    var_name = rebind.get_var_name(&assert.lhs)));
        } else {
            self.push_spec(
                indent,
                &format!("{var_name} = {expr}",
                    var_name = rebind.get_var_name(&assert.lhs),
                    expr = rebind.replace_var_names_in_expr(&assert.expr.expr)));
        }
    }

    fn generate_branch_intro(
        &mut self,
        cond_var: &(String, Var),
        is_eq: bool,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) -> usize {

        // Open the or block only if this is the first branch.
        let indent = if is_eq {
            self.push_spec(indent, "(");
            indent + 2
        } else {
            indent
        };

        let var_name = rebind.get_var_name(&cond_var.0);
        self.push_spec(
            indent,
            &format!("({var_name} {op} 0", op = if is_eq { "=" } else { "≠" }));

        indent
    }

    fn generate_branch_close(
        &mut self,
        is_eq: bool,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) -> usize {
        if is_eq {
            self.push_spec(indent, ") ∨");
            indent
        } else {
            self.push_spec(indent, ")");
            let indent = indent - 2;
            self.push_spec(indent, ")");
            indent
        }
    }

    fn generate_return_args(
        &mut self,
        branch_id: usize,
        ret_arg_names: &Vec<String>,
        ret_exprs: &Vec<String>,
        lean_info: &LeanFuncInfo,
        rebind: &VarRebind,
        indent: usize,
    ) {
        let mut ret_arg_start = 0;

        if lean_info.ret_args.branch_id_pos.is_some() {
            // The first return argument is the branch ID
            self.push_spec(
                indent,
                &format!("{ret_arg} = {branch_id}", ret_arg = &ret_arg_names[0]));
            ret_arg_start = 1;
        }

        for (name, expr) in ret_arg_names[ret_arg_start..].iter().zip(
            ret_exprs[lean_info.ret_args.num_implicit_ret_args..].iter()
        ) {
            let expr = rebind.replace_var_names_in_expr(expr);
            self.push_spec(indent, &format!("{name} = {expr}"));
        }
    }

}

struct AutoProof {
    statement: Vec<String>,
    main_proof: Vec<String>,
    final_proof: Vec<String>,
}

impl AutoProof {
    pub fn new() -> AutoProof {
        AutoProof {
            statement: Vec::new(),
            main_proof: Vec::new(),
            final_proof: Vec::new(),
        }
    }

    fn get_full_proof(&self) -> Vec<String> {
        let mut full_proof = self.statement.clone();
        full_proof.push("begin".to_string());
        full_proof.append(&mut self.main_proof.clone());
        full_proof.push("end".to_string());
        full_proof
    }

    fn push_statement(&mut self, indent: usize, str: &str) {
        self.statement.push(" ".repeat(indent) + str);
    }

    fn push_main(&mut self, indent: usize, str: &str) {
        self.main_proof.push(" ".repeat(indent) + str);
    }

    fn push_final(&mut self, indent: usize, str: &str) {
        self.final_proof.push(" ".repeat(indent) + str);
    }

    fn append_proof(&mut self, more_proof: &Vec<String>) {
        self.main_proof.append(&mut more_proof.clone());
    }
}

impl LeanGenerator for AutoProof {

    /// Return a generator for a branch.
    fn branch(&self) -> Box<dyn LeanGenerator> {
        let mut branch = Box::new(AutoProof::new());
        branch.final_proof = self.final_proof.clone();
        branch
    }

    fn append_branch(&mut self, branch: &Box<dyn LeanGenerator>) {
        self.append_proof(branch.get_lean());
    }

    fn get_lean(&self) -> &Vec<String> {
        &self.main_proof
    }

    fn generate_statement(
        &mut self,
        func_name: &str,
        lean_info: &LeanFuncInfo,
        indent: usize,
    ) {
        self.push_statement(indent, &format!("theorem sound_{func_name}"));

        let mut indent = indent + 2;

        let args_str = lean_info.get_arg_names().join(" ");

        // Arguments and hypotheses.

        if lean_info.has_args() {
            self.push_statement(indent, "-- arguments");
            self.push_statement(indent, &format!("({args_str} : F)"));
        }

        self.push_statement(indent, "-- code is in memory at s.pc");
        self.push_statement(indent, &format!("(h_mem : mem_at mem code_{func_name} σ.pc)"));

        if lean_info.has_args() {
            self.push_statement(indent, "-- input arguments on the stack");
            for (arg_name, arg_expr) in lean_info.get_arg_names_and_expr() {
                if arg_name.len() == 0 {
                    continue;
                }
                self.push_statement(indent, &format!("(hin_{arg_name} : {arg_name} = {arg_expr})"))
            }
        }

        // Conclusion
        self.push_statement(indent, "-- conclusion");
        self.push_statement(indent, ": ensures_ret mem σ (λ κ τ,");

        let range_check_arg_name = lean_info.get_range_check_arg_name();
        let has_rc = range_check_arg_name.is_some();

        if let Some(rc_name) = range_check_arg_name {
            indent += 2;
            self.push_statement(
                indent,
                &format!(
                    "∃ μ ≤ κ, rc_ensures mem (rc_bound F) μ {rc_expr} (mem (τ.ap - {rc_ret_offset})))",
                    rc_expr = lean_info.get_arg_expr(&rc_name, true).unwrap(),
                    rc_ret_offset = lean_info.get_ret_arg_offset(&rc_name).unwrap()),
            );
        }

        indent += 2;

        self.push_statement(
            indent,
            &format!(
                "{open_par}spec_{func_name} mem κ{sep}{args_str}{ret_arg_offsets}{close_par}) :=",
                open_par = if has_rc { "(" } else { "" },
                sep = if 0 < args_str.len() { " " } else { "" },
                func_name = func_name,
                ret_arg_offsets = (0..lean_info.explicit_ret_arg_num()).rev()
                    .map(|i| format!(" (mem (τ.ap - {}))", i + 1))
                    .collect::<Vec<String>>()
                .   join(""),
                close_par = if has_rc { ")" } else { "" },
        ));
    }

    fn generate_intro(
        &mut self,
        func_name: &str,
        lean_info: &LeanFuncInfo,
        indent: usize,
    ) {
        self.push_main(indent, "apply ensures_of_ensuresb, intro νbound,");
        self.push_main(indent, &format!("unpack_memory code_{func_name} at h_mem with ⟨"));
    }

    fn generate_temp_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        // xxxxxxxx not complete yet
        let (var_name, expr) = rebind_var_assignment(var, lean_info, rebind);
        let has_expr = expr.is_some();
        self.push_main(indent, &format!("mkdef hl_{var_name} : {var_name} = {expr_str},",
            expr_str = if let Some(expr) = expr { expr } else { cell_expr_to_lean(&var.var_expr, false) }));
        if has_expr {
            self.push_main(indent,
                &format!(
                    "have htv_{var_name} : {var_name} = {cell_expr},",
                    cell_expr = cell_expr_to_lean(&var.var_expr, false)));
            self.push_main(indent, "{ sorry },")
        }
        self.push_final(0, &format!("use_only hl_{var_name},"));
    }

    fn generate_local_var(
        &mut self,
        var: &VarDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        // xxxxxxxxxxxxx
    }

    fn generate_let(
        &mut self,
        assign: &AssertDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        let (lhs, rhs) = rebind_assignment(assign, lean_info, rebind);
        self.push_main(indent, &format!("mkdef hl_{lhs} : {lhs} = {rhs},"));
        // xxxxxxxxxxxxxxxx not complete yet
    }

    fn generate_assert(
        &mut self,
        assert: &AssertDesc,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) {
        // xxxxxxxxxxxxxxxx
    }

    fn generate_branch_intro(
        &mut self,
        cond_var: &(String, Var),
        is_eq: bool,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) -> usize {
        // xxxxxxxxxxxxxx
        indent
    }

    fn generate_branch_close(
        &mut self,
        is_eq: bool,
        lean_info: &LeanFuncInfo,
        rebind: &mut VarRebind,
        indent: usize,
    ) -> usize {
        // xxxxxxxxxxx
        indent
    }

    fn generate_return_args(
        &mut self,
        branch_id: usize,
        ret_arg_names: &Vec<String>,
        ret_exprs: &Vec<String>,
        lean_info: &LeanFuncInfo,
        rebind: &VarRebind,
        indent: usize,
    ) {
        // xxxxxxxxxxxxxxxxx

        // As a last step, add the final proof to the main proof (with indentation).
        for line in self.final_proof.clone() {
            self.push_main(indent, &line);
        }
    }

}

fn generate_soundness_user_spec(lean_info: &LeanFuncInfo) -> Vec<String> {
    let func_name = &lean_info.func_name[..];
    let args_str =
        lean_info.get_arg_names().iter().chain(lean_info.get_explicit_ret_arg_names().iter()).join(" ");

    let mut user_spec: Vec<String> = Vec::new();
    user_spec.push(format!("def spec_{func_name} (mem : F → F) (κ : ℕ) ({args_str} : F) : Prop :="));
    user_spec.push(String::from("  true"));

    user_spec
}

fn generate_soundness_auto_spec(lean_info: &LeanFuncInfo) -> Vec<String> {
    let mut auto_specs = AutoSpecs::new();
    generate_auto(&mut auto_specs, lean_info);

    auto_specs.specs
}

fn generate_soundness_auto_theorem(lean_info: &LeanFuncInfo) -> Vec<String> {
    let mut auto_theorem = AutoProof::new();
    generate_auto(&mut auto_theorem, lean_info);

    auto_theorem.get_full_proof()
}

fn generate_auto(lean_gen: &mut dyn LeanGenerator, lean_info: &LeanFuncInfo) {
    let func_name = &lean_info.func_name[..];
    let mut rebind = VarRebind::new();
    for arg in lean_info.get_arg_names() {
        rebind.rebind(&arg);
    }

    lean_gen.generate_statement(func_name, lean_info, 0);
    lean_gen.generate_intro(func_name, lean_info, 2);
    generate_auto_block(lean_gen, lean_info, &mut rebind, 0, 2);
}

fn generate_auto_block(
    lean_gen: &mut dyn LeanGenerator,
    lean_info: &LeanFuncInfo,
    rebind: &mut VarRebind,
    block_start: usize,
    indent: usize
) {
    for (i, statement) in lean_info.aux_info.statements[block_start..].iter().enumerate() {
        match statement {
            StatementDesc::TempVar(var) => {
                lean_gen.generate_temp_var(var, lean_info, rebind, indent);
            },
            StatementDesc::LocalVar(var) => {
                lean_gen.generate_local_var(var, lean_info, rebind, indent);
            },
            StatementDesc::Let(assign) => {
                lean_gen.generate_let(assign, lean_info, rebind, indent);
            },
            StatementDesc::Assert(assert) => {
                lean_gen.generate_assert(assert, lean_info, rebind, indent);
            },
            StatementDesc::Jump(jump) => {
                let pos = lean_info.aux_info.statements.iter().position(
                    |s|
                        if let StatementDesc::Label(label) = s {
                            label == &jump.target
                        } else {
                            false
                        }
                );

                // The jump target may be a return branch name.
                if pos.is_none() {
                    generate_auto_ret_block(
                        lean_gen, lean_info, rebind, &jump.target, indent);
                    return;
                }

                let pos = pos.unwrap();
                assert!(block_start + i < pos, "No backward jumps supported.");

                match &jump.cond_var {
                    Some(cond_var) => {
                        // Conditional jump, must generate both branches.

                        // First (equals zero) branch

                        let mut branch = lean_gen.branch();

                        let indent = branch.generate_branch_intro(
                                cond_var, true, lean_info, rebind, indent);

                        // Clone the rebind object before entering the branch, as the second branch
                        // should be independent of the rebinding accumulated in the first branch.
                        generate_auto_block(
                                &mut *branch,
                                lean_info,
                                &mut rebind.clone(),
                                block_start + i + 1,
                                indent + 2);

                        lean_gen.append_branch(&branch);

                        let indent = lean_gen.generate_branch_close(
                            true, lean_info, rebind, indent);

                        // Second (not equals zero) branch

                        let indent = lean_gen.generate_branch_intro(
                            cond_var, false, lean_info, rebind, indent);

                        generate_auto_block(
                            lean_gen,
                            lean_info,
                            rebind,
                            pos + 1,
                            indent + 2);

                        lean_gen.generate_branch_close(
                            false, lean_info, rebind, indent);
                    },
                    _ => {
                        // Unconditional jump, must generate only the target block.
                        generate_auto_block(lean_gen, lean_info, rebind, pos + 1, indent);
                    }
                };
                return; // Remain code handled already in the branches.
            },
            StatementDesc::ApPlus(step) => {}, // Do nothing
            StatementDesc::Label(label) => {}, // Do nothing
        }
    }

    generate_auto_ret_block(lean_gen, lean_info, rebind, "Fallthrough", indent);
}

fn generate_auto_ret_block(
    lean_gen: &mut dyn LeanGenerator,
    lean_info: &LeanFuncInfo,
    rebind: &VarRebind,
    ret_block_name: &str,
    indent: usize
) {
    let branch_id = lean_info.aux_info.return_args.iter().position(
        |b| b.name == ret_block_name
    ).expect("Could not find return block.");

    let ret_arg_names = lean_info.get_explicit_ret_arg_names();
    let flat_exprs = lean_info.aux_info.return_args[branch_id].flat_exprs();

    lean_gen.generate_return_args(
        branch_id, &ret_arg_names, &flat_exprs, lean_info, rebind, indent);
}

pub fn generate_lean_soundness(test_name: &str, cairo_program: &CairoProgram) -> String {

    let aux_info = match cairo_program.aux_info.as_ref() {
        Some(info) => info,
        _ => { return String::from("-- Could not find lean info."); }
    };

    let lean_info = LeanFuncInfo::new(
        test_name.split_whitespace().next().unwrap().to_string(),
        aux_info,
        cairo_program
    );

    let mut soundness: Vec<String> = Vec::new();
    // Generate the prelude
    soundness.append(generate_soundness_prelude(&lean_info).as_mut());

    // Generate the constant definitions
    soundness.append(generate_consts(&lean_info).as_mut());
    soundness.push(String::from(""));

    // Generate the user spec
    soundness.append(generate_soundness_user_spec(&lean_info).as_mut());
    soundness.push(String::from(""));
    // Generate the auto spec
    soundness.append(generate_soundness_auto_spec(&lean_info).as_mut());

    // Gneerate the soundness proof
    soundness.push(String::from(""));
    soundness.append(generate_soundness_auto_theorem(&lean_info).as_mut());

    soundness.join("\n")
}

pub fn write_lean_soundness_file(test_path: &Path, test_name: &str, soundness: Option<&String>) -> Result<(), std::io::Error> {
    let soundness_str = match soundness { Some(content) => { content }, _ => { return Ok(()); }};
    let func_name = test_name.split_whitespace().next().unwrap();
    let lean_path = lean_verification_path(test_path);
    let soundness_file_path = lean_file_path(&lean_path, &lean_soundness_file_name(func_name));
    fs::create_dir_all(lean_path)?;
    fs::write(soundness_file_path, soundness_str)
}

// Old code

/// Generates the lean assembly code file for a single function.
pub fn generate_lean_casm(
    test_path: &Path, test_name: &str, casm: Option<&String>
) -> Result<(), std::io::Error> {

    let func_name = test_name.split_whitespace().next().unwrap();
    let lean_path = lean_verification_path(test_path);
    let code_file_path = lean_file_path(&lean_path, &lean_code_file_name(func_name));

    fs::create_dir_all(lean_path)?;

    let mut lean_casm = String::new();
    lean_casm.push_str("import starkware.cairo.lean.semantics.soundness.hoare\n\n");
    lean_casm.push_str("variables {F : Type} [field F] [decidable_eq F]\n\n");
    let casm_body = format!("def code_{func_name} : list F := [\n");
    lean_casm.push_str(&casm_body);
    // Temporary, not valid Lean yet.
    lean_casm.push_str(
        match casm {
            Some(casm_str) => casm_str,
            _ => "-- No casm generated for this function.",
        }
    );
    lean_casm.push_str("]\n");

    // For debugging!
    println!("{}", lean_casm);

    fs::write(code_file_path, lean_casm)

}

/* pub fn dump_to_test_file(
    tests: OrderedHashMap<String, Test>,
    filename: &str,
) -> Result<(), std::io::Error> {
    let mut test_strings = Vec::new();
    for (test_name, test) in tests {
        let mut tag_strings = vec![TAG_PREFIX.to_string() + &test_name];
        for (tag, content) in test.attributes {
            tag_strings.push(
                TAG_PREFIX.to_string()
                    + &tag
                    + if content.is_empty() { "" } else { "\n" }
                    + &content,
            );
        }
        test_strings.push(tag_strings.join("\n\n"));
    }
    fs::write(
        filename,
        test_strings.join(&("\n\n".to_string() + TAG_PREFIX + TEST_SEPARATOR + "\n\n")) + "\n",
    )
}*/