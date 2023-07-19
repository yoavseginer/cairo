use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::env::Vars;

use cairo_lang_utils::casts::IntoOrPanic;
use cairo_lang_utils::extract_matches;
use num_bigint::BigInt;
use num_traits::One;

use crate::ap_change::ApplyApChange;
use crate::cell_expression::{CellExpression, CellOperator};
use crate::deref_or_immediate;
use crate::hints::Hint;
use crate::instructions::{
    AddApInstruction, AssertEqInstruction, CallInstruction, Instruction, InstructionBody,
    JnzInstruction, JumpInstruction, RetInstruction,
};
use crate::operand::{BinOpOperand, CellRef, DerefOrImmediate, Operation, Register, ResOperand};

#[cfg(test)]
#[path = "builder_test.rs"]
mod test;

/// Variables for casm builder, representing a `CellExpression`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Var(usize);

/// The state of the variables at some line.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct State {
    /// The value per variable.
    vars: HashMap<Var, CellExpression>,
    /// The number of allocated variables from the beginning of the run.
    allocated: i16,
    /// The AP change since the beginning of the run.
    pub ap_change: usize,
    /// The number of casm steps since the beginning of the run.
    pub steps: usize,
}
impl State {
    /// Returns the value, in relation to the initial ap value.
    fn get_value(&self, var: Var) -> CellExpression {
        self.vars[&var].clone()
    }

    /// Returns the value, in relation to the current ap value.
    pub fn get_adjusted(&self, var: Var) -> CellExpression {
        self.get_value(var).unchecked_apply_known_ap_change(self.ap_change)
    }

    /// Returns the value, assuming it is a direct cell reference.
    pub fn get_adjusted_as_cell_ref(&self, var: Var) -> CellRef {
        extract_matches!(self.get_adjusted(var), CellExpression::Deref)
    }

    /// Validates that the state is valid, as it had enough ap change.
    fn validate_finality(&self) {
        assert!(
            self.ap_change >= self.allocated.into_or_panic(),
            "Not enough instructions to update ap. Add an `ap += *` instruction. ap_change: {}, \
             allocated: {}",
            self.ap_change,
            self.allocated,
        );
    }

    /// Intersect the states of branches leading to the same label, validating that the states can
    /// intersect.
    fn intersect(&mut self, other: &Self, read_only: bool) {
        assert_eq!(self.ap_change, other.ap_change, "Merged branches not aligned on AP change.");
        assert_eq!(
            self.allocated, other.allocated,
            "Merged branches not aligned on number of allocations."
        );
        if read_only {
            assert!(self.steps >= other.steps, "Finalized branch cannot be updated.");
        } else {
            self.steps = self.steps.max(other.steps);
        }
        // Allowing removal of variables as valid code won't be producible in that case anyway, and
        // otherwise merging branches becomes very difficult.
        self.vars.retain(|var, value| {
            other
                .vars
                .get(var)
                .map(|x| assert_eq!(x, value, "Var mismatch between branches."))
                .is_some()
        });
    }
}

/// A statement added to the builder.
enum Statement {
    /// A final instruction, no need for further editing.
    Final(Instruction),
    /// A jump or call command, requires fixing the actual target label.
    Jump(String, Instruction),
    /// A target label for jumps.
    Label(String),
}

/// The builder result.
pub struct CasmBuildResult<const BRANCH_COUNT: usize> {
    /// The actual casm code.
    pub instructions: Vec<Instruction>,
    /// The state and relocations per branch.
    pub branches: [(State, Vec<usize>); BRANCH_COUNT],
}

/// Builder to more easily write casm code without specifically thinking about ap changes and the
/// sizes of opcodes. Wrong usages of it would panic instead of returning a result, as this builder
/// assumes we are in a post validation of parameters stage.
pub struct CasmBuilder {
    /// The state at a point of jumping into a label, per label.
    label_state: HashMap<String, State>,
    /// The set of labels that were already read, so cannot be updated.
    read_labels: HashSet<String>,
    /// The state at the last added statement.
    main_state: State,
    /// The added statements.
    statements: Vec<Statement>,
    /// The current set of added hints.
    current_hints: Vec<Hint>,
    /// The number of vars created. Used to not reuse var names.
    var_count: usize,
    /// Is the current state reachable.
    /// Example for unreachable state is after a unconditional jump, before any label is stated.
    reachable: bool,
     /// Auxiliary information gathered for verification.
     pub aux_info: Option<CasmBuilderAuxiliaryInfo>,
}
impl CasmBuilder {
    /// Finalizes the builder, with the requested labels as the returning branches.
    /// "Fallthrough" is a special case for the fallthrough case.
    pub fn build<const BRANCH_COUNT: usize>(
        mut self,
        branch_names: [&str; BRANCH_COUNT],
    ) -> CasmBuildResult<BRANCH_COUNT> {
        assert!(
            self.current_hints.is_empty(),
            "Build cannot be called with hints as the last addition."
        );
        let label_offsets = self.compute_label_offsets();
        if self.reachable {
            self.label_state.insert("Fallthrough".to_owned(), self.main_state);
        }
        let mut instructions = vec![];
        let mut branch_relocations = HashMap::<String, Vec<usize>>::default();
        let mut offset = 0;
        for statement in self.statements {
            match statement {
                Statement::Final(inst) => {
                    offset += inst.body.op_size();
                    instructions.push(inst);
                }
                Statement::Jump(label, mut inst) => {
                    match label_offsets.get(&label) {
                        Some(label_offset) => match &mut inst.body {
                            InstructionBody::Jnz(JnzInstruction {
                                jump_offset: DerefOrImmediate::Immediate(value),
                                ..
                            })
                            | InstructionBody::Jump(JumpInstruction {
                                target: DerefOrImmediate::Immediate(value),
                                ..
                            })
                            | InstructionBody::Call(CallInstruction {
                                target: DerefOrImmediate::Immediate(value),
                                ..
                            }) => {
                                // Updating the value, instead of assigning into it, to avoid
                                // allocating a BigInt since it is already 0.
                                value.value += *label_offset as i128 - offset as i128;
                            }

                            _ => unreachable!("Only jump or call statements should be here."),
                        },
                        None => match branch_relocations.entry(label) {
                            Entry::Occupied(mut e) => e.get_mut().push(instructions.len()),
                            Entry::Vacant(e) => {
                                e.insert(vec![instructions.len()]);
                            }
                        },
                    }
                    offset += inst.body.op_size();
                    instructions.push(inst);
                }
                Statement::Label(name) => {
                    self.label_state.remove(&name);
                }
            }
        }
        let branches = branch_names.map(|label| {
            let state = self
                .label_state
                .remove(label)
                .unwrap_or_else(|| panic!("Requested a non existing final label: {label:?}."));
            state.validate_finality();
            (state, branch_relocations.remove(label).unwrap_or_default())
        });
        assert!(self.label_state.is_empty(), "Did not use all branches.");
        assert!(branch_relocations.is_empty(), "Did not use all branch relocations.");
        CasmBuildResult { instructions, branches }
    }

