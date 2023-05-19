//! Builds the *unlowering map*, which maps each piece of the MIR to the HIR `Expr` that was
//! lowered to produce it.
//!
//! For example:
//!
//! ```Rust
//! fn f(a: i32) -> i32 {
//!     a + 1
//! }
//!
//! fn g(x: i32, y: i32) -> i32 {
//!     x + f(y)
//! }
//! ```
//!
//! For `f`, the unlowering map annotates the MIR as follows:
//!
//! ```text
//! block bb0:
//!   bb0[0]: StorageLive(_2)
//!   bb0[1]: _2 = _1
//!     []: StoreIntoLocal, `a`
//!     [Rvalue]: Expr, `a`
//!   bb0[2]: _0 = Add(move _2, const 1_i32)
//!     []: StoreIntoLocal, `a + 1`
//!     [Rvalue]: Expr, `a + 1`
//!   bb0[3]: StorageDead(_2)
//!   bb0[4]: Terminator { source_info: ..., kind: return }
//! ```
//!
//! The statement `_2 = _1` is associated with the expression `a`; the statement
//! as a whole is storing the result of evaluating `a` into a MIR local, and the
//! statement's rvalue `_1` represents the expression `a` itself.  Similarly, `_0 =
//! Add(move _2, const 1)` stores the result of `a + 1` into a local.  If needed,
//! we could extend the `unlower` pass to also record that `move _2` (a.k.a. `bb0[2]`
//! `[Rvalue, RvalueOperand(0)]`) is lowered from the `Expr` `a`.
//!
//! On `g`, the unlowering map includes the following (among other entries):
//!
//! ```text
//! bb0[5]: Terminator { source_info: ..., kind: _4 = f(move _5) -> [return: bb1, unwind: bb2] }
//!   []: StoreIntoLocal, `f(y)`
//!   [Rvalue]: Expr, `f(y)`
//!   [Rvalue, CallArg(0)]: Expr, `y`
//! bb1[1]: _0 = Add(move _3, move _4)
//!   []: StoreIntoLocal, `x + f(y)`
//!   [Rvalue]: Expr, `x + f(y)`
//! ```
//!
//! The call terminator `_4 = f(move _5)` computes `f(y)` and stores the result
//! into a local; its rvalue is `f(y)` itself, and the first argument of the rvalue
//! is `y`.

use crate::rewrite::build_span_index;
use crate::rewrite::expr::mir_op::SubLoc;
use crate::rewrite::span_index::SpanIndex;
use log::*;
use rustc_hir as hir;
use rustc_hir::intravisit;
use rustc_hir::HirId;
use rustc_middle::hir::nested_filter;
use rustc_middle::mir::{self, Body, Location};
use rustc_middle::ty::{DefIdTree, TyCtxt};
use rustc_span::symbol::sym;
use rustc_span::Span;
use std::collections::btree_map::{BTreeMap, Entry};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct MirOrigin {
    pub hir_id: HirId,
    pub span: Span,
    pub desc: MirOriginDesc,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum MirOriginDesc {
    /// This MIR represents the whole HIR expression.
    Expr,
    /// This MIR stores the result of the HIR expression into a MIR local of some kind.
    StoreIntoLocal,
}

struct UnlowerVisitor<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    mir: &'a Body<'tcx>,
    span_index: SpanIndex<Location>,
    /// Maps MIR (sub)locations to the HIR node that produced each one, if known.
    mir_map: BTreeMap<(Location, Vec<SubLoc>), MirOrigin>,
}

impl<'a, 'tcx> UnlowerVisitor<'a, 'tcx> {
    fn location_span(&self, loc: Location) -> Span {
        self.mir
            .stmt_at(loc)
            .either(|stmt| stmt.source_info.span, |term| term.source_info.span)
    }

    fn record(&mut self, loc: Location, sub_loc: &[SubLoc], ex: &hir::Expr) {
        self.record_desc(loc, sub_loc, ex, MirOriginDesc::Expr);
    }

