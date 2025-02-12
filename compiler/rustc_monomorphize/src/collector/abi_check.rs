//! This module ensures that if a function's ABI requires a particular target feature,
//! that target feature is enabled both on the callee and all callers.
use rustc_hir::CRATE_HIR_ID;
use rustc_middle::mir::visit::Visitor as MirVisitor;
use rustc_middle::mir::{self, Location, traversal};
use rustc_middle::query::Providers;
use rustc_middle::ty::inherent::*;
use rustc_middle::ty::{self, Instance, InstanceKind, ParamEnv, Ty, TyCtxt};
use rustc_session::lint::builtin::ABI_UNSUPPORTED_VECTOR_TYPES;
use rustc_span::def_id::DefId;
use rustc_span::{DUMMY_SP, Span, Symbol};
use rustc_target::abi::call::{FnAbi, PassMode};
use rustc_target::abi::{BackendRepr, RegKind};

use crate::errors::{AbiErrorDisabledVectorTypeCall, AbiErrorDisabledVectorTypeDef};

fn uses_vector_registers(mode: &PassMode, repr: &BackendRepr) -> bool {
    match mode {
        PassMode::Ignore | PassMode::Indirect { .. } => false,
        PassMode::Cast { pad_i32: _, cast } => {
            cast.prefix.iter().any(|r| r.is_some_and(|x| x.kind == RegKind::Vector))
                || cast.rest.unit.kind == RegKind::Vector
        }
        PassMode::Direct(..) | PassMode::Pair(..) => matches!(repr, BackendRepr::Vector { .. }),
    }
}

fn do_check_abi<'tcx>(
    tcx: TyCtxt<'tcx>,
    abi: &FnAbi<'tcx, Ty<'tcx>>,
    target_feature_def: DefId,
    mut emit_err: impl FnMut(&'static str),
) {
    let Some(feature_def) = tcx.sess.target.features_for_correct_vector_abi() else {
        return;
    };
    let codegen_attrs = tcx.codegen_fn_attrs(target_feature_def);
    for arg_abi in abi.args.iter().chain(std::iter::once(&abi.ret)) {
        let size = arg_abi.layout.size;
        if uses_vector_registers(&arg_abi.mode, &arg_abi.layout.backend_repr) {
            // Find the first feature that provides at least this vector size.
            let feature = match feature_def.iter().find(|(bits, _)| size.bits() <= *bits) {
                Some((_, feature)) => feature,
                None => {
                    emit_err("<no available feature for this size>");
                    continue;
                }
            };
            let feature_sym = Symbol::intern(feature);
            if !tcx.sess.unstable_target_features.contains(&feature_sym)
                && !codegen_attrs.target_features.iter().any(|x| x.name == feature_sym)
            {
                emit_err(feature);
            }
        }
    }
}

/// Checks that the ABI of a given instance of a function does not contain vector-passed arguments
/// or return values for which the corresponding target feature is not enabled.
fn check_instance_abi<'tcx>(tcx: TyCtxt<'tcx>, instance: Instance<'tcx>) {
    let param_env = ParamEnv::reveal_all();
    let Ok(abi) = tcx.fn_abi_of_instance(param_env.and((instance, ty::List::empty()))) else {
        // An error will be reported during codegen if we cannot determine the ABI of this
        // function.
        return;
    };
    do_check_abi(tcx, abi, instance.def_id(), |required_feature| {
        let span = tcx.def_span(instance.def_id());
        tcx.emit_node_span_lint(
            ABI_UNSUPPORTED_VECTOR_TYPES,
            CRATE_HIR_ID,
            span,
            AbiErrorDisabledVectorTypeDef { span, required_feature },
        );
    })
}

/// Checks that a call expression does not try to pass a vector-passed argument which requires a
/// target feature that the caller does not have, as doing so causes UB because of ABI mismatch.
fn check_call_site_abi<'tcx>(
    tcx: TyCtxt<'tcx>,
    callee: Ty<'tcx>,
    span: Span,
    caller: InstanceKind<'tcx>,
) {
    if callee.fn_sig(tcx).abi().is_rust() {
        // "Rust" ABI never passes arguments in vector registers.
        return;
    }
    let param_env = ParamEnv::reveal_all();
    let callee_abi = match *callee.kind() {
        ty::FnPtr(..) => {
            tcx.fn_abi_of_fn_ptr(param_env.and((callee.fn_sig(tcx), ty::List::empty())))
        }
        ty::FnDef(def_id, args) => {
            // Intrinsics are handled separately by the compiler.
            if tcx.intrinsic(def_id).is_some() {
                return;
            }
            let instance = ty::Instance::expect_resolve(tcx, param_env, def_id, args, DUMMY_SP);
            tcx.fn_abi_of_instance(param_env.and((instance, ty::List::empty())))
        }
        _ => {
            panic!("Invalid function call");
        }
    };

    let Ok(callee_abi) = callee_abi else {
        // ABI failed to compute; this will not get through codegen.
        return;
    };
    do_check_abi(tcx, callee_abi, caller.def_id(), |required_feature| {
        tcx.emit_node_span_lint(
            ABI_UNSUPPORTED_VECTOR_TYPES,
            CRATE_HIR_ID,
            span,
            AbiErrorDisabledVectorTypeCall { span, required_feature },
        );
    });
}

struct MirCallesAbiCheck<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a mir::Body<'tcx>,
    instance: Instance<'tcx>,
}

impl<'a, 'tcx> MirVisitor<'tcx> for MirCallesAbiCheck<'a, 'tcx> {
    fn visit_terminator(&mut self, terminator: &mir::Terminator<'tcx>, _: Location) {
        match terminator.kind {
            mir::TerminatorKind::Call { ref func, ref fn_span, .. }
            | mir::TerminatorKind::TailCall { ref func, ref fn_span, .. } => {
                let callee_ty = func.ty(self.body, self.tcx);
                let callee_ty = self.instance.instantiate_mir_and_normalize_erasing_regions(
                    self.tcx,
                    ty::ParamEnv::reveal_all(),
                    ty::EarlyBinder::bind(callee_ty),
                );
                check_call_site_abi(self.tcx, callee_ty, *fn_span, self.body.source.instance);
            }
            _ => {}
        }
    }
}

fn check_callees_abi<'tcx>(tcx: TyCtxt<'tcx>, instance: Instance<'tcx>) {
    let body = tcx.instance_mir(instance.def);
    let mut visitor = MirCallesAbiCheck { tcx, body, instance };
    for (bb, data) in traversal::mono_reachable(body, tcx, instance) {
        visitor.visit_basic_block_data(bb, data)
    }
}

fn check_feature_dependent_abi<'tcx>(tcx: TyCtxt<'tcx>, instance: Instance<'tcx>) {
    check_instance_abi(tcx, instance);
    check_callees_abi(tcx, instance);
}

pub(super) fn provide(providers: &mut Providers) {
    *providers = Providers { check_feature_dependent_abi, ..*providers }
}
