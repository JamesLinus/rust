// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Give useful errors and suggestions to users when an item can't be
//! found or is otherwise invalid.

use check::FnCtxt;
use rustc::hir::map as hir_map;
use rustc::ty::{self, Ty, TyCtxt, ToPolyTraitRef, ToPredicate, TypeFoldable};
use hir::def::Def;
use hir::def_id::{CRATE_DEF_INDEX, DefId};
use middle::lang_items::FnOnceTraitLangItem;
use rustc::traits::{Obligation, SelectionContext};
use util::nodemap::FxHashSet;

use syntax::ast;
use errors::DiagnosticBuilder;
use syntax_pos::Span;

use rustc::hir;
use rustc::hir::print;
use rustc::infer::type_variable::TypeVariableOrigin;

use std::cell;
use std::cmp::Ordering;

use super::{MethodError, NoMatchData, CandidateSource};
use super::probe::Mode;

impl<'a, 'gcx, 'tcx> FnCtxt<'a, 'gcx, 'tcx> {
    fn is_fn_ty(&self, ty: &Ty<'tcx>, span: Span) -> bool {
        let tcx = self.tcx;
        match ty.sty {
            // Not all of these (e.g. unsafe fns) implement FnOnce
            // so we look for these beforehand
            ty::TyClosure(..) |
            ty::TyFnDef(..) |
            ty::TyFnPtr(_) => true,
            // If it's not a simple function, look for things which implement FnOnce
            _ => {
                let fn_once = match tcx.lang_items().require(FnOnceTraitLangItem) {
                    Ok(fn_once) => fn_once,
                    Err(..) => return false,
                };

                self.autoderef(span, ty).any(|(ty, _)| {
                    self.probe(|_| {
                        let fn_once_substs = tcx.mk_substs_trait(ty,
                            &[self.next_ty_var(TypeVariableOrigin::MiscVariable(span))]);
                        let trait_ref = ty::TraitRef::new(fn_once, fn_once_substs);
                        let poly_trait_ref = trait_ref.to_poly_trait_ref();
                        let obligation =
                            Obligation::misc(span,
                                             self.body_id,
                                             self.param_env,
                                             poly_trait_ref.to_predicate());
                        SelectionContext::new(self).evaluate_obligation(&obligation)
                    })
                })
            }
        }
    }