    /// Computes the code offsets of all the labels.
    fn compute_label_offsets(&self) -> HashMap<String, usize> {
        let mut label_offsets = HashMap::<String, usize>::default();
        let mut offset = 0;
        for statement in &self.statements {
            match statement {
                Statement::Final(inst) | Statement::Jump(_, inst) => {
                    offset += inst.body.op_size();
                }
                Statement::Label(name) => {
                    label_offsets.insert(name.clone(), offset);
                }
            }
        }
        label_offsets
    }

    /// Adds a variable pointing to `value`.
    pub fn add_var(&mut self, value: CellExpression) -> Var {
        let var = Var(self.var_count);
        self.var_count += 1;
        self.main_state.vars.insert(var, value);
        var
    }

    /// Allocates a new variable in memory, either local (FP-based) or temp (AP-based).
    pub fn alloc_var(&mut self, local_var: bool) -> Var {
        let var = self.add_var(CellExpression::Deref(CellRef {
            offset: self.main_state.allocated,
            register: if local_var { Register::FP } else { Register::AP },
        }));
        self.main_state.allocated += 1;
        var
    }

    /// Returns an additional variable pointing to the same value.
    pub fn duplicate_var(&mut self, var: Var) -> Var {
        self.add_var(self.get_value(var, false))
    }

    /// Adds a hint, generated from `inputs` which are cell refs or immediates and `outputs` which
    /// must be cell refs.
    pub fn add_hint<
        const INPUTS_COUNT: usize,
        const OUTPUTS_COUNT: usize,
        THint: Into<Hint>,
        F: FnOnce([ResOperand; INPUTS_COUNT], [CellRef; OUTPUTS_COUNT]) -> THint,
    >(
        &mut self,
        f: F,
        inputs: [Var; INPUTS_COUNT],
        outputs: [Var; OUTPUTS_COUNT],
    ) {
        self.current_hints.push(
            f(
                inputs.map(|v| match self.get_value(v, true) {
                    CellExpression::Deref(cell) => ResOperand::Deref(cell),
                    CellExpression::DoubleDeref(cell, offset) => {
                        ResOperand::DoubleDeref(cell, offset)
                    }
                    CellExpression::Immediate(imm) => imm.into(),
                    CellExpression::BinOp { op, a: other, b } => match op {
                        CellOperator::Add => {
                            ResOperand::BinOp(BinOpOperand { op: Operation::Add, a: other, b })
                        }
                        CellOperator::Mul => {
                            ResOperand::BinOp(BinOpOperand { op: Operation::Mul, a: other, b })
                        }
                        CellOperator::Sub | CellOperator::Div => {
                            panic!("hints to non ResOperand references are not supported.")
                        }
                    },
                }),
                outputs.map(|v| self.as_cell_ref(v, true)),
            )
            .into(),
        );
    }

    /// Adds an assertion that `dst = res`.
    /// `dst` must be a cell reference.
    pub fn assert_vars_eq(&mut self, dst: Var, res: Var) {
        let a = self.as_cell_ref(dst, true);
        let b = self.get_value(res, true);
        let (a, b) = match b {
            CellExpression::Deref(cell) => (a, ResOperand::Deref(cell)),
            CellExpression::DoubleDeref(cell, offset) => (a, ResOperand::DoubleDeref(cell, offset)),
            CellExpression::Immediate(imm) => (a, imm.into()),
            CellExpression::BinOp { op, a: other, b } => match op {
                CellOperator::Add => {
                    (a, ResOperand::BinOp(BinOpOperand { op: Operation::Add, a: other, b }))
                }
                CellOperator::Mul => {
                    (a, ResOperand::BinOp(BinOpOperand { op: Operation::Mul, a: other, b }))
                }
                CellOperator::Sub => {
                    (other, ResOperand::BinOp(BinOpOperand { op: Operation::Add, a, b }))
                }
                CellOperator::Div => {
                    (other, ResOperand::BinOp(BinOpOperand { op: Operation::Mul, a, b }))
                }
            },
        };
        let instruction =
            self.next_instruction(InstructionBody::AssertEq(AssertEqInstruction { a, b }), true);
        self.statements.push(Statement::Final(instruction));
    }