    fn record_desc(
        &mut self,
        loc: Location,
        sub_loc: &[SubLoc],
        ex: &hir::Expr,
        desc: MirOriginDesc,
    ) {
        let origin = MirOrigin {
            hir_id: ex.hir_id,
            span: ex.span,
            desc,
        };
        match self.mir_map.entry((loc, sub_loc.to_owned())) {
            Entry::Vacant(e) => {
                e.insert(origin);
            }
            Entry::Occupied(e) => {
                if *e.get() != origin {
                    let old_origin = e.get().clone();
                    error!(
                        "conflicting origins for {:?} {:?} ({:?})\n\
                            origin 1 = {:?}\n\
                            origin 2 = {:?}\n\
                            expr 2 = {:?}",
                        loc,
                        sub_loc,
                        self.location_span(loc),
                        origin,
                        old_origin,
                        ex,
                    );
                }
            }
        }
    }

    fn get_sole_assign(
        &self,
        locs: &[Location],
    ) -> Option<(Location, mir::Place<'tcx>, &'a mir::Rvalue<'tcx>)> {
        if locs.len() != 1 {
            return None;
        }
        self.get_last_assign(locs)
    }

    fn get_last_assign(
        &self,
        locs: &[Location],
    ) -> Option<(Location, mir::Place<'tcx>, &'a mir::Rvalue<'tcx>)> {
        let &loc = locs.last()?;
        let stmt = self.mir.stmt_at(loc).left()?;
        match stmt.kind {
            mir::StatementKind::Assign(ref x) => Some((loc, x.0, &x.1)),
            _ => None,
        }
    }

    fn get_last_call(
        &self,
        locs: &[Location],
    ) -> Option<(
        Location,
        mir::Place<'tcx>,
        &'a mir::Operand<'tcx>,
        &'a [mir::Operand<'tcx>],
    )> {
        let &loc = locs.last()?;
        let term = self.mir.stmt_at(loc).right()?;
        match term.kind {
            mir::TerminatorKind::Call {
                ref func,
                ref args,
                destination,
                ..
            } => Some((loc, destination, func, args)),
            _ => None,
        }
    }

    fn should_ignore_statement(&self, loc: Location) -> bool {
        if let Some(stmt) = self.mir.stmt_at(loc).left() {
            match stmt.kind {
                mir::StatementKind::FakeRead(..)
                | mir::StatementKind::StorageLive(..)
                | mir::StatementKind::StorageDead(..)
                | mir::StatementKind::Nop => return true,
                _ => {}
            }
        }
        false
    }

    fn is_var(&self, pl: mir::Place<'tcx>) -> bool {
        pl.projection.len() == 0
    }

    fn visit_expr_inner(&mut self, ex: &'tcx hir::Expr<'tcx>) {
        let mut locs = Vec::with_capacity(1);
        for &loc in self.span_index.lookup_exact(ex.span) {
            if !self.should_ignore_statement(loc) {
                locs.push(loc);
            }
        }
        if locs.len() == 0 {
            return;
        }

        let warn = |slf: &Self, desc| {
            warn!("{}", desc);
            info!("locs:");
            for &loc in &locs {
                slf.mir.stmt_at(loc).either(
                    |stmt| info!("  {:?}: {:?}", locs, stmt),
                    |term| info!("  {:?}: {:?}", locs, term),
                );
            }
            info!("span = {:?}", ex.span);
            info!("expr = {:?}", ex);
        };

        // Most exprs end with an assignment, storing the result into a temporary.
        match ex.kind {
            hir::ExprKind::Assign(pl, rv, _span) => {
                // For `Assign`, we expect the assignment to be the whole thing.
                let (loc, _mir_pl, _mir_rv) = match self.get_sole_assign(&locs) {
                    Some(x) => x,
                    None => {
                        warn(self, "expected exactly one StatementKind::Assign");
                        return;
                    }
                };
                self.record(loc, &[], ex);
                self.record(loc, &[SubLoc::Dest], pl);
                self.record(loc, &[SubLoc::Rvalue], rv);
            }

            hir::ExprKind::Call(_, args) | hir::ExprKind::MethodCall(_, args, _) => {
                let (loc, _mir_pl, _mir_func, mir_args) = match self.get_last_call(&locs) {
                    Some((l, pl, f, a)) if self.is_var(pl) => (l, pl, f, a),
                    _ => {
                        warn(self, "expected final Call to store into var");
                        return;
                    }
                };
                self.record_desc(loc, &[], ex, MirOriginDesc::StoreIntoLocal);
                self.record(loc, &[SubLoc::Rvalue], ex);
                for (i, (arg, mir_arg)) in args.iter().zip(mir_args).enumerate() {
                    self.record(loc, &[SubLoc::Rvalue, SubLoc::CallArg(i)], arg);
                    // TODO: distribute extra `locs` among the various args
                    self.visit_expr_operand(arg, mir_arg, &[]);
                }
                if locs.len() > 1 {
                    warn!("NYI: extra locations {:?} in Call", &locs[..locs.len() - 1]);
                }
            }

            _ => {
                // For all other `ExprKind`s, we expect the last `loc` to be an assignment storing
                // the final result into a temporary.
                let (loc, _mir_pl, mir_rv) = match self.get_last_assign(&locs) {
                    Some((l, pl, r)) if self.is_var(pl) => (l, pl, r),
                    _ => {
                        warn(self, "expected final Assign to store into var");
                        return;
                    }
                };
                self.record_desc(loc, &[], ex, MirOriginDesc::StoreIntoLocal);
                self.record(loc, &[SubLoc::Rvalue], ex);
                self.visit_expr_rvalue(ex, mir_rv, &locs[..locs.len() - 1]);
            }
        }
    }

    fn visit_expr_rvalue(
        &mut self,
        _ex: &'tcx hir::Expr<'tcx>,
        _rv: &'a mir::Rvalue<'tcx>,
        _extra_locs: &[Location],
    ) {
        // TODO: handle adjustments, overloaded operators, etc
    }

    fn visit_expr_operand(
        &mut self,
        _ex: &'tcx hir::Expr<'tcx>,
        _rv: &'a mir::Operand<'tcx>,
        _extra_locs: &[Location],
    ) {
        // TODO: handle adjustments, overloaded operators, etc
    }
}