    pub fn report_method_error(&self,
                               span: Span,
                               rcvr_ty: Ty<'tcx>,
                               item_name: ast::Name,
                               rcvr_expr: Option<&hir::Expr>,
                               error: MethodError<'tcx>,
                               args: Option<&'gcx [hir::Expr]>) {
        // avoid suggestions when we don't know what's going on.
        if rcvr_ty.references_error() {
            return;
        }

        let report_candidates = |err: &mut DiagnosticBuilder, mut sources: Vec<CandidateSource>| {

            sources.sort();
            sources.dedup();
            // Dynamic limit to avoid hiding just one candidate, which is silly.
            let limit = if sources.len() == 5 { 5 } else { 4 };

            for (idx, source) in sources.iter().take(limit).enumerate() {
                match *source {
                    CandidateSource::ImplSource(impl_did) => {
                        // Provide the best span we can. Use the item, if local to crate, else
                        // the impl, if local to crate (item may be defaulted), else nothing.
                        let item = self.associated_item(impl_did, item_name)
                            .or_else(|| {
                                self.associated_item(
                                    self.tcx.impl_trait_ref(impl_did).unwrap().def_id,

                                    item_name
                                )
                            }).unwrap();
                        let note_span = self.tcx.hir.span_if_local(item.def_id).or_else(|| {
                            self.tcx.hir.span_if_local(impl_did)
                        });

                        let impl_ty = self.impl_self_ty(span, impl_did).ty;

                        let insertion = match self.tcx.impl_trait_ref(impl_did) {
                            None => format!(""),
                            Some(trait_ref) => {
                                format!(" of the trait `{}`",
                                        self.tcx.item_path_str(trait_ref.def_id))
                            }
                        };

                        let note_str = format!("candidate #{} is defined in an impl{} \
                                                for the type `{}`",
                                               idx + 1,
                                               insertion,
                                               impl_ty);
                        if let Some(note_span) = note_span {
                            // We have a span pointing to the method. Show note with snippet.
                            err.span_note(note_span, &note_str);
                        } else {
                            err.note(&note_str);
                        }
                    }
                    CandidateSource::TraitSource(trait_did) => {
                        let item = self.associated_item(trait_did, item_name).unwrap();
                        let item_span = self.tcx.def_span(item.def_id);
                        span_note!(err,
                                   item_span,
                                   "candidate #{} is defined in the trait `{}`",
                                   idx + 1,
                                   self.tcx.item_path_str(trait_did));
                        err.help(&format!("to disambiguate the method call, write `{}::{}({}{})` \
                                          instead",
                                          self.tcx.item_path_str(trait_did),
                                          item_name,
                                          if rcvr_ty.is_region_ptr() && args.is_some() {
                                              if rcvr_ty.is_mutable_pointer() {
                                                  "&mut "
                                              } else {
                                                  "&"
                                              }
                                          } else {
                                              ""
                                          },
                                          args.map(|arg| arg.iter()
                                              .map(|arg| print::to_string(print::NO_ANN,
                                                                          |s| s.print_expr(arg)))
                                              .collect::<Vec<_>>()
                                              .join(", ")).unwrap_or("...".to_owned())));
                    }
                }
            }
            if sources.len() > limit {
                err.note(&format!("and {} others", sources.len() - limit));
            }
        };

        match error {
            MethodError::NoMatch(NoMatchData { static_candidates: static_sources,
                                               unsatisfied_predicates,
                                               out_of_scope_traits,
                                               lev_candidate,
                                               mode,
                                               .. }) => {
                let tcx = self.tcx;

                let actual = self.resolve_type_vars_if_possible(&rcvr_ty);
                let mut err = if !actual.references_error() {
                    struct_span_err!(tcx.sess, span, E0599,
                                     "no {} named `{}` found for type `{}` in the \
                                      current scope",
                                     if mode == Mode::MethodCall {
                                         "method"
                                     } else {
                                         match item_name.as_str().chars().next() {
                                             Some(name) => {
                                                 if name.is_lowercase() {
                                                     "function or associated item"
                                                 } else {
                                                     "associated item"
                                                 }
                                             },
                                             None => {
                                                 ""
                                             },
                                         }
                                     },
                                     item_name,
                                     self.ty_to_string(actual))
                } else {
                    self.tcx.sess.diagnostic().struct_dummy()
                };

                // If the method name is the name of a field with a function or closure type,
                // give a helping note that it has to be called as (x.f)(...).
                if let Some(expr) = rcvr_expr {
                    for (ty, _) in self.autoderef(span, rcvr_ty) {
                        match ty.sty {
                            ty::TyAdt(def, substs) if !def.is_enum() => {
                                if let Some(field) = def.struct_variant()
                                    .find_field_named(item_name) {
                                    let snippet = tcx.sess.codemap().span_to_snippet(expr.span);
                                    let expr_string = match snippet {
                                        Ok(expr_string) => expr_string,
                                        _ => "s".into(), // Default to a generic placeholder for the
                                        // expression when we can't generate a
                                        // string snippet
                                    };

                                    let field_ty = field.ty(tcx, substs);
                                    let scope = self.tcx.hir.get_module_parent(self.body_id);
                                    if field.vis.is_accessible_from(scope, self.tcx) {
                                        if self.is_fn_ty(&field_ty, span) {
                                            err.help(&format!("use `({0}.{1})(...)` if you \
                                                               meant to call the function \
                                                               stored in the `{1}` field",
                                                              expr_string,
                                                              item_name));
                                        } else {
                                            err.help(&format!("did you mean to write `{0}.{1}` \
                                                               instead of `{0}.{1}(...)`?",
                                                              expr_string,
                                                              item_name));
                                        }
                                        err.span_label(span, "field, not a method");
                                    } else {
                                        err.span_label(span, "private field, not a method");
                                    }
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }

                if self.is_fn_ty(&rcvr_ty, span) {
                    macro_rules! report_function {
                        ($span:expr, $name:expr) => {
                            err.note(&format!("{} is a function, perhaps you wish to call it",
                                              $name));
                        }
                    }

                    if let Some(expr) = rcvr_expr {
                        if let Ok(expr_string) = tcx.sess.codemap().span_to_snippet(expr.span) {
                            report_function!(expr.span, expr_string);
                        } else if let hir::ExprPath(hir::QPath::Resolved(_, ref path)) = expr.node {
                            if let Some(segment) = path.segments.last() {
                                report_function!(expr.span, segment.name);
                            }
                        }
                    }
                }

                if !static_sources.is_empty() {
                    err.note("found the following associated functions; to be used as methods, \
                              functions must have a `self` parameter");
                    err.help(&format!("try with `{}::{}`", self.ty_to_string(actual), item_name));

                    report_candidates(&mut err, static_sources);
                }

                if !unsatisfied_predicates.is_empty() {
                    let bound_list = unsatisfied_predicates.iter()
                        .map(|p| format!("`{} : {}`", p.self_ty(), p))
                        .collect::<Vec<_>>()
                        .join("\n");
                    err.note(&format!("the method `{}` exists but the following trait bounds \
                                       were not satisfied:\n{}",
                                      item_name,
                                      bound_list));
                }

                self.suggest_traits_to_import(&mut err,
                                              span,
                                              rcvr_ty,
                                              item_name,
                                              rcvr_expr,
                                              out_of_scope_traits);

                if let Some(lev_candidate) = lev_candidate {
                    err.help(&format!("did you mean `{}`?", lev_candidate.name));
                }
                err.emit();
            }

            MethodError::Ambiguity(sources) => {
                let mut err = struct_span_err!(self.sess(),
                                               span,
                                               E0034,
                                               "multiple applicable items in scope");
                err.span_label(span, format!("multiple `{}` found", item_name));

                report_candidates(&mut err, sources);
                err.emit();
            }

            MethodError::PrivateMatch(def, out_of_scope_traits) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0624,
                                               "{} `{}` is private", def.kind_name(), item_name);
                self.suggest_valid_traits(&mut err, out_of_scope_traits);
                err.emit();
            }

            MethodError::IllegalSizedBound(candidates) => {
                let msg = format!("the `{}` method cannot be invoked on a trait object", item_name);
                let mut err = self.sess().struct_span_err(span, &msg);
                if !candidates.is_empty() {
                    let help = format!("{an}other candidate{s} {were} found in the following \
                                        trait{s}, perhaps add a `use` for {one_of_them}:",
                                    an = if candidates.len() == 1 {"an" } else { "" },
                                    s = if candidates.len() == 1 { "" } else { "s" },
                                    were = if candidates.len() == 1 { "was" } else { "were" },
                                    one_of_them = if candidates.len() == 1 {
                                        "it"
                                    } else {
                                        "one_of_them"
                                    });
                    self.suggest_use_candidates(&mut err, help, candidates);
                }
                err.emit();
            }

            MethodError::BadReturnType => {
                bug!("no return type expectations but got BadReturnType")
            }
        }
    }

    fn suggest_use_candidates(&self,
                              err: &mut DiagnosticBuilder,
                              mut msg: String,
                              candidates: Vec<DefId>) {
        let limit = if candidates.len() == 5 { 5 } else { 4 };
        for (i, trait_did) in candidates.iter().take(limit).enumerate() {
            msg.push_str(&format!("\ncandidate #{}: `use {};`",
                                    i + 1,
                                    self.tcx.item_path_str(*trait_did)));
        }
        if candidates.len() > limit {
            msg.push_str(&format!("\nand {} others", candidates.len() - limit));
        }
        err.note(&msg[..]);
    }

    fn suggest_valid_traits(&self,
                            err: &mut DiagnosticBuilder,
                            valid_out_of_scope_traits: Vec<DefId>) -> bool {
        if !valid_out_of_scope_traits.is_empty() {
            let mut candidates = valid_out_of_scope_traits;
            candidates.sort();
            candidates.dedup();
            err.help("items from traits can only be used if the trait is in scope");
            let msg = format!("the following {traits_are} implemented but not in scope, \
                               perhaps add a `use` for {one_of_them}:",
                            traits_are = if candidates.len() == 1 {
                                "trait is"
                            } else {
                                "traits are"
                            },
                            one_of_them = if candidates.len() == 1 {
                                "it"
                            } else {
                                "one of them"
                            });

            self.suggest_use_candidates(err, msg, candidates);
            true
        } else {
            false
        }
    }

    fn suggest_traits_to_import(&self,
                                err: &mut DiagnosticBuilder,
                                span: Span,
                                rcvr_ty: Ty<'tcx>,
                                item_name: ast::Name,
                                rcvr_expr: Option<&hir::Expr>,
                                valid_out_of_scope_traits: Vec<DefId>) {
        if self.suggest_valid_traits(err, valid_out_of_scope_traits) {
            return;
        }

        let type_is_local = self.type_derefs_to_local(span, rcvr_ty, rcvr_expr);

        // there's no implemented traits, so lets suggest some traits to
        // implement, by finding ones that have the item name, and are
        // legal to implement.
        let mut candidates = all_traits(self.tcx)
            .filter(|info| {
                // we approximate the coherence rules to only suggest
                // traits that are legal to implement by requiring that
                // either the type or trait is local. Multidispatch means
                // this isn't perfect (that is, there are cases when
                // implementing a trait would be legal but is rejected
                // here).
                (type_is_local || info.def_id.is_local())
                    && self.associated_item(info.def_id, item_name).is_some()
            })
            .collect::<Vec<_>>();

        if !candidates.is_empty() {
            // sort from most relevant to least relevant
            candidates.sort_by(|a, b| a.cmp(b).reverse());
            candidates.dedup();

            // FIXME #21673 this help message could be tuned to the case
            // of a type parameter: suggest adding a trait bound rather
            // than implementing.
            err.help("items from traits can only be used if the trait is implemented and in scope");
            let mut msg = format!("the following {traits_define} an item `{name}`, \
                                   perhaps you need to implement {one_of_them}:",
                                  traits_define = if candidates.len() == 1 {
                                      "trait defines"
                                  } else {
                                      "traits define"
                                  },
                                  one_of_them = if candidates.len() == 1 {
                                      "it"
                                  } else {
                                      "one of them"
                                  },
                                  name = item_name);

            for (i, trait_info) in candidates.iter().enumerate() {
                msg.push_str(&format!("\ncandidate #{}: `{}`",
                                      i + 1,
                                      self.tcx.item_path_str(trait_info.def_id)));
            }
            err.note(&msg[..]);
        }
    }

    /// Checks whether there is a local type somewhere in the chain of
    /// autoderefs of `rcvr_ty`.
    fn type_derefs_to_local(&self,
                            span: Span,
                            rcvr_ty: Ty<'tcx>,
                            rcvr_expr: Option<&hir::Expr>)
                            -> bool {
        fn is_local(ty: Ty) -> bool {
            match ty.sty {
                ty::TyAdt(def, _) => def.did.is_local(),

                ty::TyDynamic(ref tr, ..) => tr.principal()
                    .map_or(false, |p| p.def_id().is_local()),

                ty::TyParam(_) => true,

                // everything else (primitive types etc.) is effectively
                // non-local (there are "edge" cases, e.g. (LocalType,), but
                // the noise from these sort of types is usually just really
                // annoying, rather than any sort of help).
                _ => false,
            }
        }

        // This occurs for UFCS desugaring of `T::method`, where there is no
        // receiver expression for the method call, and thus no autoderef.
        if rcvr_expr.is_none() {
            return is_local(self.resolve_type_vars_with_obligations(rcvr_ty));
        }

        self.autoderef(span, rcvr_ty).any(|(ty, _)| is_local(ty))
    }
}

pub type AllTraitsVec = Vec<DefId>;

#[derive(Copy, Clone)]
pub struct TraitInfo {
    pub def_id: DefId,
}

impl TraitInfo {
    fn new(def_id: DefId) -> TraitInfo {
        TraitInfo { def_id: def_id }
    }
}
impl PartialEq for TraitInfo {
    fn eq(&self, other: &TraitInfo) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for TraitInfo {}
impl PartialOrd for TraitInfo {
    fn partial_cmp(&self, other: &TraitInfo) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TraitInfo {
    fn cmp(&self, other: &TraitInfo) -> Ordering {
        // local crates are more important than remote ones (local:
        // cnum == 0), and otherwise we throw in the defid for totality

        let lhs = (other.def_id.krate, other.def_id);
        let rhs = (self.def_id.krate, self.def_id);
        lhs.cmp(&rhs)
    }
}

/// Retrieve all traits in this crate and any dependent crates.
pub fn all_traits<'a, 'gcx, 'tcx>(tcx: TyCtxt<'a, 'gcx, 'tcx>) -> AllTraits<'a> {
    if tcx.all_traits.borrow().is_none() {
        use rustc::hir::itemlikevisit;

        let mut traits = vec![];

        // Crate-local:
        //
        // meh.
        struct Visitor<'a, 'tcx: 'a> {
            map: &'a hir_map::Map<'tcx>,
            traits: &'a mut AllTraitsVec,
        }
        impl<'v, 'a, 'tcx> itemlikevisit::ItemLikeVisitor<'v> for Visitor<'a, 'tcx> {
            fn visit_item(&mut self, i: &'v hir::Item) {
                match i.node {
                    hir::ItemTrait(..) => {
                        let def_id = self.map.local_def_id(i.id);
                        self.traits.push(def_id);
                    }
                    _ => {}
                }
            }

            fn visit_trait_item(&mut self, _trait_item: &hir::TraitItem) {
            }

            fn visit_impl_item(&mut self, _impl_item: &hir::ImplItem) {
            }
        }
        tcx.hir.krate().visit_all_item_likes(&mut Visitor {
            map: &tcx.hir,
            traits: &mut traits,
        });

        // Cross-crate:
        let mut external_mods = FxHashSet();
        fn handle_external_def(tcx: TyCtxt,
                               traits: &mut AllTraitsVec,
                               external_mods: &mut FxHashSet<DefId>,
                               def: Def) {
            let def_id = def.def_id();
            match def {
                Def::Trait(..) => {
                    traits.push(def_id);
                }
                Def::Mod(..) => {
                    if !external_mods.insert(def_id) {
                        return;
                    }
                    for child in tcx.item_children(def_id).iter() {
                        handle_external_def(tcx, traits, external_mods, child.def)
                    }
                }
                _ => {}
            }
        }
        for &cnum in tcx.crates().iter() {
            let def_id = DefId {
                krate: cnum,
                index: CRATE_DEF_INDEX,
            };
            handle_external_def(tcx, &mut traits, &mut external_mods, Def::Mod(def_id));
        }

        *tcx.all_traits.borrow_mut() = Some(traits);
    }

    let borrow = tcx.all_traits.borrow();
    assert!(borrow.is_some());
    AllTraits {
        borrow,
        idx: 0,
    }
}

pub struct AllTraits<'a> {
    borrow: cell::Ref<'a, Option<AllTraitsVec>>,
    idx: usize,
}

impl<'a> Iterator for AllTraits<'a> {
    type Item = TraitInfo;

    fn next(&mut self) -> Option<TraitInfo> {
        let AllTraits { ref borrow, ref mut idx } = *self;
        // ugh.
        borrow.as_ref().unwrap().get(*idx).map(|info| {
            *idx += 1;
            TraitInfo::new(*info)
        })
    }
}