    /// Writes and increments a buffer.
    /// Useful for RangeCheck and similar buffers.
    /// `buffer` must be a cell reference, or a cell reference with a small added constant.
    /// `value` must be a cell reference.
    pub fn buffer_write_and_inc(&mut self, buffer: Var, value: Var) {
        let (cell, offset) = self.buffer_get_and_inc(buffer);
        let location = self.add_var(CellExpression::DoubleDeref(cell, offset));
        self.assert_vars_eq(value, location);
    }

    /// Increments a buffer and allocates and returns variable pointing to its previous value.
    pub fn get_ref_and_inc(&mut self, buffer: Var) -> Var {
        let (cell, offset) = self.as_cell_ref_plus_const(buffer, 0, false);
        self.main_state.vars.insert(
            buffer,
            CellExpression::BinOp {
                op: CellOperator::Add,
                a: cell,
                b: deref_or_immediate!(BigInt::from(offset) + 1),
            },
        );
        self.add_var(CellExpression::DoubleDeref(cell, offset))
    }

    /// Increments a buffer and returning the previous value it pointed to.
    /// Useful for writing, reading and referencing values.
    /// `buffer` must be a cell reference, or a cell reference with a small added constant.
    fn buffer_get_and_inc(&mut self, buffer: Var) -> (CellRef, i16) {
        let (base, offset) = match self.get_value(buffer, false) {
            CellExpression::Deref(cell) => (cell, 0),
            CellExpression::BinOp {
                op: CellOperator::Add,
                a,
                b: DerefOrImmediate::Immediate(imm),
            } => (a, imm.value.try_into().expect("Too many buffer writes.")),
            _ => panic!("Not a valid buffer."),
        };
        self.main_state.vars.insert(
            buffer,
            CellExpression::BinOp {
                op: CellOperator::Add,
                a: base,
                b: deref_or_immediate!(offset + 1),
            },
        );
        (base, offset)
    }

    /// Increases AP by `size`.
    pub fn add_ap(&mut self, size: usize) {
        let instruction = self.next_instruction(
            InstructionBody::AddAp(AddApInstruction { operand: BigInt::from(size).into() }),
            false,
        );
        self.statements.push(Statement::Final(instruction));
        self.main_state.ap_change += size;
    }

    /// Increases the AP change by `size`, without adding an instruction.
    pub fn increase_ap_change(&mut self, amount: usize) {
        self.main_state.ap_change += amount;
        self.main_state.allocated += amount.into_or_panic::<i16>();
    }

    /// Returns a variable that is the `op` of `lhs` and `rhs`.
    /// `lhs` must be a cell reference and `rhs` must be deref or immediate.
    pub fn bin_op(&mut self, op: CellOperator, lhs: Var, rhs: Var) -> Var {
        self.add_var(CellExpression::BinOp {
            op,
            a: self.as_cell_ref(lhs, false),
            b: self.as_deref_or_imm(rhs, false),
        })
    }

    /// Returns a variable that is `[[var] + offset]`.
    /// `var` must be a cell reference, or a cell ref plus a small constant.
    pub fn double_deref(&mut self, var: Var, offset: i16) -> Var {
        let (cell, full_offset) = self.as_cell_ref_plus_const(var, offset, false);
        self.add_var(CellExpression::DoubleDeref(cell, full_offset))
    }

    /// Sets the label to have the set states, otherwise tests if the state matches the existing one
    /// by merging.
    fn set_or_test_label_state(&mut self, label: String, state: State) {
        match self.label_state.entry(label) {
            Entry::Occupied(mut e) => {
                let read_only = self.read_labels.contains(e.key());
                e.get_mut().intersect(&state, read_only);
            }
            Entry::Vacant(e) => {
                e.insert(state);
            }
        }
    }

    /// Add a statement to jump to `label`.
    pub fn jump(&mut self, label: String) {
        let instruction = self.next_instruction(
            InstructionBody::Jump(JumpInstruction {
                target: deref_or_immediate!(0),
                relative: true,
            }),
            true,
        );
        self.statements.push(Statement::Jump(label.clone(), instruction));
        let mut state = State::default();
        std::mem::swap(&mut state, &mut self.main_state);
        self.set_or_test_label_state(label, state);
        self.reachable = false;
    }

    /// Add a statement to jump to `label` if `condition != 0`.
    /// `condition` must be a cell reference.
    pub fn jump_nz(&mut self, condition: Var, label: String) {
        let cell = self.as_cell_ref(condition, true);
        let instruction = self.next_instruction(
            InstructionBody::Jnz(JnzInstruction {
                condition: cell,
                jump_offset: deref_or_immediate!(0),
            }),
            true,
        );
        self.statements.push(Statement::Jump(label.clone(), instruction));
        self.set_or_test_label_state(label, self.main_state.clone());
    }

    /// Adds a label here named `name`.
    pub fn label(&mut self, name: String) {
        if self.reachable {
            self.set_or_test_label_state(name.clone(), self.main_state.clone());
        }
        self.read_labels.insert(name.clone());
        self.main_state = self
            .label_state
            .get(&name)
            .unwrap_or_else(|| panic!("No known value for state on reaching {name}."))
            .clone();
        self.statements.push(Statement::Label(name));
        self.reachable = true;
    }