impl<'a, 'tcx> intravisit::Visitor<'tcx> for UnlowerVisitor<'a, 'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }

    fn visit_expr(&mut self, ex: &'tcx hir::Expr<'tcx>) {
        self.visit_expr_inner(ex);
        intravisit::walk_expr(self, ex);
    }
}

pub fn unlower<'tcx>(
    tcx: TyCtxt<'tcx>,
    mir: &Body<'tcx>,
    hir_body_id: hir::BodyId,
) -> BTreeMap<(Location, Vec<SubLoc>), MirOrigin> {
    // If this MIR body came from a `#[derive]`, ignore it.
    let def_id = mir.source.def_id();
    if let Some(parent_def_id) = tcx.opt_parent(def_id) {
        if tcx.has_attr(parent_def_id, sym::automatically_derived) {
            return BTreeMap::new();
        }
    }

    // Build `span_index`, which maps `Span`s to MIR `Locations`.
    let span_index = build_span_index(mir);

    // Run the visitor.
    let hir = tcx.hir().body(hir_body_id);

    let mut v = UnlowerVisitor {
        tcx,
        mir,
        span_index,
        mir_map: BTreeMap::new(),
    };
    intravisit::Visitor::visit_body(&mut v, hir);

    // Print results.
    eprintln!("unlowering for {:?}:", mir.source);
    for (bb_id, bb) in mir.basic_blocks().iter_enumerated() {
        eprintln!("  block {:?}:", bb_id);
        for (i, stmt) in bb.statements.iter().enumerate() {
            let loc = Location {
                block: bb_id,
                statement_index: i,
            };

            eprintln!("    {:?}: {:?}", loc, stmt);
            for (k, v) in v.mir_map.range(&(loc, vec![])..) {
                if k.0 != loc {
                    break;
                }
                let ex = tcx.hir().expect_expr(v.hir_id);
                eprintln!("      {:?}: {:?}, {:?}", k.1, v.desc, ex.span);
            }
        }

        {
            let term = bb.terminator();
            let loc = Location {
                block: bb_id,
                statement_index: bb.statements.len(),
            };

            eprintln!("    {:?}: {:?}", loc, term);
            for (k, v) in v.mir_map.range(&(loc, vec![])..) {
                if k.0 != loc {
                    break;
                }
                let ex = tcx.hir().expect_expr(v.hir_id);
                eprintln!("      {:?}: {:?}, {:?}", k.1, v.desc, ex.span);
            }
        }
    }

    v.mir_map
}
