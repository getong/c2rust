//! Rewriting of expressions comes with one extra bit of complexity: sometimes the code we're
//! modifying has had autoderef and/or autoref `Adjustment`s applied to it. To avoid unexpectedly
//! changing which adjustments get applied, we "materialize" the `Adjustment`s, making them
//! explicit in the source code. For example, `vec.len()`, which implicitly applies deref and ref
//! adjustments to `vec`, would be converted to `(&*vec).len()`, where the deref and ref operations
//! are explicit, and might be further rewritten from there. However, we don't want to materialize
//! all adjustments, as this would make even non-rewritten code extremely verbose, so we try to
//! materialize adjustments only on code that's subject to some rewrite.

use crate::context::{AnalysisCtxt, Assignment, FlagSet, LTy, PermissionSet};
use crate::pointer_id::PointerTable;
use crate::type_desc::{self, Ownership, Quantity, TypeDesc};
use crate::util::{ty_callee, Callee};
use rustc_ast::Mutability;
use rustc_middle::mir::{
    BasicBlock, Body, Location, Operand, Place, Rvalue, Statement, StatementKind, Terminator,
    TerminatorKind,
};
use rustc_middle::ty::TyKind;
use std::collections::HashMap;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SubLoc {
    /// The LHS of an assignment or call.  `StatementKind::Assign/TerminatorKind::Call -> Place`
    Dest,
    /// The RHS of an assignment.  `StatementKind::Assign -> Rvalue`
    AssignRvalue,
    /// The Nth argument of a call.  `TerminatorKind::Call -> Operand`
    CallArg(usize),
    /// The Nth operand of an rvalue.  `Rvalue -> Operand`
    RvalueOperand(usize),
    /// The Nth place of an rvalue.  Used for cases like `Rvalue::Ref` that directly refer to a
    /// `Place`.  `Rvalue -> Place`
    RvaluePlace(usize),
    /// The place referenced by an operand.  `Operand::Move/Operand::Copy -> Place`
    OperandPlace,
    /// The pointer used in the Nth innermost deref within a place.  `Place -> Place`
    PlacePointer(usize),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RewriteKind {
    /// Replace `ptr.offset(i)` with something like `&ptr[i..]`.
    OffsetSlice { mutbl: bool },
    /// Replace `slice` with `&slice[0]`.
    SliceFirst { mutbl: bool },
    /// Replace `ptr` with `&*ptr`, converting `&mut T` to `&T`.
    MutToImm,
    /// Remove a call to `as_ptr` or `as_mut_ptr`.
    RemoveAsPtr,
    /// Replace &raw with & or &raw mut with &mut
    RawToRef { mutbl: bool },
    /// Replace `y` in `let x = y` with `Cell::new(y)`, i.e. `let x = Cell::new(y)`
    /// TODO: ensure `y` implements `Copy`
    CellNew,
    /// Replace `*y` with `Cell::get(y)` where `y` is a pointer
    CellGet,
    /// Replace `*y = x` with `Cell::set(x)` where `y` is a pointer
    CellSet,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MirRewrite {
    pub kind: RewriteKind,
    pub sub_loc: Vec<SubLoc>,
}

struct ExprRewriteVisitor<'a, 'tcx> {
    acx: &'a AnalysisCtxt<'a, 'tcx>,
    perms: PointerTable<'a, PermissionSet>,
    flags: PointerTable<'a, FlagSet>,
    rewrites: &'a mut HashMap<Location, Vec<MirRewrite>>,
    mir: &'a Body<'tcx>,
    loc: Location,
    sub_loc: Vec<SubLoc>,
}

impl<'a, 'tcx> ExprRewriteVisitor<'a, 'tcx> {
    pub fn new(
        acx: &'a AnalysisCtxt<'a, 'tcx>,
        asn: &'a Assignment,
        rewrites: &'a mut HashMap<Location, Vec<MirRewrite>>,
        mir: &'a Body<'tcx>,
    ) -> ExprRewriteVisitor<'a, 'tcx> {
        let perms = asn.perms();
        let flags = asn.flags();
        ExprRewriteVisitor {
            acx,
            perms,
            flags,
            rewrites,
            mir,
            loc: Location {
                block: BasicBlock::from_usize(0),
                statement_index: 0,
            },
            sub_loc: Vec::new(),
        }
    }

    fn enter<F: FnOnce(&mut Self) -> R, R>(&mut self, sub: SubLoc, f: F) -> R {
        self.sub_loc.push(sub);
        let r = f(self);
        self.sub_loc.pop();
        r
    }

    fn enter_dest<F: FnOnce(&mut Self) -> R, R>(&mut self, f: F) -> R {
        self.enter(SubLoc::Dest, f)
    }

    fn enter_assign_rvalue<F: FnOnce(&mut Self) -> R, R>(&mut self, f: F) -> R {
        self.enter(SubLoc::AssignRvalue, f)
    }

    fn enter_call_arg<F: FnOnce(&mut Self) -> R, R>(&mut self, i: usize, f: F) -> R {
        self.enter(SubLoc::CallArg(i), f)
    }

    fn enter_rvalue_operand<F: FnOnce(&mut Self) -> R, R>(&mut self, i: usize, f: F) -> R {
        self.enter(SubLoc::RvalueOperand(i), f)
    }

    fn enter_rvalue_place<F: FnOnce(&mut Self) -> R, R>(&mut self, i: usize, f: F) -> R {
        self.enter(SubLoc::RvaluePlace(i), f)
    }

    #[allow(dead_code)]
    fn _enter_operand_place<F: FnOnce(&mut Self) -> R, R>(&mut self, f: F) -> R {
        self.enter(SubLoc::OperandPlace, f)
    }

    #[allow(dead_code)]
    fn _enter_place_pointer<F: FnOnce(&mut Self) -> R, R>(&mut self, i: usize, f: F) -> R {
        self.enter(SubLoc::PlacePointer(i), f)
    }

    fn visit_statement(&mut self, stmt: &Statement<'tcx>, loc: Location) {
        self.loc = loc;
        debug_assert!(self.sub_loc.is_empty());

        match stmt.kind {
            StatementKind::Assign(ref x) => {
                let (pl, ref rv) = **x;

                if matches!(rv, Rvalue::Cast(..)) && self.acx.c_void_casts.should_skip_stmt(loc) {
                    // This is a cast to or from `void*` associated with a `malloc`, `free`, or
                    // other libc call.
                    //
                    // TODO: we should probably emit a rewrite here to remove the cast; then when
                    // we implement rewriting of the actual call, it won't need to deal with
                    // additional rewrites at a separate location from the call itself.
                    return;
                }

                let pl_lty = self.acx.type_of(pl);

                // FIXME: Needs changes to handle CELL pointers in struct fields.  Suppose `pl` is
                // something like `*(_1.0)`, where the `.0` field is CELL.  This should be
                // converted to a `Cell::get` call, but we would fail to enter this case because
                // `_1` fails the `is_any_ptr()` check.
                if pl.is_indirect() && self.acx.local_tys[pl.local].ty.is_any_ptr() {
                    let local_lty = self.acx.local_tys[pl.local];
                    let local_ptr = local_lty.label;
                    let perms = self.perms[local_ptr];
                    let flags = self.flags[local_ptr];
                    let desc = type_desc::perms_to_desc(local_lty.ty, perms, flags);
                    if desc.own == Ownership::Cell {
                        // this is an assignment like `*x = 2` but `x` has CELL permissions
                        self.enter_assign_rvalue(|v| v.emit(RewriteKind::CellSet))
                    }
                }

                #[allow(clippy::single_match)]
                match rv {
                    Rvalue::Use(rv_op) => {
                        let local_ty = self.acx.local_tys[pl.local].ty;
                        let local_addr = self.acx.addr_of_local[pl.local];
                        let perms = self.perms[local_addr];
                        let flags = self.flags[local_addr];
                        let desc = type_desc::local_perms_to_desc(local_ty, perms, flags);
                        if desc.own == Ownership::Cell {
                            // this is an assignment like `let x = 2` but `x` has CELL permissions
                            self.enter_assign_rvalue(|v| {
                                v.enter_rvalue_operand(0, |v| v.emit(RewriteKind::CellNew))
                            })
                        }

                        if let Some(rv_place) = rv_op.place() {
                            if rv_place.is_indirect()
                                && self.acx.local_tys[rv_place.local].ty.is_any_ptr()
                            {
                                let local_lty = self.acx.local_tys[rv_place.local];
                                let local_ptr = local_lty.label;
                                let flags = self.flags[local_ptr];
                                if flags.contains(FlagSet::CELL) {
                                    // this is an assignment like `let x = *y` but `y` has CELL permissions
                                    self.enter_assign_rvalue(|v| {
                                        v.enter_rvalue_operand(0, |v| v.emit(RewriteKind::CellGet))
                                    })
                                }
                            }
                        }
                    }
                    _ => {}
                };

                let rv_lty = self.acx.type_of_rvalue(rv, loc);
                self.enter_assign_rvalue(|v| v.visit_rvalue(rv, Some(rv_lty)));
                self.emit_cast_lty_lty(rv_lty, pl_lty);
                self.enter_dest(|v| v.visit_place(pl));
            }
            StatementKind::FakeRead(..) => {}
            StatementKind::SetDiscriminant { .. } => todo!("statement {:?}", stmt),
            StatementKind::Deinit(..) => {}
            StatementKind::StorageLive(..) => {}
            StatementKind::StorageDead(..) => {}
            StatementKind::Retag(..) => {}
            StatementKind::AscribeUserType(..) => {}
            StatementKind::Coverage(..) => {}
            StatementKind::CopyNonOverlapping(..) => todo!("statement {:?}", stmt),
            StatementKind::Nop => {}
        }
    }

    fn visit_terminator(&mut self, term: &Terminator<'tcx>, loc: Location) {
        let tcx = self.acx.tcx();
        self.loc = loc;
        debug_assert!(self.sub_loc.is_empty());

        match term.kind {
            TerminatorKind::Goto { .. } => {}
            TerminatorKind::SwitchInt { .. } => {}
            TerminatorKind::Resume => {}
            TerminatorKind::Abort => {}
            TerminatorKind::Return => {}
            TerminatorKind::Unreachable => {}
            TerminatorKind::Drop { .. } => {}
            TerminatorKind::DropAndReplace { .. } => {}
            TerminatorKind::Call {
                ref func,
                ref args,
                destination,
                target: _,
                ..
            } => {
                let func_ty = func.ty(self.mir, tcx);
                let pl_ty = self.acx.type_of(destination);

                // Special cases for particular functions.
                match ty_callee(tcx, func_ty) {
                    Callee::PtrOffset { .. } => {
                        self.visit_ptr_offset(&args[0], pl_ty);
                        return;
                    }
                    Callee::SliceAsPtr { .. } => {
                        self.visit_slice_as_ptr(&args[0], pl_ty);
                        return;
                    }
                    _ => {}
                }

                // General case: cast `args` to match the signature of `func`.
                let poly_sig = func_ty.fn_sig(tcx);
                let sig = tcx.erase_late_bound_regions(poly_sig);

                for (i, _op) in args.iter().enumerate() {
                    if i >= sig.inputs().len() {
                        // This is a call to a variadic function, and we've gone past the end of
                        // the declared arguments.
                        // TODO: insert a cast to turn `op` back into its original declared type
                        // (i.e. upcast the chosen reference type back to a raw pointer)
                        continue;
                    }

                    // TODO: get the `LTy` to use for the callee's argument
                    // let expect_ty = ...;
                    // self.enter_call_arg(i, |v| v.visit_operand(op, expect_ty));
                }
            }
            TerminatorKind::Assert { .. } => {}
            TerminatorKind::Yield { .. } => {}
            TerminatorKind::GeneratorDrop => {}
            TerminatorKind::FalseEdge { .. } => {}
            TerminatorKind::FalseUnwind { .. } => {}
            TerminatorKind::InlineAsm { .. } => todo!("terminator {:?}", term),
        }
    }

    /// Visit an `Rvalue`.  If `expect_ty` is `Some`, also emit whatever casts are necessary to
    /// make the `Rvalue` produce a value of type `expect_ty`.
    fn visit_rvalue(&mut self, rv: &Rvalue<'tcx>, expect_ty: Option<LTy<'tcx>>) {
        match *rv {
            Rvalue::Use(ref op) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(op, expect_ty));
            }
            Rvalue::Repeat(ref op, _) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(op, None));
            }
            Rvalue::Ref(_rg, _kind, pl) => {
                self.enter_rvalue_place(0, |v| v.visit_place(pl));
            }
            Rvalue::ThreadLocalRef(_def_id) => {
                // TODO
            }
            Rvalue::AddressOf(mutbl, pl) => {
                self.enter_rvalue_place(0, |v| v.visit_place(pl));
                if let Some(expect_ty) = expect_ty {
                    let desc = type_desc::perms_to_desc(
                        expect_ty.ty,
                        self.perms[expect_ty.label],
                        self.flags[expect_ty.label],
                    );
                    self.enter_rvalue_operand(0, |v| match desc.own {
                        Ownership::Cell => v.emit(RewriteKind::RawToRef { mutbl: false }),
                        Ownership::Imm | Ownership::Mut => v.emit(RewriteKind::RawToRef {
                            mutbl: mutbl == Mutability::Mut,
                        }),
                        _ => (),
                    });
                }
            }
            Rvalue::Len(pl) => {
                self.enter_rvalue_place(0, |v| v.visit_place(pl));
            }
            Rvalue::Cast(_kind, ref op, _ty) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(op, None));
            }
            Rvalue::BinaryOp(_bop, ref ops) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(&ops.0, None));
                self.enter_rvalue_operand(1, |v| v.visit_operand(&ops.1, None));
            }
            Rvalue::CheckedBinaryOp(_bop, ref ops) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(&ops.0, None));
                self.enter_rvalue_operand(1, |v| v.visit_operand(&ops.1, None));
            }
            Rvalue::NullaryOp(..) => {}
            Rvalue::UnaryOp(_uop, ref op) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(op, None));
            }
            Rvalue::Discriminant(pl) => {
                self.enter_rvalue_place(0, |v| v.visit_place(pl));
            }
            Rvalue::Aggregate(ref _kind, ref ops) => {
                for (i, op) in ops.iter().enumerate() {
                    self.enter_rvalue_operand(i, |v| v.visit_operand(op, None));
                }
            }
            Rvalue::ShallowInitBox(ref op, _ty) => {
                self.enter_rvalue_operand(0, |v| v.visit_operand(op, None));
            }
            Rvalue::CopyForDeref(pl) => {
                self.enter_rvalue_place(0, |v| v.visit_place(pl));
            }
        }
    }

    /// Visit an `Operand`.  If `expect_ty` is `Some`, also emit whatever casts are necessary to
    /// make the `Operand` produce a value of type `expect_ty`.
    fn visit_operand(&mut self, op: &Operand<'tcx>, expect_ty: Option<LTy<'tcx>>) {
        match *op {
            Operand::Copy(pl) | Operand::Move(pl) => {
                self.visit_place(pl);

                if let Some(expect_ty) = expect_ty {
                    let ptr_lty = self.acx.type_of(pl);
                    if !ptr_lty.label.is_none() {
                        self.emit_cast_lty_lty(ptr_lty, expect_ty);
                    }
                }
            }
            Operand::Constant(..) => {}
        }
    }

    /// Like [`Self::visit_operand`], but takes an expected `TypeDesc` instead of an expected `LTy`.
    fn visit_operand_desc(&mut self, op: &Operand<'tcx>, expect_desc: TypeDesc<'tcx>) {
        match *op {
            Operand::Copy(pl) | Operand::Move(pl) => {
                self.visit_place(pl);

                let ptr_lty = self.acx.type_of(pl);
                if !ptr_lty.label.is_none() {
                    self.emit_cast_lty_desc(ptr_lty, expect_desc);
                }
            }
            Operand::Constant(..) => {}
        }
    }

    fn visit_place(&mut self, _pl: Place<'tcx>) {
        // TODO: walk over `pl` to handle all derefs (casts, `*x` -> `(*x).get()`)
    }

    fn visit_ptr_offset(&mut self, op: &Operand<'tcx>, result_ty: LTy<'tcx>) {
        // Compute the expected type for the argument, and emit a cast if needed.
        let result_ptr = result_ty.label;
        let result_desc =
            type_desc::perms_to_desc(result_ty.ty, self.perms[result_ptr], self.flags[result_ptr]);

        let arg_expect_desc = TypeDesc {
            own: result_desc.own,
            qty: match result_desc.qty {
                Quantity::Single => Quantity::Slice,
                Quantity::Slice => Quantity::Slice,
                Quantity::OffsetPtr => Quantity::OffsetPtr,
                Quantity::Array => unreachable!("perms_to_desc should not return Quantity::Array"),
            },
            pointee_ty: result_desc.pointee_ty,
        };

        self.enter_call_arg(0, |v| v.visit_operand_desc(op, arg_expect_desc));

        // Emit `OffsetSlice` for the offset itself.
        let mutbl = matches!(result_desc.own, Ownership::Mut);

        self.emit(RewriteKind::OffsetSlice { mutbl });

        // If the result is `Single`, also insert an upcast.
        if result_desc.qty == Quantity::Single {
            self.emit(RewriteKind::SliceFirst { mutbl });
        }
    }

    fn visit_slice_as_ptr(&mut self, op: &Operand<'tcx>, result_lty: LTy<'tcx>) {
        let op_lty = self.acx.type_of(op);
        let op_ptr = op_lty.label;
        let result_ptr = result_lty.label;

        let op_desc = type_desc::perms_to_desc(op_lty.ty, self.perms[op_ptr], self.flags[op_ptr]);
        let result_desc = type_desc::perms_to_desc(
            result_lty.ty,
            self.perms[result_ptr],
            self.flags[result_ptr],
        );

        if op_desc.own == result_desc.own && op_desc.qty == result_desc.qty {
            // Input and output types will be the same after rewriting, so the `as_ptr` call is not
            // needed.
            self.emit(RewriteKind::RemoveAsPtr);
        }
    }

    fn emit(&mut self, rw: RewriteKind) {
        self.rewrites
            .entry(self.loc)
            .or_insert_with(Vec::new)
            .push(MirRewrite {
                kind: rw,
                sub_loc: self.sub_loc.clone(),
            });
    }

    fn emit_cast_desc_desc(&mut self, from: TypeDesc<'tcx>, to: TypeDesc<'tcx>) {
        assert_eq!(
            self.acx.tcx().erase_regions(from.pointee_ty),
            self.acx.tcx().erase_regions(to.pointee_ty),
        );

        if from == to {
            return;
        }

        if from.qty == to.qty && (from.own, to.own) == (Ownership::Mut, Ownership::Imm) {
            self.emit(RewriteKind::MutToImm);
            return;
        }

        // TODO: handle Slice -> Single here instead of special-casing in `offset`

        eprintln!("unsupported cast kind: {:?} -> {:?}", from, to);
    }

    fn emit_cast_lty_desc(&mut self, from_lty: LTy<'tcx>, to: TypeDesc<'tcx>) {
        let from = type_desc::perms_to_desc(
            from_lty.ty,
            self.perms[from_lty.label],
            self.flags[from_lty.label],
        );
        self.emit_cast_desc_desc(from, to);
    }

    #[allow(dead_code)]
    fn emit_cast_desc_lty(&mut self, from: TypeDesc<'tcx>, to_lty: LTy<'tcx>) {
        let to = type_desc::perms_to_desc(
            to_lty.ty,
            self.perms[to_lty.label],
            self.flags[to_lty.label],
        );
        self.emit_cast_desc_desc(from, to);
    }

    fn emit_cast_lty_lty(&mut self, from_lty: LTy<'tcx>, to_lty: LTy<'tcx>) {
        if from_lty.label.is_none() && to_lty.label.is_none() {
            return;
        }

        let from_raw = matches!(from_lty.ty.kind(), TyKind::RawPtr(..));
        let to_raw = matches!(to_lty.ty.kind(), TyKind::RawPtr(..));
        if !from_raw && !to_raw {
            // TODO: hack to work around issues with already-safe code
            return;
        }

        let lty_to_desc = |slf: &mut Self, lty: LTy<'tcx>| {
            type_desc::perms_to_desc(lty.ty, slf.perms[lty.label], slf.flags[lty.label])
        };

        let from = lty_to_desc(self, from_lty);
        let to = lty_to_desc(self, to_lty);
        self.emit_cast_desc_desc(from, to);
    }
}

pub fn gen_mir_rewrites<'tcx>(
    acx: &AnalysisCtxt<'_, 'tcx>,
    asn: &Assignment,
    mir: &Body<'tcx>,
) -> HashMap<Location, Vec<MirRewrite>> {
    let mut out = HashMap::new();

    let mut v = ExprRewriteVisitor::new(acx, asn, &mut out, mir);

    for (bb_id, bb) in mir.basic_blocks().iter_enumerated() {
        for (i, stmt) in bb.statements.iter().enumerate() {
            let loc = Location {
                block: bb_id,
                statement_index: i,
            };
            v.visit_statement(stmt, loc);
        }

        if let Some(ref term) = bb.terminator {
            let loc = Location {
                block: bb_id,
                statement_index: bb.statements.len(),
            };
            v.visit_terminator(term, loc);
        }
    }

    out
}