    /// Rescoping the values, while ignoring all vars not stated in `vars` and giving the vars on
    /// the left side the values of the vars on the right side.
    pub fn rescope<const VAR_COUNT: usize>(&mut self, vars: [(Var, Var); VAR_COUNT]) {
        self.main_state.validate_finality();
        let values =
            vars.map(|(new_var, value_var)| (new_var, self.main_state.get_adjusted(value_var)));
        self.main_state.ap_change = 0;
        self.main_state.allocated = 0;
        self.main_state.vars.clear();
        self.main_state.vars.extend(values);
    }

    /// Adds a call command to 'label'. All AP based variables are passed to the called function
    /// state and dropped from the calling function state.
    pub fn call(&mut self, label: String) {
        self.main_state.validate_finality();
        // Vars to be passed to the called function state.
        let mut function_vars: HashMap<Var, CellExpression> = HashMap::default();
        // FP based vars which will remain in the current state.
        let mut main_vars: HashMap<Var, CellExpression> = HashMap::default();
        let ap_change = self.main_state.ap_change;
        let cell_to_var_flags = |cell: &CellRef| {
            if cell.register == Register::AP { (true, false) } else { (false, true) }
        };
        for (var, value) in self.main_state.vars.iter() {
            let (function_var, main_var) = match value {
                CellExpression::DoubleDeref(cell, _) | CellExpression::Deref(cell) => {
                    cell_to_var_flags(cell)
                }
                CellExpression::Immediate(_) => (true, true),
                CellExpression::BinOp { op: _, a, b } => match b {
                    DerefOrImmediate::Deref(cell) => {
                        if a.register == cell.register {
                            cell_to_var_flags(cell)
                        } else {
                            // Mixed FP and AP based, dropped from both states.
                            (false, false)
                        }
                    }
                    DerefOrImmediate::Immediate(_) => (true, true),
                },
            };
            if function_var {
                // Apply ap change (+2 because of the call statement) and change to FP based before
                // the function call.
                let mut value = value.clone().unchecked_apply_known_ap_change(ap_change + 2);
                match &mut value {
                    CellExpression::DoubleDeref(cell, _) | CellExpression::Deref(cell) => {
                        cell.register = Register::FP
                    }
                    CellExpression::Immediate(_) => {}
                    CellExpression::BinOp { a, b, .. } => {
                        a.register = Register::FP;
                        match b {
                            DerefOrImmediate::Deref(cell) => cell.register = Register::FP,
                            DerefOrImmediate::Immediate(_) => {}
                        }
                    }
                }
                function_vars.insert(*var, value);
            }
            if main_var {
                main_vars.insert(*var, value.clone());
            }
        }

        let instruction = self.next_instruction(
            InstructionBody::Call(CallInstruction {
                relative: true,
                target: deref_or_immediate!(0),
            }),
            false,
        );
        self.statements.push(Statement::Jump(label.clone(), instruction));

        self.main_state.vars = main_vars;
        self.main_state.allocated = 0;
        self.main_state.ap_change = 0;
        let function_state = State { vars: function_vars, ..Default::default() };
        self.set_or_test_label_state(label, function_state);
    }

    /// A return statement in the code.
    pub fn ret(&mut self) {
        self.main_state.validate_finality();
        let instruction = self.next_instruction(InstructionBody::Ret(RetInstruction {}), false);
        self.statements.push(Statement::Final(instruction));
        self.reachable = false;
    }

    /// The number of steps at the last added statement.
    pub fn steps(&self) -> usize {
        self.main_state.steps
    }

    /// Resets the steps counter.
    pub fn reset_steps(&mut self) {
        self.main_state.steps = 0;
    }

    /// Create an assert that would always fail.
    pub fn fail(&mut self) {
        let cell = CellRef { offset: -1, register: Register::FP };
        let instruction = self.next_instruction(
            InstructionBody::AssertEq(AssertEqInstruction {
                a: cell,
                b: ResOperand::BinOp(BinOpOperand {
                    op: Operation::Add,
                    a: cell,
                    b: DerefOrImmediate::Immediate(BigInt::one().into()),
                }),
            }),
            false,
        );
        self.statements.push(Statement::Final(instruction));
        self.reachable = false;
    }

    /// Returns `var`s value, with fixed ap if `adjust_ap` is true.
    pub fn get_value(&self, var: Var, adjust_ap: bool) -> CellExpression {
        if adjust_ap { self.main_state.get_adjusted(var) } else { self.main_state.get_value(var) }
    }

    /// Returns `var`s value as a cell reference, with fixed ap if `adjust_ap` is true.
    fn as_cell_ref(&self, var: Var, adjust_ap: bool) -> CellRef {
        extract_matches!(self.get_value(var, adjust_ap), CellExpression::Deref)
    }

    /// Returns `var`s value as a cell reference or immediate, with fixed ap if `adjust_ap` is true.
    fn as_deref_or_imm(&self, var: Var, adjust_ap: bool) -> DerefOrImmediate {
        match self.get_value(var, adjust_ap) {
            CellExpression::Deref(cell) => DerefOrImmediate::Deref(cell),
            CellExpression::Immediate(imm) => DerefOrImmediate::Immediate(imm.into()),
            CellExpression::DoubleDeref(_, _) | CellExpression::BinOp { .. } => {
                panic!("wrong usage.")
            }
        }
    }

    /// Returns `var`s value as a cell reference plus a small const offset, with fixed ap if
    /// `adjust_ap` is true.
    fn as_cell_ref_plus_const(
        &self,
        var: Var,
        additional_offset: i16,
        adjust_ap: bool,
    ) -> (CellRef, i16) {
        match self.get_value(var, adjust_ap) {
            CellExpression::Deref(cell) => (cell, additional_offset),
            CellExpression::BinOp {
                op: CellOperator::Add,
                a,
                b: DerefOrImmediate::Immediate(imm),
            } => (
                a,
                (imm.value + additional_offset).try_into().expect("Offset too large for deref."),
            ),
            _ => panic!("Not a valid ptr."),
        }
    }

    /// Returns an instruction wrapping the instruction body, and updates the state.
    /// If `inc_ap_supported` may add an `ap++` to the instruction.
    fn next_instruction(&mut self, body: InstructionBody, inc_ap_supported: bool) -> Instruction {
        assert!(self.reachable, "Cannot add instructions at unreachable code.");
        let inc_ap =
            inc_ap_supported && self.main_state.allocated as usize > self.main_state.ap_change;
        if inc_ap {
            self.main_state.ap_change += 1;
        }
        self.main_state.steps += 1;
        let mut hints = vec![];
        std::mem::swap(&mut hints, &mut self.current_hints);
        Instruction { body, inc_ap, hints }
    }

    pub fn move_aux_info(&mut self) -> Option<CasmBuilderAuxiliaryInfo> {
        std::mem::replace(&mut self.aux_info, None)
    }
}

impl Default for CasmBuilder {
    fn default() -> Self {
        Self {
            label_state: Default::default(),
            read_labels: Default::default(),
            main_state: Default::default(),
            statements: Default::default(),
            current_hints: Default::default(),
            var_count: Default::default(),
            reachable: true,
            aux_info: Some(Default::default()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct VarBaseDesc {
    pub name: String,
    pub var_id: Var,
    pub var_expr: CellExpression,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ExprDesc {
    pub expr: String,
    pub var_a: VarBaseDesc,
    pub op: String,
    pub var_b: Option<VarBaseDesc>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct VarDesc {
    pub name: String,
    pub var_id: Var,
    pub var_expr: CellExpression,
    pub expr: Option<ExprDesc>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AssertDesc {
    pub lhs: VarBaseDesc,
    pub expr: ExprDesc,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConstDesc {
    pub name: String,
    pub var_id: Var,
    pub expr: String,
    pub value: CellExpression,
}

#[derive(Debug, PartialEq, Eq)]
pub struct JumpDesc {
    pub target: String,
    pub cond_var: Option<(String, Var)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RetExprDesc {
    pub names: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RetBranchDesc {
    pub name: String,
    pub exprs: Vec<RetExprDesc>,
}

impl RetBranchDesc {
    pub fn get_expr_at_pos(&self, pos: usize) -> Option<String> {
        let mut skipped: usize = 0;
        for expr in &self.exprs {
            if skipped + expr.names.len() > pos {
                return Some(expr.names[pos - skipped].clone());
            }
            skipped += expr.names.len();
        }
        None
    }

    pub fn flat_exprs(&self) -> Vec<String> {
        self.exprs.iter().flat_map(|exprs| exprs.names.iter().map(|s| s.clone())).collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StatementDesc {
    TempVar(VarDesc),
    LocalVar(VarDesc),
    Let(AssertDesc),
    Assert(AssertDesc),
    ApPlus(usize),
    Jump(JumpDesc),
    Label(String),
    Fail,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CasmBuilderAuxiliaryInfo {
    pub var_names: HashMap<Var, String>,
    pub consts: Vec<ConstDesc>,
    pub args: Vec<(Var, CellExpression)>,
    pub statements: Vec<StatementDesc>,
    pub return_args: Vec<RetBranchDesc>,
    pub core_libfunc_instr_num: usize,
}

impl CasmBuilderAuxiliaryInfo {
    pub fn not_empty(&self) -> bool {
        0 < self.statements.len()
    }

    pub fn add_arg(&mut self, var_name: &str, var: Var, cell_expr: CellExpression) {
        self.var_names.insert(var, String::from(var_name));
        self.args.push((var, cell_expr));
    }

    pub fn add_return_branch(&mut self, branch_name: &str, vars: &[&[Var]]) {
        self.return_args.push(RetBranchDesc {
            name: String::from(branch_name),
            exprs: vars.iter().map(|v| RetExprDesc {
                names: v.iter().map(
                    |var| self.var_names.get(var).unwrap_or(&String::from("ρ")).clone()).collect(),
            }).collect(),
        });
    }

    pub fn add_tempvar(&mut self, var_name: &str, var: Var, var_expr: CellExpression) {
        self.var_names.insert(var, String::from(var_name));
        self.statements.push(StatementDesc::TempVar(
            VarDesc {
                name: String::from(var_name),
                var_id: var,
                var_expr: var_expr,
                expr: None,
            }
        ));
    }

    pub fn add_localvar(&mut self, var_name: &str, var: Var, var_expr: CellExpression) {
        self.var_names.insert(var, String::from(var_name));
        self.statements.push(StatementDesc::LocalVar(
            VarDesc {
                name: String::from(var_name),
                var_id: var,
                var_expr: var_expr,
                expr: None,
            }
        ));
    }

    pub fn add_const(&mut self, var_name: &str, var: Var, expr: &str, value: CellExpression) {
        self.var_names.insert(var, String::from(var_name));
        self.consts.push(ConstDesc {
            name: String::from(var_name),
            var_id: var,
            expr: String::from(expr),
            value: value,
        });
    }

    pub fn add_let(
        &mut self,
        lhs: VarBaseDesc,
        expr: &str,
        var_a: VarBaseDesc,
        op: &str,
        var_b: Option<VarBaseDesc>,
    ) {
        self.var_names.insert(lhs.var_id, String::from(&lhs.name));
        self.statements.push(StatementDesc::Let(
            AssertDesc {
                lhs: lhs,
                expr: ExprDesc {
                    expr: String::from(expr),
                    var_a: var_a,
                    op: String::from(op),
                    var_b: var_b,
                }
            }
        ));
    }

    pub fn add_ap_plus(&mut self, step_size: usize) {
        self.statements.push(StatementDesc::ApPlus(step_size));
    }

    pub fn make_var_desc(&self, name: &str, id: Var, expr: CellExpression) -> VarBaseDesc {
        VarBaseDesc {
            name: name.into(),
            var_id: id,
            var_expr: expr,
        }
    }

    pub fn add_assert(
        &mut self,
        lhs: VarBaseDesc,
        rhs: &str,
        var_a: VarBaseDesc,
        op: &str,
        var_b: Option<VarBaseDesc>,
    ) {
        match self.statements.last_mut() {
            Some(StatementDesc::TempVar(tv)) =>  {
                if tv.name == lhs.name && tv.expr.is_none() {
                    tv.expr = Some(ExprDesc {
                        expr: String::from(rhs),
                        var_a: var_a,
                        op: String::from(op),
                        var_b: var_b,
                    });
                    return;
                }
            },
            Some(StatementDesc::LocalVar(lv)) => {
                if lv.name == lhs.name && lv.expr.is_none() {
                    lv.expr = Some(ExprDesc {
                        expr: String::from(rhs),
                        var_a: var_a,
                        op: String::from(op),
                        var_b: var_b,
                    });
                    return;
                }
            },
            _ => {},
        }

        self.statements.push(StatementDesc::Assert(
            AssertDesc {
                lhs: lhs,
                expr: ExprDesc {
                    expr: String::from(rhs),
                    var_a: var_a,
                    op: String::from(op),
                    var_b: var_b,
                }
            }));
    }

    pub fn add_jump(&mut self, label: &str) {
        self.statements.push(StatementDesc::Jump(JumpDesc {
            target: String::from(label),
            cond_var: None,
        }));
    }

    pub fn add_jump_nz(&mut self, label: &str, cond_var_name: &str, cond_var: Var) {
        self.statements.push(StatementDesc::Jump(JumpDesc {
            target: String::from(label),
            cond_var: Some((String::from(cond_var_name), cond_var)),
        }));
    }

    pub fn add_label(&mut self, label: &str) {
        self.statements.push(StatementDesc::Label(String::from(label)));
    }

    pub fn add_fail(&mut self) {
        self.statements.push(StatementDesc::Fail);
    }

    pub fn finalize(&mut self, num_casm_instructions: usize) {
        // Record the number of instructions generated for the code built up to
        // here (additional instructions may be added later).
        self.core_libfunc_instr_num = num_casm_instructions;
    }

}

impl Default for CasmBuilderAuxiliaryInfo {
    fn default() -> Self {
        Self {
            var_names: Default::default(),
            consts: Default::default(),
            args: Default::default(),
            statements: Default::default(),
            return_args: Default::default(),
            core_libfunc_instr_num: 0,
        }
    }
}

#[macro_export]
macro_rules! casm_build_extend {
    ($builder:ident,) => {};
    ($builder:ident, tempvar $var:ident; $($tok:tt)*) => {
        let $var = $builder.alloc_var(false);
        {
            let var_expr = $builder.get_value($var, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_tempvar(stringify!($var), $var, var_expr);
            }
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, localvar $var:ident; $($tok:tt)*) => {
        let $var = $builder.alloc_var(true);
        {
            let var_expr = $builder.get_value($var, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_localvar(stringify!($var), $var, var_expr);
            }
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, ap += $value:expr; $($tok:tt)*) => {
        $builder.add_ap($value);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_ap_plus($value);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, const $imm:ident = $value:expr; $($tok:tt)*) => {
        let $imm = $builder.add_var($crate::cell_expression::CellExpression::Immediate(($value).into()));
        let value = $builder.get_value($imm, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_const(stringify!($imm), $imm, stringify!($value), value);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = $res:ident; $($tok:tt)*) => {
        $builder.assert_vars_eq($dst, $res);
        let dst_expr = $builder.get_value($dst, false);
        let res_expr = $builder.get_value($res, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_assert(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                stringify!($res),
                aux_info.make_var_desc(stringify!($res), $res, res_expr),
                "",
                None
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = $a:ident + $b:ident; $($tok:tt)*) => {
        {
            let __sum = $builder.bin_op($crate::cell_expression::CellOperator::Add, $a, $b);
            let dst_expr = $builder.get_value($dst, false);
            let a_expr = $builder.get_value($a, false);
            let b_expr = $builder.get_value($b, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                    stringify!($a + $b),
                    aux_info.make_var_desc(stringify!($a), $a, a_expr),
                    "+",
                    Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
                );
            }
            $builder.assert_vars_eq($dst, __sum);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = $a:ident * $b:ident; $($tok:tt)*) => {
        {
            let __product = $builder.bin_op($crate::cell_expression::CellOperator::Mul, $a, $b);
            let dst_expr = $builder.get_value($dst, false);
            let a_expr = $builder.get_value($a, false);
            let b_expr = $builder.get_value($b, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                    stringify!($a * $b),
                    aux_info.make_var_desc(stringify!($a), $a, a_expr),
                    "*",
                    Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
                );
            }
            $builder.assert_vars_eq($dst, __product);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = $a:ident - $b:ident; $($tok:tt)*) => {
        {
            let __diff = $builder.bin_op($crate::cell_expression::CellOperator::Sub, $a, $b);
            let dst_expr = $builder.get_value($dst, false);
            let a_expr = $builder.get_value($a, false);
            let b_expr = $builder.get_value($b, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                    stringify!($a - $b),
                    aux_info.make_var_desc(stringify!($a), $a, a_expr),
                    "-",
                    Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
                );
            }
            $builder.assert_vars_eq($dst, __diff);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = $a:ident / $b:ident; $($tok:tt)*) => {
        {
            let __division = $builder.bin_op($crate::cell_expression::CellOperator::Div, $a, $b);
            let dst_expr = $builder.get_value($dst, false);
            let a_expr = $builder.get_value($a, false);
            let b_expr = $builder.get_value($b, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                    stringify!($a / $b),
                    aux_info.make_var_desc(stringify!($a), $a, a_expr),
                    "/",
                    Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
                );
            }
            $builder.assert_vars_eq($dst, __division);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = $buffer:ident [ $offset:expr ] ; $($tok:tt)*) => {
        {
            let __deref = $builder.double_deref($buffer, $offset);
            let dst_expr = $builder.get_value($dst, false);
            let buffer_expr = $builder.get_value($buffer, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                    &format!("mem ({} + ({}))", stringify!($buffer), stringify!($offset)),
                    aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                    "*(+)",
                    None,
                );
            }
            $builder.assert_vars_eq($dst, __deref);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $dst:ident = * $buffer:ident; $($tok:tt)*) => {
        {
            let __deref = $builder.double_deref($buffer, 0);
            let dst_expr = $builder.get_value($dst, false);
            let buffer_expr = $builder.get_value($buffer, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                    &format!("mem {}", stringify!($buffer)),
                    aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                    "*()",
                    None,
                );
            }
            $builder.assert_vars_eq($dst, __deref);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, assert $value:ident = * ( $buffer:ident ++ ); $($tok:tt)*) => {
        $builder.buffer_write_and_inc($buffer, $value);
        {
            let value_expr = $builder.get_value($value, false);
            let buffer_expr = $builder.get_value($buffer, false);
            if let Some(aux_info) = &mut $builder.aux_info {
                aux_info.add_assert(
                    aux_info.make_var_desc(stringify!($value), $value, value_expr),
                    &format!("mem {}", stringify!($buffer)),
                    aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                    "*()",
                    None,
                );
                // Currently, we do not create intermediate variables for these steps,
                // but this may change in the future.
                /*aux_info.add_let(
                    aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                    &format!("{} + 1", stringify!($buffer)),
                    aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                    "++",
                    None,
                );*/
            }
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, tempvar $var:ident = $value:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = $value; $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = $lhs:ident + $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = $lhs + $rhs; $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = $lhs:ident * $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = $lhs * $rhs; $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = $lhs:ident - $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = $lhs - $rhs; $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = $lhs:ident / $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = $lhs / $rhs; $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = * ( $buffer:ident ++ ); $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = *($buffer++); $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = $buffer:ident [ $offset:expr ]; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = $buffer[$offset]; $($tok)*);
    };
    ($builder:ident, tempvar $var:ident = * $buffer:ident ; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, tempvar $var; assert $var = *$buffer; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = $value:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = $value; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = $lhs:ident + $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = $lhs + $rhs; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = $lhs:ident * $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = $lhs * $rhs; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = $lhs:ident - $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = $lhs - $rhs; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = $lhs:ident / $rhs:ident; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = $lhs / $rhs; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = * ( $buffer:ident ++ ); $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = *($buffer++); $($tok)*);
    };
    ($builder:ident, localvar $var:ident = $buffer:ident [ $offset:expr ]; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = $buffer[$offset]; $($tok)*);
    };
    ($builder:ident, localvar $var:ident = * $buffer:ident ; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, localvar $var; assert $var = *$buffer; $($tok)*);
    };
    ($builder:ident, let $dst:ident = $a:ident + $b:ident; $($tok:tt)*) => {
        let $dst = $builder.bin_op($crate::cell_expression::CellOperator::Add, $a, $b);
        let dst_expr = $builder.get_value($dst, false);
        let a_expr = $builder.get_value($a, false);
        let b_expr = $builder.get_value($b, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                stringify!($a + $b),
                aux_info.make_var_desc(stringify!($a), $a, a_expr),
                "+",
                Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = $a:ident * $b:ident; $($tok:tt)*) => {
        let $dst = $builder.bin_op($crate::cell_expression::CellOperator::Mul, $a, $b);
        let dst_expr = $builder.get_value($dst, false);
        let a_expr = $builder.get_value($a, false);
        let b_expr = $builder.get_value($b, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                stringify!($a * $b),
                aux_info.make_var_desc(stringify!($a), $a, a_expr),
                "*",
                Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = $a:ident - $b:ident; $($tok:tt)*) => {
        let $dst = $builder.bin_op($crate::cell_expression::CellOperator::Sub, $a, $b);
        let dst_expr = $builder.get_value($dst, false);
        let a_expr = $builder.get_value($a, false);
        let b_expr = $builder.get_value($b, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                stringify!($a - $b),
                aux_info.make_var_desc(stringify!($a), $a, a_expr),
                "-",
                Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = $a:ident / $b:ident; $($tok:tt)*) => {
        let $dst = $builder.bin_op($crate::cell_expression::CellOperator::Div, $a, $b);
        let dst_expr = $builder.get_value($dst, false);
        let a_expr = $builder.get_value($a, false);
        let b_expr = $builder.get_value($b, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                stringify!($a / $b),
                aux_info.make_var_desc(stringify!($a), $a, a_expr),
                "/",
                Some(aux_info.make_var_desc(stringify!($b), $b, b_expr))
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = * ( $buffer:ident ++ ); $($tok:tt)*) => {
        let $dst = $builder.get_ref_and_inc($buffer);
        let dst_expr = $builder.get_value($dst, false);
        let buffer_expr = $builder.get_value($buffer, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                &format!("mem {}", stringify!($buffer)),
                aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                "*()",
                None
            );
            // Currently, we do not create intermediate variables for these steps,
            // but this may change in the future.
            /*aux_info.add_let(
                aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                &format!("{} + 1", stringify!($buffer)),
                aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                "++",
                None,
            );*/
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = $buffer:ident [ $offset:expr ] ; $($tok:tt)*) => {
        let $dst = $builder.double_deref($buffer, $offset);
        let dst_expr = $builder.get_value($dst, false);
        let buffer_expr = $builder.get_value($buffer, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                &format!("mem ({} + ({}))", stringify!($buffer), stringify!($offset)),
                aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                "*(+)",
                None,
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = *$buffer:ident; $($tok:tt)*) => {
        let $dst = $builder.double_deref($buffer, 0);
        let dst_expr = $builder.get_value($dst, false);
        let buffer_expr = $builder.get_value($buffer, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                &format!("mem {}", stringify!($buffer)),
                aux_info.make_var_desc(stringify!($buffer), $buffer, buffer_expr),
                "*()",
                None,
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let $dst:ident = $src:ident; $($tok:tt)*) => {
        let $dst = $builder.duplicate_var($src);
        let dst_expr = $builder.get_value($dst, false);
        let src_expr = $builder.get_value($src, false);
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_let(
                aux_info.make_var_desc(stringify!($dst), $dst, dst_expr),
                stringify!($src),
                aux_info.make_var_desc(stringify!($src), $src, src_expr),
                "",
                None
            );
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, jump $target:ident; $($tok:tt)*) => {
        $builder.jump(std::stringify!($target).to_owned());
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_jump(stringify!($target));
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, jump $target:ident if $condition:ident != 0; $($tok:tt)*) => {
        $builder.jump_nz($condition, std::stringify!($target).to_owned());
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_jump_nz(stringify!($target), stringify!($condition), $condition);
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, let ($($var_name:ident),*) = call $target:ident; $($tok:tt)*) => {
        $builder.call(std::stringify!($target).to_owned());

        let __var_count = {0i16 $(+ (stringify!($var_name), 1i16).1)*};
        let mut __var_index = 0;
        $(
            let $var_name = $builder.add_var($crate::cell_expression::CellExpression::Deref($crate::operand::CellRef {
                offset: __var_index - __var_count,
                register: $crate::operand::Register::AP,
            }));
            __var_index += 1;
        )*
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, ret; $($tok:tt)*) => {
        $builder.ret();
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, $label:ident: $($tok:tt)*) => {
        $builder.label(std::stringify!($label).to_owned());
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_label(stringify!($label));
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, fail; $($tok:tt)*) => {
        $builder.fail();
        if let Some(aux_info) = &mut $builder.aux_info {
            aux_info.add_fail();
        }
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, hint $hint_head:ident$(::$hint_tail:ident)+ {
            $($input_name:ident : $input_value:ident),*
        } into {
            $($output_name:ident : $output_value:ident),*
        }; $($tok:tt)*) => {
        $builder.add_hint(
            |[$($input_name),*], [$($output_name),*]| $hint_head$(::$hint_tail)+ {
                $($input_name,)* $($output_name,)*
            },
            [$($input_value,)*],
            [$($output_value,)*],
        );
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, hint $hint_name:ident {
            $($input_name:ident : $input_value:ident),*
        } into {
            $($output_name:ident : $output_value:ident),*
        }; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, hint $crate::hints::CoreHint::$hint_name {
            $($input_name : $input_value),*
        } into {
            $($output_name : $output_value),*
        }; $($tok)*)
    };
    ($builder:ident, hint $hint_head:ident$(::$hint_tail:ident)* {
        $($arg_name:ident : $arg_value:ident),*
    }; $($tok:tt)*) => {
        $crate::casm_build_extend!($builder, hint $hint_head$(::$hint_tail)* {
            $($arg_name : $arg_value),*
        } into {}; $($tok)*)
    };
    ($builder:ident, rescope { $($new_var:ident = $value_var:ident),* }; $($tok:tt)*) => {
        $builder.rescope([$(($new_var, $value_var)),*]);
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, #{ validate steps == $count:expr; } $($tok:tt)*) => {
        assert_eq!($builder.steps(), $count);
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, #{ steps = 0; } $($tok:tt)*) => {
        $builder.reset_steps();
        $crate::casm_build_extend!($builder, $($tok)*)
    };
    ($builder:ident, #{ $counter:ident += steps; steps = 0; } $($tok:tt)*) => {
        $counter += $builder.steps() as i32;
        $builder.reset_steps();
        $crate::casm_build_extend!($builder, $($tok)*)
    };
}
