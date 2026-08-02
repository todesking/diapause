//! Code generation: turns the lowered CFG and its analysis into the
//! state enum, the `Coroutine` impl, and the `__drive` dispatch loop.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::analyze_cfg::{self, Analysis, ArgInfo, InPlacePlan, Variant};
use crate::args::{Fingerprint, MacroArgs};
use crate::cfg::{BindingId, BlockId, Cfg, OpaqueJumpKind, ResumeBinding, Terminator, TySource};
use crate::lower::{self, PatBindingCollector, skip_nested_scopes};
use crate::signature::{
    ArgVar, augment_generics, check_return_type, check_signature, parse_args,
    phantom_for_unused_params,
};

pub fn expand(attr: TokenStream, item: syn::ItemFn) -> syn::Result<TokenStream> {
    let prepared = Prepared::parse(attr, item)?;
    let cfg = prepared.lower()?;
    let analysis = prepared.analyze(&cfg)?;

    // Self-check in the spirit of rustc's MIR validation: re-derive the
    // invariants the codegen below relies on and abort loudly on any
    // violation. Only debug builds of the macro itself run it (tests,
    // difftest); a release build of user code skips it entirely. A
    // failure is a diapause-macro bug, never a user error, hence the
    // panic instead of a compile error.
    if cfg!(debug_assertions)
        && let Err(msg) = crate::validate::validate(&cfg, &analysis, prepared.args.len())
    {
        panic!(
            "internal IR validation failed for coroutine `{}` (this is a bug in \
             diapause-macro):\n{msg}",
            prepared.item.sig.ident
        );
    }

    Ok(prepared.codegen(&cfg, &analysis))
}

/// Intermediate artifacts of one expansion, produced by [`expand_debug`]
/// for debugging and visualization tooling (the playground). Each field
/// is the output of one pipeline stage; when a stage fails, the fields
/// of that stage and everything after it are `None` and `result` carries
/// the stage's error.
#[derive(Debug)]
pub struct DebugExpansion {
    /// The CFG exactly as lowered, before the simplification pass
    /// (goto-chain merging, unreachable-block removal, inlining).
    pub cfg_unsimplified: Option<Cfg>,
    /// The simplified CFG that analysis and codegen consume.
    pub cfg: Option<Cfg>,
    /// State-variant layout, liveness, and borrow reconstruction info.
    pub analysis: Option<Analysis>,
    /// The generated code, or the first failing stage's error.
    pub result: syn::Result<TokenStream>,
}

/// Runs the same pipeline as [`expand`] but returns the intermediate
/// artifacts of every stage alongside the result (see [`DebugExpansion`]).
///
/// Two deliberate differences from `expand`: a stage failure still
/// returns the artifacts of the stages before it, and an IR validation
/// failure is reported as an error in `result` (with the offending CFG
/// and analysis attached) instead of a panic.
pub fn expand_debug(attr: TokenStream, item: syn::ItemFn) -> DebugExpansion {
    let mut cfg_unsimplified = None;
    let mut cfg = None;
    let mut analysis = None;
    let result = expand_debug_stages(attr, item, &mut cfg_unsimplified, &mut cfg, &mut analysis);
    DebugExpansion {
        cfg_unsimplified,
        cfg,
        analysis,
        result,
    }
}

/// [`expand_debug`]'s pipeline: stores each stage's output through the
/// out-parameters as soon as it exists, so an early `?` return leaves
/// everything already produced in place.
fn expand_debug_stages(
    attr: TokenStream,
    item: syn::ItemFn,
    cfg_unsimplified: &mut Option<Cfg>,
    cfg_out: &mut Option<Cfg>,
    analysis_out: &mut Option<Analysis>,
) -> syn::Result<TokenStream> {
    let prepared = Prepared::parse(attr, item)?;
    let mut cfg = prepared.lower_unsimplified()?;
    *cfg_unsimplified = Some(cfg.clone());
    crate::cfg::simplify(&mut cfg);
    let cfg = &*cfg_out.insert(cfg);
    let analysis = &*analysis_out.insert(prepared.analyze(cfg)?);
    // Unlike `expand`, run the validator unconditionally and surface a
    // failure as an error: a debugging front end wants to display the
    // invalid IR rather than crash.
    if let Err(msg) = crate::validate::validate(cfg, analysis, prepared.args.len()) {
        return Err(syn::Error::new(
            prepared.item.sig.ident.span(),
            format!(
                "internal IR validation failed for coroutine `{}` (this is a bug in \
                 diapause-macro):\n{msg}",
                prepared.item.sig.ident
            ),
        ));
    }
    Ok(prepared.codegen(cfg, analysis))
}

/// Output of the pre-CFG stages of the pipeline: the parsed and checked
/// item plus its rewritten body, ready to be lowered. Splitting this out
/// of [`expand`] lets `expand_debug` run the same stages one at a time
/// and capture the intermediate artifacts.
struct Prepared {
    item: syn::ItemFn,
    /// The body after yield hoisting and early-exit rewriting; this is
    /// what lowering consumes (the original stays in `item`).
    body: syn::Block,
    macro_args: MacroArgs,
    fingerprint: u64,
    args: Vec<ArgVar>,
    generics: syn::Generics,
    ret_ty: syn::Type,
}

impl Prepared {
    /// Parses the attribute arguments, checks the signature, and rewrites
    /// the body (yield hoisting, early-exit rewriting).
    fn parse(attr: TokenStream, item: syn::ItemFn) -> syn::Result<Self> {
        // Hashed before the body is rewritten, from the exact source tokens.
        let fingerprint = source_fingerprint(&attr, &item.sig, &item.block);
        let macro_args: MacroArgs = syn::parse2(attr)?;
        // A manual `fingerprint = "tag"` replaces the source hash: states
        // persisted under equal tags are declared compatible by the user.
        let fingerprint = match &macro_args.fingerprint {
            Fingerprint::Manual(tag) => fnv1a(FNV_OFFSET_BASIS, tag.value().as_bytes()),
            _ => fingerprint,
        };

        check_signature(&item.sig)?;
        let mut args = parse_args(&item.sig)?;
        let generics = augment_generics(&item.sig, &mut args)?;

        let ret_ty: syn::Type = match &item.sig.output {
            syn::ReturnType::Default => syn::parse_quote!(()),
            syn::ReturnType::Type(_, ty) => (**ty).clone(),
        };
        check_return_type(&ret_ty)?;

        // Expression-position yields with a pure evaluation prefix become
        // `let __tmpN = yield_!(..);` statements, so lowering only sees the
        // native statement forms. Runs first: `?` must still be visible as
        // `Expr::Try` (an effect ending the prefix).
        let mut body = (*item.block).clone();
        // A `?` applied directly to a delegation macro splits into
        // binding the completion value first; runs before the passes
        // below so they only see the native forms (a plain `?` and a
        // statement-position delegation).
        rewrite_delegate_try(&mut body);
        crate::hoist::hoist_yields(&mut body);
        // Early `return e` and `e?` become completion transitions everywhere
        // in the body, including inside opaque statements; the rewritten form
        // only makes sense inside `__drive`, where `self` and `State` resolve.
        rewrite_early_exits(&mut body, &ret_ty);

        Ok(Prepared {
            item,
            body,
            macro_args,
            fingerprint,
            args,
            generics,
            ret_ty,
        })
    }

    /// Lowers the rewritten body to the simplified CFG.
    fn lower(&self) -> syn::Result<Cfg> {
        let (arg_idents, arg_pats) = self.lower_inputs();
        lower::lower(&arg_idents, &arg_pats, &self.body)
    }

    /// Like [`Self::lower`], but stops before the simplification pass;
    /// `expand_debug` snapshots this stage and simplifies afterwards.
    fn lower_unsimplified(&self) -> syn::Result<Cfg> {
        let (arg_idents, arg_pats) = self.lower_inputs();
        lower::lower_unsimplified(&arg_idents, &arg_pats, &self.body)
    }

    /// The argument names and destructuring patterns lowering starts from.
    #[allow(clippy::type_complexity)]
    fn lower_inputs(&self) -> (Vec<syn::Ident>, Vec<(syn::Pat, syn::Ident)>) {
        let arg_idents = self.args.iter().map(|a| a.ident.clone()).collect();
        let arg_pats = self
            .args
            .iter()
            .filter_map(|a| a.pattern.clone().map(|p| (p, a.ident.clone())))
            .collect();
        (arg_idents, arg_pats)
    }

    fn analyze(&self, cfg: &Cfg) -> syn::Result<Analysis> {
        let arg_infos: Vec<ArgInfo> = self.args.iter().map(ArgInfo::from).collect();
        analyze_cfg::analyze_with(
            cfg,
            &arg_infos,
            &self.macro_args.resume_ty,
            self.macro_args.in_place,
        )
    }

    /// Turns the CFG and its analysis into the generated item tokens.
    /// Infallible: every user error is diagnosed by the stages above.
    fn codegen(&self, cfg: &Cfg, analysis: &Analysis) -> TokenStream {
        let Prepared {
            item,
            macro_args,
            fingerprint,
            args,
            generics,
            ret_ty,
            ..
        } = self;
        let fingerprint = *fingerprint;
        let fp_enabled = macro_args.fingerprint.enabled();

        // `#[derive(...)]` written below the coroutine attribute applies to
        // the generated State enum; everything else (doc comments, allow,
        // etc.) stays on the starter fn.
        let (derive_attrs, fn_attrs): (Vec<&syn::Attribute>, Vec<&syn::Attribute>) =
            item.attrs.iter().partition(|a| a.path().is_ident("derive"));
        let vis = &item.vis;
        let name = &item.sig.ident;
        let yield_ty = &macro_args.yield_ty;
        let resume_ty = &macro_args.resume_ty;

        let arg_ident: Vec<&syn::Ident> = args.iter().map(|a| &a.ident).collect();
        let arg_ty: Vec<&syn::Type> = args.iter().map(|a| &a.ty).collect();
        let arg_pat: Vec<TokenStream> = args
            .iter()
            .map(|a| bind_pat(&a.mutability, &a.ident))
            .collect();

        // Without yields the body is a single transition, so no panic can
        // occur between a state write and the return: Done doubles as the
        // placeholder, Poisoned is omitted, and every resume is
        // unconditionally an error. `Suspension::new` is the only place that
        // branches on that condition; everything downstream only sees the
        // consequences.
        let n_yields = cfg.blocks.iter().filter(|b| b.resume_point).count();
        let suspension = Suspension::new(n_yields);

        // The `fingerprint` flag threads a plain `__fp: u64` field through
        // every data-carrying variant: initialized to FINGERPRINT at
        // construction and on every transition, checked on entry to
        // `start`/`resume` (the guard runs with `__fp` bound by the match).
        let fp_guard = fp_enabled.then(|| {
            let msg = format!("this state was created by a different version of `{name}`");
            quote! {
                if *__fp != Self::FINGERPRINT {
                    ::core::panic!(#msg);
                }
            }
        });
        let fp_bind = fp_enabled.then(|| quote!(__fp,));
        let fp_init = fp_enabled.then(|| quote!(__fp: #fingerprint,));

        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let cx = ExpandCtx {
            generics,
            where_clause,
            cfg,
            analysis,
            arg_ident: &arg_ident,
            arg_ty: &arg_ty,
            arg_pat: &arg_pat,
            yield_ty,
            ret_ty,
            resume_ty,
            fp_guard: fp_guard.as_ref(),
        };

        let StateEnum {
            tokens: state_enum_tokens,
            phantom_init,
        } = build_state_enum(&cx, &derive_attrs, &suspension.poisoned_variant);
        let range_iter_def = cfg
            .bindings
            .iter()
            .any(|b| matches!(b.ty, TySource::RangeInclusiveIter(_)))
            .then(|| range_inclusive_iter_def(&derive_attrs));
        let resume_body = build_resume_dispatch(&cx, &suspension);
        let status_body = build_status_dispatch(&cx, &suspension);
        let drive_fn = build_drive_fn(&cx, &suspension.placeholder);
        let fp_impl = fp_enabled.then(|| build_fingerprinted_impl(&cx, fingerprint));

        // A coroutine driven with `()` resume values can be iterated, so
        // emit an `IntoIterator` into `diapause::Iter` that feeds `for`
        // loops without an explicit `Iter::new`. Guarded on a syntactic
        // `resume = ()` (covers both the explicit form and the default).
        // Only the by-value impl is generated: an `IntoIterator` for
        // `&mut State` would make rustc's reachability analysis mark the
        // state fields pub-reachable and fire `private_interfaces` on
        // private coroutines holding crate-private types; partial
        // iteration goes through `Iter::new(&mut c)` instead (via the
        // blanket `Coroutine for &mut C` impl).
        let into_iter_impl = is_unit_ty(resume_ty).then(|| {
            quote! {
                impl #impl_generics ::core::iter::IntoIterator for State #ty_generics
                #where_clause
                {
                    type Item = #yield_ty;
                    type IntoIter = ::diapause::Iter<Self>;
                    fn into_iter(self) -> Self::IntoIter {
                        ::diapause::Iter::new(self)
                    }
                }
            }
        });

        quote! {
        #(#fn_attrs)*
        #vis fn #name #impl_generics (#(#arg_ident: #arg_ty),*) -> #name::State #ty_generics
        #where_clause
        {
            #name::State::Start { #(#arg_ident,)* #fp_init #phantom_init }
        }

        #vis mod #name {
            #[allow(unused_imports)]
            use super::*;

            #state_enum_tokens

            #range_iter_def

            impl #impl_generics ::diapause::Coroutine<#resume_ty> for State #ty_generics
            #where_clause
            {
                type Yield = #yield_ty;
                type Return = #ret_ty;

                fn start(&mut self) -> ::diapause::CoroutineState<#yield_ty, #ret_ty> {
                    match self {
                        State::Start { #fp_bind .. } => { #fp_guard }
                        _ => ::core::panic!("Already started"),
                    }
                    self.__drive(::core::option::Option::None)
                }

                fn resume(
                    &mut self,
                    _resume: #resume_ty,
                ) -> ::diapause::CoroutineState<#yield_ty, #ret_ty> {
                    #resume_body
                }

                fn status(&self) -> ::diapause::CoroutineStatus {
                    #status_body
                }
            }

            #into_iter_impl

            #fp_impl

            impl #impl_generics State #ty_generics #where_clause {
                /// Fingerprint of the coroutine source this state type was
                /// generated from: an FNV-1a hash of the attribute
                /// arguments, signature, and body tokens. Editing the
                /// coroutine changes it; comments and formatting do not.
                pub const FINGERPRINT: u64 = #fingerprint;

                #drive_fn
            }
        }
        }
    }
}

// === Codegen assembly ===
//
// `expand` above parses and validates the source item, then hands off to
// the four pieces below: the n_yields==0 special case (`Suspension`), the
// `State` enum (`build_state_enum`), `resume()`'s dispatch
// (`build_resume_dispatch`), and `__drive` (`build_drive_fn`).

/// The pieces of the parsed item that the codegen-assembly functions below
/// need but none of them owns.
struct ExpandCtx<'a> {
    generics: &'a syn::Generics,
    where_clause: Option<&'a syn::WhereClause>,
    cfg: &'a Cfg,
    analysis: &'a Analysis,
    arg_ident: &'a [&'a syn::Ident],
    arg_ty: &'a [&'a syn::Type],
    arg_pat: &'a [TokenStream],
    yield_ty: &'a syn::Type,
    ret_ty: &'a syn::Type,
    resume_ty: &'a syn::Type,
    /// `Some` iff the `fingerprint` flag was given: the mismatch check
    /// to run where a dispatch arm has bound `__fp`.
    fp_guard: Option<&'a TokenStream>,
}

impl ExpandCtx<'_> {
    fn fp_enabled(&self) -> bool {
        self.fp_guard.is_some()
    }
}

/// Whether the coroutine ever suspends, and the codegen fallout of that:
/// without yields, the body is a single transition, so no panic can occur
/// between a state write and the return. `Done` then doubles as the
/// placeholder, `Poisoned` is omitted, and (in `build_resume_dispatch`)
/// every resume is unconditionally an error.
struct Suspension {
    poisoned_variant: TokenStream,
    placeholder: TokenStream,
    poisoned_arm: Option<TokenStream>,
    has_yields: bool,
}

impl Suspension {
    fn new(n_yields: usize) -> Self {
        if n_yields == 0 {
            Suspension {
                poisoned_variant: quote!(),
                placeholder: quote!(State::Done),
                poisoned_arm: None,
                has_yields: false,
            }
        } else {
            Suspension {
                poisoned_variant: quote!(Poisoned,),
                placeholder: quote!(State::Poisoned),
                poisoned_arm: Some(quote!(State::Poisoned => ::core::panic!("Poisoned"),)),
                has_yields: true,
            }
        }
    }
}

struct StateEnum {
    /// The `enum State { .. }` item, ready to splice into `mod #name`.
    tokens: TokenStream,
    /// `PhantomData` field value used by the free-standing starter
    /// function to build the initial `State::Start`.
    phantom_init: Option<TokenStream>,
}

/// Builds the `enum State` declaration: variant list (in `BlockId` order,
/// entry excluded since it becomes `Start`), the `PhantomData` field/init
/// pair anchoring otherwise-unconstrained generic parameters, and
/// `Poisoned`'s presence (`poisoned_variant`, from `Suspension`).
fn build_state_enum(
    cx: &ExpandCtx,
    derive_attrs: &[&syn::Attribute],
    poisoned_variant: &TokenStream,
) -> StateEnum {
    // The fingerprint field is ordinary data to user derives (serde,
    // Clone, ...); its meaning lives entirely in the generated checks.
    let fp_field = cx.fp_enabled().then(|| quote!(__fp: u64,));

    // Variant declarations in BlockId order: deterministic, and linear
    // bodies produce a `Start, S1..Sn` layout.
    let state_variants: Vec<TokenStream> = cx
        .analysis
        .variants
        .iter()
        .filter(|v| v.block != cx.cfg.entry)
        .map(|v| {
            let ident = &v.ident;
            let field_defs = v.fields.iter().map(|f| {
                let ident = &f.ident;
                let ty = &f.ty;
                quote!(#ident: #ty)
            });
            quote!(#ident { #(#field_defs,)* #fp_field })
        })
        .collect();

    // A generic parameter used only inside the body would leave the enum
    // with an unconstrained parameter (E0392); a PhantomData field in
    // Start keeps such parameters anchored.
    let all_field_tys = cx.arg_ty.iter().copied().chain(
        cx.analysis
            .variants
            .iter()
            .flat_map(|v| &v.fields)
            .map(|f| &f.ty),
    );
    let phantom_ty = phantom_for_unused_params(cx.generics, all_field_tys);
    let phantom_field = phantom_ty
        .as_ref()
        .map(|ty| quote!(__phantom: ::core::marker::PhantomData<#ty>,));
    let phantom_init = phantom_ty
        .as_ref()
        .map(|_| quote!(__phantom: ::core::marker::PhantomData,));

    let generics = cx.generics;
    let where_clause = cx.where_clause;
    let arg_ident = cx.arg_ident;
    let arg_ty = cx.arg_ty;
    let tokens = quote! {
        #(#derive_attrs)*
        pub enum State #generics #where_clause {
            Start { #(#arg_ident: #arg_ty,)* #fp_field #phantom_field },
            #(#state_variants,)*
            Done,
            #poisoned_variant
        }
    };
    StateEnum {
        tokens,
        phantom_init,
    }
}

/// Builds the iterator stored for `for _ in a..=b` loops (emitted only
/// when the body has one). `RangeInclusive` itself cannot be stored:
/// its serde impl serializes only `start`/`end` and drops the internal
/// exhaustion flag, so a state persisted right after the final element
/// was yielded would re-yield that element forever after a round trip.
/// The explicit `done` field round-trips exactly, and iteration
/// delegates to `RangeInclusive` so the stepping semantics stay std's.
/// Generated into the coroutine module so the user's derives (serde,
/// Clone, ...) apply to it exactly as to the state enum.
fn range_inclusive_iter_def(derive_attrs: &[&syn::Attribute]) -> TokenStream {
    quote! {
        #(#derive_attrs)*
        #[allow(dead_code)]
        pub struct __RangeInclusiveIter<T> {
            pub start: T,
            pub end: T,
            pub done: bool,
        }

        #[allow(dead_code)]
        impl<T: ::core::cmp::PartialOrd> __RangeInclusiveIter<T> {
            fn new(range: ::core::ops::RangeInclusive<T>) -> Self {
                // `is_empty` sees the exhaustion flag, so converting an
                // already-exhausted range stays faithful.
                let done = range.is_empty();
                let (start, end) = range.into_inner();
                __RangeInclusiveIter { start, end, done }
            }
        }

        impl<T> ::core::iter::Iterator for __RangeInclusiveIter<T>
        where
            T: ::core::clone::Clone + ::core::cmp::PartialOrd,
            ::core::ops::RangeInclusive<T>: ::core::iter::Iterator<Item = T>,
        {
            type Item = T;

            fn next(&mut self) -> ::core::option::Option<T> {
                if self.done {
                    return ::core::option::Option::None;
                }
                let mut range = self.start.clone()..=self.end.clone();
                let item = ::core::iter::Iterator::next(&mut range);
                if item.is_none() || range.is_empty() {
                    self.done = true;
                }
                // `next` advanced the range's start; carry it over so
                // the next call resumes where std's iterator would.
                (self.start, _) = range.into_inner();
                item
            }
        }
    }
}

/// Builds `resume()`'s body: the pre-`__drive` state check, dispatched on
/// whether the coroutine ever suspends (`suspension.has_yields`).
fn build_resume_dispatch(cx: &ExpandCtx, suspension: &Suspension) -> TokenStream {
    // resume() permits only suspension variants; internal variants are
    // reachable only through forged states (serde etc.).
    let s_idents: Vec<&syn::Ident> = cx
        .analysis
        .variants
        .iter()
        .filter(|v| cx.cfg.blocks[v.block].resume_point)
        .map(|v| &v.ident)
        .collect();
    let has_internal = (0..cx.cfg.blocks.len())
        .any(|b| b != cx.cfg.entry && !cx.cfg.blocks[b].inline && !cx.cfg.blocks[b].resume_point);
    let invalid_arm = has_internal.then(|| quote!(_ => ::core::panic!("Invalid state"),));
    let poisoned_arm = &suspension.poisoned_arm;

    if !suspension.has_yields {
        // Every resume is an error; the diverging match is the whole
        // body so that no unreachable `__drive` call follows it.
        quote! {
            match self {
                State::Start { .. } => ::core::panic!("Not started"),
                State::Done => ::core::panic!("Already done"),
                #invalid_arm
            }
        }
    } else {
        let fp_bind = cx.fp_enabled().then(|| quote!(__fp,));
        let fp_guard = cx.fp_guard;
        quote! {
            match self {
                #(State::#s_idents { #fp_bind .. } => { #fp_guard })*
                State::Start { .. } => ::core::panic!("Not started"),
                State::Done => ::core::panic!("Already done"),
                #poisoned_arm
                #invalid_arm
            }
            self.__drive(::core::option::Option::Some(_resume))
        }
    }
}

/// Builds `status()`'s body: a match on the discriminant only, dispatched
/// on whether the coroutine ever suspends (`suspension.has_yields`), same
/// as `build_resume_dispatch`.
fn build_status_dispatch(cx: &ExpandCtx, suspension: &Suspension) -> TokenStream {
    // Mirrors build_resume_dispatch's `s_idents`/`has_internal`: resume
    // points map to `Suspended`, and internal-transition variants (only
    // reachable through forged states, same as `resume`'s "Invalid
    // state") map to `Poisoned` as the closest fit among the four
    // statuses.
    let s_idents: Vec<&syn::Ident> = cx
        .analysis
        .variants
        .iter()
        .filter(|v| cx.cfg.blocks[v.block].resume_point)
        .map(|v| &v.ident)
        .collect();
    let has_internal = (0..cx.cfg.blocks.len())
        .any(|b| b != cx.cfg.entry && !cx.cfg.blocks[b].inline && !cx.cfg.blocks[b].resume_point);
    let invalid_arm = has_internal.then(|| quote!(_ => ::diapause::CoroutineStatus::Poisoned,));

    if !suspension.has_yields {
        quote! {
            match self {
                State::Start { .. } => ::diapause::CoroutineStatus::NotStarted,
                State::Done => ::diapause::CoroutineStatus::Done,
                #invalid_arm
            }
        }
    } else {
        quote! {
            match self {
                #(State::#s_idents { .. } => ::diapause::CoroutineStatus::Suspended,)*
                State::Start { .. } => ::diapause::CoroutineStatus::NotStarted,
                State::Done => ::diapause::CoroutineStatus::Done,
                State::Poisoned => ::diapause::CoroutineStatus::Poisoned,
                #invalid_arm
            }
        }
    }
}

/// Builds the whole `fn __drive` definition: `Codegen` assembly, one
/// dispatch arm per variant, and the placeholder-swap loop.
fn build_drive_fn(cx: &ExpandCtx, placeholder: &TokenStream) -> TokenStream {
    let mut resume_bindings: BTreeMap<BlockId, &ResumeBinding> = BTreeMap::new();
    for block in &cx.cfg.blocks {
        if let Terminator::Yield {
            resume_binding: Some(rb),
            next,
            ..
        } = &block.terminator
        {
            resume_bindings.insert(*next, rb);
        }
    }

    let arg_pat = cx.arg_pat;
    let codegen = Codegen {
        cfg: cx.cfg,
        analysis: cx.analysis,
        resume_bindings,
        yield_ty: cx.yield_ty,
        ret_ty: cx.ret_ty,
        start_pattern: quote!(State::Start { #(#arg_pat,)* .. }),
        placeholder: placeholder.clone(),
        fp_pat: cx.fp_enabled().then(|| quote!(__fp: _,)),
        fp_init: cx.fp_enabled().then(|| quote!(__fp: Self::FINGERPRINT,)),
    };
    let drive_arms: Vec<TokenStream> = cx
        .analysis
        .variants
        .iter()
        .map(|v| codegen.arm(v))
        .collect();
    // In-place resume: eligible suspension variants run on `self`
    // directly, before the placeholder swap (see
    // `analyze_cfg::InPlacePlan`). Every path in a fast arm returns,
    // so falling through to the swap means no fast arm matched.
    let fast_arms: Vec<TokenStream> = cx
        .analysis
        .in_place
        .iter()
        .map(|p| codegen.fast_arm(p))
        .collect();
    let fast_dispatch = (!fast_arms.is_empty()).then(|| {
        // `non_snake_case`: `__self_{name}` of a synthetic field
        // (`__iter0`, `__dg0`) has interior consecutive underscores.
        // The standard arms below re-compile the same user code, so
        // genuine user naming warnings still surface there.
        quote! {
            #[allow(non_snake_case)]
            match *self {
                #(#fast_arms)*
                _ => {}
            }
        }
    });
    let resume_ty = cx.resume_ty;
    let yield_ty = cx.yield_ty;
    let ret_ty = cx.ret_ty;

    quote! {
        /// Runs the state machine until the next suspension point
        /// or completion. Not visible outside the module.
        /// (`path_statements`: a hoisted trailing yield in discard
        /// position leaves a bare `__tmpN;` behind. `unused_labels`:
        /// a body without internal transitions never continues
        /// `'__dispatch`. `unused_assignments`: splitting a variable
        /// across state arms lets rustc see per-arm dead stores that
        /// are live on another path of the original body.
        /// `unused_parens`: the in-place arms rewrite stored
        /// variables into parenthesized dereferences, which rustc
        /// flags when one lands in an already-delimited position.)
        #[allow(
            unused_mut,
            unreachable_code,
            path_statements,
            unused_labels,
            unused_assignments,
            unused_parens
        )]
        fn __drive(
            &mut self,
            mut __resume: ::core::option::Option<#resume_ty>,
        ) -> ::diapause::CoroutineState<#yield_ty, #ret_ty> {
            #fast_dispatch
            // Fields must be moved out through &mut self, so the
            // state is swapped for a placeholder up front; a
            // panic in user code leaves it behind.
            let mut __state = ::core::mem::replace(self, #placeholder);
            // Every arm diverges: internal transitions assign the
            // next state and `continue '__dispatch` (a statement,
            // so it also works from inside opaque statements);
            // suspension and completion `return`.
            '__dispatch: loop {
                match __state {
                    #(#drive_arms)*
                    // Unreachable: start/resume checked the state.
                    _ => ::core::panic!("Poisoned"),
                }
            }
        }
    }
}

/// Builds the `diapause::Fingerprinted` impl (only when the
/// `fingerprint` flag was given): `check_fingerprint` is the graceful
/// counterpart of the panicking guard in `start`/`resume`.
fn build_fingerprinted_impl(cx: &ExpandCtx, fingerprint: u64) -> TokenStream {
    let data_variants = cx
        .analysis
        .variants
        .iter()
        .filter(|v| v.block != cx.cfg.entry)
        .map(|v| &v.ident);
    let (impl_generics, ty_generics, where_clause) = cx.generics.split_for_impl();
    quote! {
        impl #impl_generics ::diapause::Fingerprinted for State #ty_generics
        #where_clause
        {
            const FINGERPRINT: u64 = #fingerprint;

            fn check_fingerprint(
                &self,
            ) -> ::core::result::Result<(), ::diapause::FingerprintMismatch> {
                let found = match self {
                    State::Start { __fp, .. } => *__fp,
                    #(State::#data_variants { __fp, .. } => *__fp,)*
                    _ => return ::core::result::Result::Ok(()),
                };
                if found == <Self as ::diapause::Fingerprinted>::FINGERPRINT {
                    ::core::result::Result::Ok(())
                } else {
                    ::core::result::Result::Err(::diapause::FingerprintMismatch {
                        expected: <Self as ::diapause::Fingerprinted>::FINGERPRINT,
                        found,
                    })
                }
            }
        }
    }
}

// === Dispatch-arm generation ===

struct Codegen<'a> {
    cfg: &'a Cfg,
    analysis: &'a Analysis,
    resume_bindings: BTreeMap<BlockId, &'a ResumeBinding>,
    yield_ty: &'a syn::Type,
    ret_ty: &'a syn::Type,
    start_pattern: TokenStream,
    /// The placeholder swapped into `*self` (`State::Poisoned`, or
    /// `State::Done` when the body never yields).
    placeholder: TokenStream,
    /// `__fp: _,` in variant unpack patterns when fingerprinting.
    fp_pat: Option<TokenStream>,
    /// `__fp: Self::FINGERPRINT,` in constructed next-state values.
    fp_init: Option<TokenStream>,
}

impl Codegen<'_> {
    /// One dispatch arm for a variant: unpack it, rebind the resume value,
    /// then run the block.
    fn arm(&self, v: &Variant) -> TokenStream {
        let pattern = if v.block == self.cfg.entry {
            self.start_pattern.clone()
        } else {
            let ident = &v.ident;
            let pats = v.fields.iter().map(|f| bind_pat(&f.mutability, &f.ident));
            let fp_pat = &self.fp_pat;
            quote!(State::#ident { #(#pats,)* #fp_pat })
        };
        let resume_stmt = self.resume_stmt(v.block);
        let body = self.block_code(v.block);
        quote!(#pattern => { #resume_stmt #body })
    }

    /// The `let` re-binding the resume value at the head of a
    /// resume-point arm. A resume-point variant's only predecessor is
    /// its yield, so the take() runs exactly once per __drive call and
    /// cannot fail.
    fn resume_stmt(&self, b: BlockId) -> Option<TokenStream> {
        self.resume_bindings.get(&b).map(|rb| {
            let mutability = &rb.mutability;
            let ident = &self.cfg.bindings[rb.binding.0].ident;
            let ty = rb.ty.as_ref().map(|ty| quote!(: #ty));
            quote! {
                let #mutability #ident #ty =
                    __resume.take().expect("BUG: resume value already consumed");
            }
        })
    }

    /// A block's statements and transition, with crossed borrows
    /// re-established first, removed original borrow `let`s omitted, and
    /// jump markers left by lowering replaced with their transitions.
    /// `b` may be inline (no reborrows of its own) or a variant block.
    fn block_code(&self, b: BlockId) -> TokenStream {
        let reborrows = self
            .analysis
            .variant(b)
            .map_or(&[][..], |v| &v.reborrows)
            .iter()
            .map(|rb| {
                let target_mut = &rb.target_mut;
                let target = &rb.target;
                let source = &rb.source;
                let mut_tok = rb.mutable.then(|| quote!(mut));
                quote!(let #target_mut #target = & #mut_tok #source;)
            });
        let has_jumps = !self.cfg.blocks[b].jumps.is_empty();
        let stmts = self.cfg.blocks[b]
            .stmts
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.analysis.removed_stmts[b].contains(i))
            .map(|(_, stmt)| {
                if !has_jumps {
                    return quote!(#stmt);
                }
                let mut stmt = stmt.clone();
                JumpMarkerReplacer { codegen: self }.visit_stmt_mut(&mut stmt);
                quote!(#stmt)
            });
        let term = self.terminator_code(b);
        quote!(#(#reborrows)* #(#stmts)* #term)
    }

    /// The transition out of a block: every path diverges, either by
    /// assigning the next state and continuing the dispatch loop or by
    /// a suspension/completion `return`.
    fn terminator_code(&self, b: BlockId) -> TokenStream {
        match &self.cfg.blocks[b].terminator {
            Terminator::Goto(t) => self.edge(*t),
            Terminator::Branch { cond, then_, else_ } => {
                let t = self.edge(*then_);
                let e = self.edge(*else_);
                quote!(if #cond { #t } else { #e })
            }
            Terminator::Match { scrutinee, arms } => {
                let arms = arms.iter().map(|arm| {
                    let pat = &arm.pat;
                    let guard = arm.guard.as_ref().map(|g| quote!(if #g));
                    let body = self.edge(arm.body);
                    quote!(#pat #guard => { #body })
                });
                quote!(match #scrutinee { #(#arms)* })
            }
            Terminator::Yield { value, next, .. } => {
                let yield_ty = self.yield_ty;
                let next_state = self.state_value(*next);
                // The yield value is evaluated before live variables are
                // moved into the state, matching the original order.
                quote! {
                    let __yielded: #yield_ty = #value;
                    *self = #next_state;
                    return ::diapause::CoroutineState::Yielded(__yielded);
                }
            }
            Terminator::Return(e) => {
                let ret_ty = self.ret_ty;
                // The completion value is bound before the state is
                // marked `Done` so that a panic in it still leaves the
                // placeholder behind. That puts the user's expression in
                // value position, where a deliberately diverging one
                // (`unreachable!()` after a yield that is never resumed,
                // a `-> !` helper) trips
                // `clippy::diverging_sub_expression` on the user's own
                // span and fails `-D warnings` builds -- even though the
                // same expression is fine as the tail of a plain `fn`.
                // The allow is scoped to this binding, so the lint still
                // fires on sub-expressions elsewhere in the body.
                quote! {
                    #[allow(clippy::diverging_sub_expression)]
                    let __ret: #ret_ty = #e;
                    *self = State::Done;
                    return ::diapause::CoroutineState::Complete(__ret);
                }
            }
            // A bare diverging expression: assigning it to `__ret` like
            // a `Return` would trip `clippy::diverging_sub_expression`
            // on the user's spans.
            Terminator::Unreachable(e) => quote!(#e),
            Terminator::IterNext {
                iter,
                pat,
                body,
                exit,
            } => {
                // The iterator was moved out of the state by the variant
                // unpack (or defined by the preheader `let`); it moves
                // back into the next state wherever it is still live.
                let some_edge = self.edge(*body);
                let none_edge = self.edge(*exit);
                quote! {
                    match ::core::iter::Iterator::next(&mut #iter) {
                        ::core::option::Option::Some(#pat) => { #some_edge }
                        ::core::option::Option::None => { #none_edge }
                    }
                }
            }
        }
    }

    /// A transition edge: inline blocks are embedded in the predecessor's
    /// arm, variant blocks become a state assignment that re-enters the
    /// dispatch loop.
    fn edge(&self, b: BlockId) -> TokenStream {
        if self.cfg.blocks[b].inline {
            let code = self.block_code(b);
            quote!({ #code })
        } else {
            let next_state = self.state_value(b);
            quote!({
                __state = #next_state;
                continue '__dispatch;
            })
        }
    }

    fn state_value(&self, b: BlockId) -> TokenStream {
        // `edge` only calls this for non-inline targets, and `Yield`'s
        // `next` is always a resume point (never inline).
        let v = self
            .analysis
            .variant(b)
            .expect("BUG: state_value called for an inline block");
        let ident = &v.ident;
        let fields = v.fields.iter().map(|f| &f.ident);
        let fp_init = &self.fp_init;
        quote!(State::#ident { #(#fields,)* #fp_init })
    }

    // === In-place resume arms ===
    //
    // One arm per `analyze_cfg::InPlacePlan`, matched on `*self` before
    // the placeholder swap: the variant's fields are bound `ref mut` as
    // `__self_{name}` and uses of the stored names are rewritten to
    // dereferences, so a resume that suspends at the same variant again
    // touches only what the user code touches — no whole-enum moves.
    // Edges into the plan's cold blocks (completion paths) first move
    // the state back out (`rehydrate`) and continue with the standard
    // by-value code. The trade-off is panic behavior: a panic inside
    // the hot part of a fast arm leaves the partially updated suspended
    // variant instead of `Poisoned`.

    /// One in-place dispatch arm for a plan's resume-point variant.
    fn fast_arm(&self, plan: &InPlacePlan) -> TokenStream {
        let v = self
            .analysis
            .variant(plan.block)
            .expect("BUG: in-place plan for a block without a variant");
        let ident = &v.ident;
        let pats = v.fields.iter().map(|f| {
            let name = &f.ident;
            let sf = self_field_ident(name);
            quote!(#name: ref mut #sf)
        });
        let resume_stmt = self.resume_stmt(plan.block);
        let mut path = FastPath::default();
        let body = self.fast_block_code(plan, plan.block, &mut path);
        quote!(State::#ident { #(#pats,)* .. } => { #resume_stmt #body })
    }

    /// The stored names currently reached through their `ref mut`
    /// bindings: the plan's fields minus those re-bound to plain locals
    /// on the current path.
    fn active_names(&self, plan: &InPlacePlan, path: &FastPath) -> BTreeSet<String> {
        plan.fields
            .difference(&path.rebound)
            .map(|id| self.cfg.bindings[id.0].ident.to_string())
            .collect()
    }

    fn rewrite_expr(&self, e: &syn::Expr, plan: &InPlacePlan, path: &FastPath) -> syn::Expr {
        let active = self.active_names(plan, path);
        let mut e = e.clone();
        InPlaceRewriter { active: &active }.visit_expr_mut(&mut e);
        e
    }

    /// A hot region block's statements and transition in in-place mode.
    /// `path` accumulates what this path has done so far: the stored
    /// bindings re-bound to plain locals (`for`-head patterns,
    /// re-binding `let`s; their uses stay un-rewritten and the yield
    /// writes them back) and the variant blocks whose reborrows were
    /// established (rebuilt from locals again after a rehydration).
    fn fast_block_code(&self, plan: &InPlacePlan, b: BlockId, path: &mut FastPath) -> TokenStream {
        debug_assert!(
            plan.hot.contains(&b),
            "BUG: fast codegen left the hot region"
        );
        debug_assert!(
            self.cfg.blocks[b].jumps.is_empty(),
            "BUG: opaque jump in a fast region"
        );
        let mut out = TokenStream::new();
        // Reborrows, like `block_code`, but a stored source is reached
        // through its `ref mut` binding.
        let active = self.active_names(plan, path);
        let reborrows = self.analysis.variant(b).map_or(&[][..], |v| &v.reborrows);
        if !reborrows.is_empty() {
            path.reborrow_blocks.push(b);
        }
        for rb in reborrows {
            let target_mut = &rb.target_mut;
            let target = &rb.target;
            let source = &rb.source;
            let mut_tok = rb.mutable.then(|| quote!(mut));
            let src = if rb.source_is_local && active.contains(&source.to_string()) {
                let sf = self_field_ident(source);
                quote!((*#sf))
            } else {
                quote!(#source)
            };
            // `unused_variables`: a reborrow used only by this block's
            // cold parts is shadowed by the rehydration's rebuild and
            // never read here.
            out.extend(quote! {
                #[allow(unused_variables)]
                let #target_mut #target = & #mut_tok #src;
            });
        }
        for (i, stmt) in self.cfg.blocks[b].stmts.iter().enumerate() {
            if !self.analysis.removed_stmts[b].contains(&i) {
                let active = self.active_names(plan, path);
                let mut stmt = stmt.clone();
                InPlaceRewriter { active: &active }.visit_stmt_mut(&mut stmt);
                out.extend(quote!(#stmt));
            }
            // A stored binding re-bound by this `let` is a plain local
            // from here on (the `let`'s own initializer still saw the
            // dereference); the yield writes the local back.
            for &id in &self.cfg.blocks[b].defs {
                if plan.fields.contains(&id) && self.cfg.bindings[id.0].def_stmt == Some(i) {
                    path.rebound.insert(id);
                }
            }
        }
        out.extend(self.fast_terminator_code(plan, b, path));
        out
    }

    /// The transition out of a hot region block in in-place mode.
    /// Branch paths clone `path`: every path diverges (returns), so no
    /// two paths' re-bindings can meet.
    fn fast_terminator_code(
        &self,
        plan: &InPlacePlan,
        b: BlockId,
        path: &mut FastPath,
    ) -> TokenStream {
        match &self.cfg.blocks[b].terminator {
            Terminator::Goto(t) => self.fast_edge(plan, *t, path),
            Terminator::Branch { cond, then_, else_ } => {
                let cond = self.rewrite_expr(cond, plan, path);
                let t = self.fast_edge(plan, *then_, &mut path.clone());
                let e = self.fast_edge(plan, *else_, &mut path.clone());
                quote!(if #cond { #t } else { #e })
            }
            Terminator::Match { scrutinee, arms } => {
                let scrutinee = self.rewrite_expr(scrutinee, plan, path);
                let arms = arms.iter().map(|arm| {
                    let pat = &arm.pat;
                    let guard = arm.guard.as_ref().map(|g| {
                        let g = self.rewrite_expr(g, plan, path);
                        quote!(if #g)
                    });
                    let body = self.fast_edge(plan, arm.body, &mut path.clone());
                    quote!(#pat #guard => { #body })
                });
                quote!(match #scrutinee { #(#arms)* })
            }
            Terminator::Yield { value, next, .. } => {
                debug_assert_eq!(*next, plan.block, "BUG: in-place region yields elsewhere");
                let yield_ty = self.yield_ty;
                let value = self.rewrite_expr(value, plan, path);
                // Stored bindings re-bound on this path live in plain
                // locals; move them back into their fields. Everything
                // else was updated in place, so the enum (tag included)
                // needs no other write.
                let writebacks = path.rebound.iter().map(|id| {
                    let name = &self.cfg.bindings[id.0].ident;
                    let sf = self_field_ident(name);
                    quote!(*#sf = #name;)
                });
                quote! {
                    let __yielded: #yield_ty = #value;
                    #(#writebacks)*
                    return ::diapause::CoroutineState::Yielded(__yielded);
                }
            }
            // A hot block always reaches a yield through its successors.
            Terminator::Return(_) | Terminator::Unreachable(_) => {
                unreachable!("BUG: completion terminator in a hot in-place block")
            }
            Terminator::IterNext {
                iter,
                pat,
                body,
                exit,
            } => {
                let active = self.active_names(plan, path);
                let iter_expr = if active.contains(&iter.to_string()) {
                    let sf = self_field_ident(iter);
                    quote!(&mut *#sf)
                } else {
                    quote!(&mut #iter)
                };
                // The head pattern may re-bind stored bindings (a loop
                // variable that is used across the yield).
                let mut body_path = path.clone();
                let mut c = PatBindingCollector::default();
                c.visit_pat(pat);
                let pat_names: BTreeSet<String> =
                    c.bindings.into_iter().map(|(i, _)| i.to_string()).collect();
                for &id in &self.cfg.blocks[*body].defs {
                    if plan.fields.contains(&id)
                        && pat_names.contains(&self.cfg.bindings[id.0].ident.to_string())
                    {
                        body_path.rebound.insert(id);
                    }
                }
                let some_edge = self.fast_edge(plan, *body, &mut body_path);
                let none_edge = self.fast_edge(plan, *exit, &mut path.clone());
                quote! {
                    match ::core::iter::Iterator::next(#iter_expr) {
                        ::core::option::Option::Some(#pat) => { #some_edge }
                        ::core::option::Option::None => { #none_edge }
                    }
                }
            }
        }
    }

    /// A region-internal edge. A hot target is inlined in in-place mode
    /// (the region is a tree, so exactly once); a cold target is where
    /// the fast path ends: the state is moved back out and the cold
    /// subtree — all standard-inline blocks — is emitted by the
    /// standard by-value codegen.
    fn fast_edge(&self, plan: &InPlacePlan, t: BlockId, path: &mut FastPath) -> TokenStream {
        if plan.hot.contains(&t) {
            let code = self.fast_block_code(plan, t, path);
            quote!({ #code })
        } else {
            let rehydrate = self.rehydrate(plan, path);
            let code = self.block_code(t);
            quote!({ #rehydrate #code })
        }
    }

    /// Moves the state back out at a hot→cold edge: swaps in the
    /// placeholder (restoring the poison-on-panic behavior for what
    /// follows) and re-binds the fields as the by-value locals the
    /// standard codegen would have. Fields already re-bound to plain
    /// locals stay locals — binding them would shadow the fresh value
    /// with the stale one — and their stale field values are dropped by
    /// the `..`. Reborrows established on the fast path point into the
    /// moved-out state; they are rebuilt from the fresh locals so cold
    /// code sees valid references.
    fn rehydrate(&self, plan: &InPlacePlan, path: &FastPath) -> TokenStream {
        let v = self
            .analysis
            .variant(plan.block)
            .expect("BUG: in-place plan for a block without a variant");
        let rebound_names: BTreeSet<String> = path
            .rebound
            .iter()
            .map(|id| self.cfg.bindings[id.0].ident.to_string())
            .collect();
        let ident = &v.ident;
        let pats = v
            .fields
            .iter()
            .filter(|f| !rebound_names.contains(&f.ident.to_string()))
            .map(|f| bind_pat(&f.mutability, &f.ident));
        let placeholder = &self.placeholder;
        let reborrows = path
            .reborrow_blocks
            .iter()
            .flat_map(|&b| self.analysis.variant(b).map_or(&[][..], |v| &v.reborrows))
            .map(|rb| {
                let target_mut = &rb.target_mut;
                let target = &rb.target;
                let source = &rb.source;
                let mut_tok = rb.mutable.then(|| quote!(mut));
                quote! {
                    #[allow(unused_variables)]
                    let #target_mut #target = & #mut_tok #source;
                }
            });
        quote! {
            #[allow(unused_variables)]
            let State::#ident { #(#pats,)* .. } = ::core::mem::replace(self, #placeholder)
            else {
                ::core::unreachable!()
            };
            #(#reborrows)*
        }
    }
}

/// Per-path state of the in-place codegen walk (see
/// `Codegen::fast_block_code`).
#[derive(Clone, Default)]
struct FastPath {
    /// Stored bindings re-bound to plain locals on this path.
    rebound: BTreeSet<BindingId>,
    /// Variant blocks whose reborrows this path has established.
    reborrow_blocks: Vec<BlockId>,
}

/// The `ref mut` binding holding the stored `ident` in an in-place arm.
fn self_field_ident(ident: &syn::Ident) -> syn::Ident {
    format_ident!("__self_{}", ident.unraw(), span = ident.span())
}

/// Rewrites uses of the in-place stored bindings into dereferences of
/// their `ref mut` pattern bindings: `x` becomes `(*__self_x)`. Flat by
/// construction: the analysis rejected every plan in which a stored
/// name is shadowed inside a statement, so each occurrence of an active
/// name refers to the stored binding. Patterns bind rather than read
/// and are skipped; nested items are separate scopes; closures are
/// visited (they capture the coroutine's locals).
struct InPlaceRewriter<'a> {
    active: &'a BTreeSet<String>,
}

impl InPlaceRewriter<'_> {
    /// The ident of `e` when it is a bare use of an active stored name.
    fn rewrite_target(&self, e: &syn::Expr) -> Option<syn::Ident> {
        let syn::Expr::Path(p) = e else { return None };
        if !p.attrs.is_empty()
            || p.qself.is_some()
            || p.path.leading_colon.is_some()
            || p.path.segments.len() != 1
            || !p.path.segments[0].arguments.is_none()
        {
            return None;
        }
        let ident = &p.path.segments[0].ident;
        self.active
            .contains(&ident.to_string())
            .then(|| ident.clone())
    }
}

impl VisitMut for InPlaceRewriter<'_> {
    fn visit_expr_mut(&mut self, e: &mut syn::Expr) {
        if let Some(ident) = self.rewrite_target(e) {
            let sf = self_field_ident(&ident);
            *e = syn::parse_quote_spanned!(ident.span() => (*#sf));
            return;
        }
        syn::visit_mut::visit_expr_mut(self, e);
    }

    /// `S { x }` shorthand: add the `:` so the member name survives the
    /// value's rewrite into a dereference.
    fn visit_field_value_mut(&mut self, fv: &mut syn::FieldValue) {
        if fv.colon_token.is_none() && self.rewrite_target(&fv.expr).is_some() {
            fv.colon_token = Some(Default::default());
        }
        syn::visit_mut::visit_field_value_mut(self, fv);
    }

    fn visit_pat_mut(&mut self, _: &mut syn::Pat) {}
    fn visit_item_mut(&mut self, _: &mut syn::Item) {}
}

/// Replaces the `__diapause_jump!` markers lowering left for
/// `break`/`continue` escaping an opaque statement (see
/// `cfg::OpaqueJump`) with their real transitions: a next-state
/// assignment re-entering the dispatch loop, or a completion for a
/// valued `break` out of a tail-position loop.
struct JumpMarkerReplacer<'a, 'b> {
    codegen: &'a Codegen<'b>,
}

/// The body of a `__diapause_jump!(k [, value])` marker.
struct MarkerArgs {
    k: usize,
    value: Option<syn::Expr>,
}

impl syn::parse::Parse for MarkerArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let k: syn::LitInt = input.parse()?;
        let value = if input.peek(syn::Token![,]) {
            input.parse::<syn::Token![,]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(MarkerArgs {
            k: k.base10_parse()?,
            value,
        })
    }
}

impl VisitMut for JumpMarkerReplacer<'_, '_> {
    fn visit_expr_mut(&mut self, e: &mut syn::Expr) {
        syn::visit_mut::visit_expr_mut(self, e);
        let syn::Expr::Macro(m) = e else { return };
        // A foreign macro sharing a jump-carrying block with a marker
        // (e.g. an `assert!` beside a rewritten `break`) passes through.
        if !lower::is_jump_marker(&m.mac) {
            return;
        }
        let span = m.mac.span();
        let args: MarkerArgs = m
            .mac
            .parse_body()
            .expect("BUG: malformed __diapause_jump! marker");
        match self.codegen.cfg.opaque_jumps[args.k].kind {
            OpaqueJumpKind::Goto { target, .. } => {
                let next_state = self.codegen.state_value(target);
                *e = syn::parse_quote_spanned!(span => {
                    __state = #next_state;
                    continue '__dispatch;
                });
            }
            OpaqueJumpKind::Complete => {
                let mut value = args.value.expect("BUG: completion marker without a value");
                // The value traveled as macro tokens, so markers nested
                // inside it have not been visited yet.
                self.visit_expr_mut(&mut value);
                let ret_ty = self.codegen.ret_ty;
                *e = syn::parse_quote_spanned!(span => {
                    let __ret: #ret_ty = #value;
                    *self = State::Done;
                    return ::diapause::CoroutineState::Complete(__ret);
                });
            }
        }
    }

    // Markers never occur inside these (the lowering rewriter skips
    // them); leave any user macro coincidentally named like ours alone.
    skip_nested_scopes!(VisitMut);
}

/// Field shorthand with the original binding mode: `mut x` rebinds the
/// stored variable mutably when the state is unpacked.
fn bind_pat(mutability: &Option<syn::Token![mut]>, ident: &syn::Ident) -> TokenStream {
    quote!(#mutability #ident)
}

/// Whether `ty` is syntactically the unit type `()`, i.e. an empty tuple.
///
/// Used to decide whether to emit the `IntoIterator` impl, which only
/// makes sense for `resume = ()` coroutines. The check is purely
/// syntactic (the macro has no type information), which covers both the
/// explicit `resume = ()` and the omitted-and-defaulted case, since the
/// default is parsed into the same empty-tuple type.
fn is_unit_ty(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Tuple(t) if t.elems.is_empty())
}

// === Source fingerprint ===

/// Hashes the coroutine's source — attribute arguments, signature, and
/// body — as seen by the macro, before any rewriting. Token-based, so
/// comments and formatting do not affect it, but any edit to the tokens
/// (including the attribute arguments) changes the value.
///
/// Stringification via `TokenStream::to_string` is stable in practice
/// but not formally guaranteed across rustc/proc-macro2 versions; if it
/// ever shifts, a recompile of unchanged source would change the
/// fingerprint (documented in the README).
fn source_fingerprint(attr: &TokenStream, sig: &syn::Signature, body: &syn::Block) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for part in [
        attr.to_string(),
        quote!(#sig).to_string(),
        quote!(#body).to_string(),
    ] {
        hash = fnv1a(hash, part.as_bytes());
        // Separator so part boundaries cannot alias; `\0` never occurs
        // in token stringification.
        hash = fnv1a(hash, b"\0");
    }
    hash
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// 64-bit FNV-1a. Hand-rolled instead of `std::hash::DefaultHasher`
/// because that algorithm is unspecified and may change between Rust
/// versions, which would change fingerprints of unchanged source.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

// === Delegation `?` desugaring ===

/// Desugars a `?` applied directly to a delegation macro
/// (`yield_all!(sub)?` / `yield_all_resume!(sub, rv)?`, with or without
/// the `box` modifier) in the statement positions the macros support: a
/// whole `let` initializer, an expression statement, and a block's
/// trailing expression. The completion value is bound to a synthetic
/// `__dt{k}` temporary and the `?` re-applied to that:
///
/// ```text
/// let v: T = yield_all!(sub)?;  // let __dt0 = yield_all!(sub);
///                               // let v: T = __dt0?;
/// yield_all!(sub)?;             // let __dt1 = yield_all!(sub);
///                               // let _ = __dt1?;
/// yield_all!(sub)?              // let __dt2 = yield_all!(sub);
///                               // __dt2?
/// ```
///
/// Runs before the other body rewrites: `rewrite_early_exits` then
/// turns the plain `?` on the temporary into a completion transition
/// like any other, and lowering sees a native delegation `let` whose
/// destination type follows the operand (`TySource::DelegateReturn`),
/// which is what lets the temporary cross the join after the delegation
/// loop without an annotation. The discarding statement form binds the
/// rewritten `?` to `_` so a non-`()` Ok value typechecks. A `?` on a
/// delegation in any other position is left alone for lowering to
/// reject with the usual position error.
fn rewrite_delegate_try(body: &mut syn::Block) {
    /// The delegation-macro expression under a `?`'s operand, seen
    /// through parentheses and macro-expansion groups.
    fn delegate_macro(e: &syn::Expr) -> Option<&syn::ExprMacro> {
        match e {
            syn::Expr::Paren(p) => delegate_macro(&p.expr),
            syn::Expr::Group(g) => delegate_macro(&g.expr),
            syn::Expr::Macro(em) if lower::is_delegate_macro(&em.mac) => Some(em),
            _ => None,
        }
    }

    struct Rewriter {
        /// Number of `__dt{k}` temporaries created so far (per body).
        count: usize,
    }

    impl Rewriter {
        /// Desugars one statement, returning the `let __dt{k} = ..;` to
        /// insert in front of it.
        fn rewrite_stmt(&mut self, stmt: &mut syn::Stmt) -> Option<syn::Stmt> {
            let try_expr = match stmt {
                // A `let ... else` initializer is not a supported
                // delegation position; leave it for lowering to reject.
                syn::Stmt::Local(local) => {
                    let init = local.init.as_mut().filter(|i| i.diverge.is_none())?;
                    match &mut *init.expr {
                        syn::Expr::Try(t) => t,
                        _ => return None,
                    }
                }
                syn::Stmt::Expr(syn::Expr::Try(t), _) => t,
                _ => return None,
            };
            let mac = delegate_macro(&try_expr.expr)?.clone();
            let span = mac.mac.span();
            let ident = syn::Ident::new(&format!("__dt{}", self.count), span);
            self.count += 1;
            *try_expr.expr = syn::parse_quote_spanned!(span => #ident);
            // An expression statement discards the Ok value: bind the
            // rewritten `?` to `_` (a bare match statement — which the
            // `?` becomes — would demand a `()` value instead).
            if let syn::Stmt::Expr(e, Some(_)) = stmt {
                let e = std::mem::replace(e, syn::Expr::Verbatim(TokenStream::new()));
                *stmt = syn::parse_quote_spanned!(span => let _ = #e;);
            }
            Some(syn::parse_quote_spanned!(span => let #ident = #mac;))
        }
    }

    impl VisitMut for Rewriter {
        fn visit_block_mut(&mut self, block: &mut syn::Block) {
            let mut out = Vec::with_capacity(block.stmts.len());
            for mut stmt in std::mem::take(&mut block.stmts) {
                out.extend(self.rewrite_stmt(&mut stmt));
                syn::visit_mut::visit_stmt_mut(self, &mut stmt);
                out.push(stmt);
            }
            block.stmts = out;
        }
        skip_nested_scopes!(VisitMut);
    }
    Rewriter { count: 0 }.visit_block_mut(body);
}

// === Early-exit rewriting (`?`) ===

/// Rewrites `e?` anywhere in the body (inside opaque statements and
/// `yield_!` value expressions too; closures, async blocks, and nested
/// items are separate scopes and excluded) into a `Try::branch` match
/// whose Break arm is a completion transition. Valid at any expression
/// position; the exit value is evaluated before the state write so a
/// panic inside it (or inside a `From` conversion) still poisons.
///
/// `return`s are left in place — statement- and expression-position
/// alike. Lowering terminates statement-position ones (a statement, a
/// match-arm body, a branch or function tail) with a CFG `Return` (no
/// false fall-through edge), rewrites those inside opaque statements
/// into completion jump markers, and `rewrite_leftover_returns`
/// finalizes any that survive in embedded expressions; every form
/// generates the same value-before-state-write transition as the `?`
/// exit here. Rewriting expression-position returns here instead used
/// to interpolate the value tokens into a fresh `parse_quote!`, which
/// re-parsed a nested `?`'s verbatim exit back into an `Expr::Return`
/// node; the opaque rewriter then wrapped that synthesized exit in a
/// second completion, an E0308 in the generated code.
fn rewrite_early_exits(body: &mut syn::Block, ret_ty: &syn::Type) {
    /// The completion transition `{ let __ret: T = value; *self =
    /// State::Done; return Complete(__ret); }`. The `return` is emitted
    /// as a verbatim statement — not an `Expr::Return` node — so later
    /// passes can tell synthesized early exits apart from the user's own
    /// `return`s (lowering's opaque-statement rewriter turns the latter
    /// into completion jump markers and must leave these alone). The
    /// printed tokens are identical either way. `value` is interpolated
    /// into a `parse_quote!` and so must not itself carry a synthesized
    /// exit — the re-parse would strip that verbatim marking; the only
    /// caller passes freshly built `from_residual` tokens.
    fn completion_transition(
        ret_ty: &syn::Type,
        value: TokenStream,
        span: proc_macro2::Span,
    ) -> syn::Expr {
        let mut block: syn::Block = syn::parse_quote_spanned!(span => {
            let __ret: #ret_ty = #value;
            *self = State::Done;
        });
        block.stmts.push(syn::Stmt::Expr(
            syn::Expr::Verbatim(quote::quote_spanned!(span =>
                return ::diapause::CoroutineState::Complete(__ret)
            )),
            Some(syn::Token![;](span)),
        ));
        syn::Expr::Block(syn::ExprBlock {
            attrs: Vec::new(),
            label: None,
            block,
        })
    }

    struct Rewriter<'a> {
        ret_ty: &'a syn::Type,
    }
    impl VisitMut for Rewriter<'_> {
        fn visit_expr_mut(&mut self, e: &mut syn::Expr) {
            syn::visit_mut::visit_expr_mut(self, e);
            let ret_ty = self.ret_ty;
            if let syn::Expr::Try(t) = e {
                // Spanning the generated tokens to the `?` makes a
                // trait-bound error (unsupported operand type, or a
                // Result/Option mismatch) point at the offending `?`
                // instead of the whole attribute.
                let span = t.question_token.span();
                let exit = completion_transition(
                    ret_ty,
                    quote::quote_spanned!(span =>
                        ::diapause::FromResidual::from_residual(__r)
                    ),
                    span,
                );
                // The exit arm and the operand are grafted in as AST
                // nodes rather than interpolated: `parse_quote!`
                // would re-parse the verbatim `return`s inside them
                // (the exit's own, or a nested `?`'s) into
                // `Expr::Return` nodes, losing the synthesized-exit
                // marking.
                let mut em: syn::ExprMatch = syn::parse_quote_spanned!(span =>
                    match ::diapause::Try::branch(__operand) {
                        ::core::ops::ControlFlow::Continue(__v) => __v,
                        ::core::ops::ControlFlow::Break(__r) => __exit,
                    }
                );
                let syn::Expr::Call(branch) = &mut *em.expr else {
                    unreachable!("BUG: the scrutinee is a call by construction")
                };
                branch.args[0] = (*t.expr).clone();
                *em.arms[1].body = exit;
                *e = syn::Expr::Match(em);
            }
        }

        // syn does not parse macro token streams, so a `yield_!` value
        // expression must be rewritten explicitly; foreign macro tokens
        // stay untouched (as with `?` outside coroutines, `?` inside
        // them is not visible to us).
        fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
            if !lower::is_yield_macro(mac) || mac.tokens.is_empty() {
                return;
            }
            if let Ok(mut value) = mac.parse_body::<syn::Expr>() {
                self.visit_expr_mut(&mut value);
                mac.tokens = quote!(#value);
            }
        }
        skip_nested_scopes!(VisitMut);
    }
    Rewriter { ret_ty }.visit_block_mut(body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn rewritten(mut block: syn::Block) -> String {
        let ret_ty: syn::Type = parse_quote!(Result<u32, E>);
        rewrite_early_exits(&mut block, &ret_ty);
        quote!(#block).to_string()
    }

    #[test]
    fn try_becomes_a_branch_match() {
        let out = rewritten(parse_quote!({
            let x = f()?;
        }));
        assert!(out.contains(":: diapause :: Try :: branch (f ())"));
        assert!(out.contains(":: diapause :: FromResidual :: from_residual"));
        assert!(out.contains("* self = State :: Done ;"));
        assert!(!out.contains('?'));
    }

    #[test]
    fn nested_try_operands_are_rewritten() {
        let out = rewritten(parse_quote!({
            g(f()?)?;
        }));
        assert!(!out.contains('?'));
        assert_eq!(out.matches("Try :: branch").count(), 2);
    }

    #[test]
    fn try_inside_yield_macro_tokens_is_rewritten() {
        let out = rewritten(parse_quote!({
            let x = yield_!(f()?);
        }));
        assert!(out.contains("yield_ ! (match :: diapause :: Try :: branch (f ())"));
    }

    #[test]
    fn statement_return_is_left_for_lowering() {
        // Statement-position returns become CFG terminators in lowering;
        // the rewriter must leave them alone (at any block depth).
        let out = rewritten(parse_quote!({
            if c {
                return f();
            }
            return;
        }));
        assert!(out.contains("return f () ;"), "{out}");
        assert!(out.contains("return ;"), "{out}");
        assert!(!out.contains("State :: Done"), "{out}");
    }

    #[test]
    fn try_inside_a_statement_return_value_is_rewritten() {
        let out = rewritten(parse_quote!({
            return f()?;
        }));
        assert!(
            out.starts_with("{ return match :: diapause :: Try :: branch"),
            "{out}"
        );
        // Only the `?`'s exit transition is synthesized; the statement
        // return itself stays.
        assert_eq!(out.matches("State :: Done").count(), 1, "{out}");
    }

    #[test]
    fn expression_position_returns_are_left_for_lowering() {
        let out = rewritten(parse_quote!({
            let x = f().unwrap_or_else(|| return g());
        }));
        // The closure is a separate scope; its return stays.
        assert!(out.contains("return g ()"), "{out}");
        let out = rewritten(parse_quote!({
            let x: u32 = if c { return 1 } else { 2 };
            h(x);
        }));
        // An arm-tail return is statement position (a block statement);
        // it stays for lowering.
        assert!(out.contains("return 1"), "{out}");
        assert!(!out.contains("State :: Done"), "{out}");
        // An unbraced match-arm return and a return embedded in a
        // larger expression stay too: lowering terminates the former
        // (or the opaque rewriter marks it), and the CFG-finalization
        // pass rewrites whatever survives embedded.
        let out = rewritten(parse_quote!({
            match s {
                A(v) => return Ok(v),
                B => f(return 2),
            }
        }));
        assert!(out.contains("return Ok (v)"), "{out}");
        assert!(out.contains("f (return 2)"), "{out}");
        assert!(!out.contains("State :: Done"), "{out}");
    }

    /// Regression: a `?` inside an expression-position `return`'s value
    /// (the conteff driver shape, `=> return Ok(result?)`). Rewriting
    /// the return here used to re-parse the `?`'s verbatim exit into an
    /// `Expr::Return` node, which the opaque rewriter wrapped in a
    /// second completion — an E0308 in the generated code. The return
    /// must survive untouched with exactly the `?`'s one exit inside.
    #[test]
    fn try_inside_an_arm_return_value_is_rewritten_once() {
        let out = rewritten(parse_quote!({
            match step {
                Complete(result) => return Ok(result?),
                Yielded(y) => {
                    let _r = yield_!(y);
                }
            }
        }));
        assert!(
            out.contains("return Ok (match :: diapause :: Try :: branch"),
            "{out}"
        );
        // Exactly one completion: the `?`'s own exit. The doubled
        // rewrite added a second `State :: Done` / `Complete` pair.
        assert_eq!(out.matches("State :: Done").count(), 1, "{out}");
        assert_eq!(
            out.matches(":: diapause :: CoroutineState :: Complete")
                .count(),
            1,
            "{out}"
        );
    }

    #[test]
    fn synthesized_exits_never_parse_as_return_nodes() {
        // The opaque-statement rewriter in lowering rewrites every
        // `Expr::Return` it sees into a completion jump marker; the
        // exits synthesized here must therefore never round-trip into
        // `Expr::Return` nodes, even when nested inside another `?`'s
        // operand.
        struct FindReturn {
            found: bool,
        }
        impl<'ast> syn::visit::Visit<'ast> for FindReturn {
            fn visit_expr_return(&mut self, _: &'ast syn::ExprReturn) {
                self.found = true;
            }
        }
        let ret_ty: syn::Type = parse_quote!(Result<u32, E>);
        let mut block: syn::Block = parse_quote!({
            let x = g(f()?)?;
            let y = h()?;
        });
        rewrite_early_exits(&mut block, &ret_ty);
        let mut f = FindReturn { found: false };
        syn::visit::visit_block(&mut f, &block);
        assert!(!f.found, "synthesized exits must stay verbatim");
    }

    fn delegate_rewritten(mut block: syn::Block) -> String {
        rewrite_delegate_try(&mut block);
        quote!(#block).to_string()
    }

    fn assert_delegate_rewrites(input: syn::Block, expected: syn::Block) {
        assert_eq!(delegate_rewritten(input), quote!(#expected).to_string());
    }

    fn assert_delegate_unchanged(block: syn::Block) {
        let before = quote!(#block).to_string();
        assert_eq!(delegate_rewritten(block), before);
    }

    #[test]
    fn delegate_try_in_a_let_initializer_binds_a_temporary() {
        assert_delegate_rewrites(
            parse_quote!({
                let v: u32 = yield_all!(sub)?;
            }),
            parse_quote!({
                let __dt0 = yield_all!(sub);
                let v: u32 = __dt0?;
            }),
        );
    }

    #[test]
    fn delegate_try_statement_discards_through_a_wildcard_let() {
        assert_delegate_rewrites(
            parse_quote!({
                yield_all!(sub)?;
                f();
            }),
            parse_quote!({
                let __dt0 = yield_all!(sub);
                let _ = __dt0?;
                f();
            }),
        );
    }

    #[test]
    fn delegate_try_tail_stays_a_tail() {
        assert_delegate_rewrites(
            parse_quote!({ yield_all!(sub)? }),
            parse_quote!({
                let __dt0 = yield_all!(sub);
                __dt0?
            }),
        );
    }

    #[test]
    fn boxed_and_resume_delegate_try_forms_desugar_too() {
        assert_delegate_rewrites(
            parse_quote!({
                let v: u32 = yield_all!(box sub)?;
                yield_all_resume!(sub2, rv)?;
            }),
            parse_quote!({
                let __dt0 = yield_all!(box sub);
                let v: u32 = __dt0?;
                let __dt1 = yield_all_resume!(sub2, rv);
                let _ = __dt1?;
            }),
        );
    }

    #[test]
    fn parenthesized_delegate_try_operand_desugars_bare() {
        assert_delegate_rewrites(
            parse_quote!({
                let v: u32 = (yield_all!(sub))?;
            }),
            parse_quote!({
                let __dt0 = yield_all!(sub);
                let v: u32 = __dt0?;
            }),
        );
    }

    #[test]
    fn nested_block_delegate_try_desugars_recursively() {
        assert_delegate_rewrites(
            parse_quote!({
                if c {
                    yield_all!(sub)?;
                }
            }),
            parse_quote!({
                if c {
                    let __dt0 = yield_all!(sub);
                    let _ = __dt0?;
                }
            }),
        );
    }

    #[test]
    fn expression_position_delegate_try_is_untouched() {
        // Unsupported positions keep the macro in place for lowering's
        // position error.
        assert_delegate_unchanged(parse_quote!({
            f(yield_all!(sub)?);
        }));
        assert_delegate_unchanged(parse_quote!({
            let v: u32 = 1 + yield_all!(sub)?;
        }));
    }

    #[test]
    fn plain_try_and_plain_delegation_are_untouched() {
        assert_delegate_unchanged(parse_quote!({
            let x = f()?;
            let v: u32 = yield_all!(sub);
            yield_all!(sub);
        }));
    }

    #[test]
    fn let_else_delegate_try_initializer_is_untouched() {
        assert_delegate_unchanged(parse_quote!({
            let Some(x) = yield_all!(sub)? else {
                return;
            };
        }));
    }

    #[test]
    fn closure_bodies_are_untouched_by_delegate_try() {
        assert_delegate_unchanged(parse_quote!({
            let f = || yield_all!(sub)?;
        }));
    }

    #[test]
    fn fnv1a_matches_known_vectors() {
        // Reference values for FNV-1a 64: hashing must never change, or
        // recompiled fingerprints would stop matching persisted states.
        assert_eq!(fnv1a(FNV_OFFSET_BASIS, b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(FNV_OFFSET_BASIS, b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(FNV_OFFSET_BASIS, b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn fingerprint_ignores_formatting_but_not_edits() {
        let fp = |attr: TokenStream, f: syn::ItemFn| source_fingerprint(&attr, &f.sig, &f.block);
        let base = fp(
            quote!(yield = u32),
            parse_quote! {
                fn c(n: u32) {
                    yield_!(n);
                }
            },
        );
        // Comments and whitespace are invisible at the token level.
        let reformatted = fp(
            quote!(yield = u32),
            parse_quote! {
                fn c(n: u32) {
                    // a comment
                    yield_!(n) ;
                }
            },
        );
        assert_eq!(base, reformatted);
        // Attribute arguments and body edits both change the hash.
        let edited_attr = fp(
            quote!(yield = u64),
            parse_quote! {
                fn c(n: u32) {
                    yield_!(n);
                }
            },
        );
        let edited_body = fp(
            quote!(yield = u32),
            parse_quote! {
                fn c(n: u32) {
                    yield_!(n + 1);
                }
            },
        );
        assert_ne!(base, edited_attr);
        assert_ne!(base, edited_body);
    }

    #[test]
    fn expand_debug_success_returns_every_stage() {
        let item: syn::ItemFn = parse_quote! {
            fn c(n: u32) -> u32 {
                let mut i = 0u32;
                while i < n {
                    yield_!(i);
                    i += 1;
                }
                i
            }
        };
        let dbg = expand_debug(quote!(yield = u32), item.clone());
        let tokens = dbg.result.expect("expansion should succeed");
        assert_eq!(
            tokens.to_string(),
            expand(quote!(yield = u32), item).unwrap().to_string(),
            "expand_debug must generate exactly what expand does"
        );
        let pre = dbg.cfg_unsimplified.expect("pre-simplification CFG");
        let post = dbg.cfg.expect("simplified CFG");
        // Simplification only merges and removes blocks (never adds),
        // and inlining is decided by it, so the snapshot has none.
        assert!(pre.blocks.len() >= post.blocks.len());
        assert!(pre.blocks.iter().all(|b| !b.inline));
        let analysis = dbg.analysis.expect("analysis");
        assert!(analysis.variants.iter().any(|v| v.ident == "S1"));
    }

    #[test]
    fn reborrow_of_non_local_name_crossing_yield_does_not_panic() {
        // A direct borrow of a name that is not a local binding (a
        // `static`/`const`, or an undefined name rustc will diagnose)
        // is rebuilt by name after resume. The IR self-check must not
        // treat its source as a missing field and panic; it should
        // hand the tokens to rustc, which then reports any bad name.
        // Regression: found by the `expand` fuzz target.
        let item: syn::ItemFn = parse_quote! {
            fn c() -> u32 {
                let w: &u32 = &UNDEFINED_GLOBAL;
                let r = yield_!(1u32);
                (*w).wrapping_add(r)
            }
        };
        let out = expand(quote!(yield = u32, resume = u32), item)
            .expect("a non-local reborrow must expand, not error");
        // The rebuilt borrow references the name verbatim for rustc.
        assert!(out.to_string().contains("UNDEFINED_GLOBAL"));
    }

    #[test]
    fn let_with_yield_in_pattern_errors_without_panic() {
        // `let yield_!(a);` parses as a `let` with a macro pattern and
        // no initializer: the yield is in the binding, not an
        // initializer. Lowering must report a diagnostic rather than
        // panic on the missing initializer. Regression: found by the
        // `expand` fuzz target.
        let item: syn::ItemFn =
            syn::parse_str("fn c(a: u32) {\n    let yield_!(a);\n}").expect("input parses as a fn");
        let err = expand(quote!(yield = u32), item)
            .expect_err("a yield in a let pattern must be a diagnostic");
        assert!(err.to_string().contains("not supported in this position"));
    }

    #[test]
    fn expand_debug_parse_error_has_no_artifacts() {
        let dbg = expand_debug(
            quote!(banana = i32),
            parse_quote!(
                fn c() {}
            ),
        );
        assert!(dbg.result.is_err());
        assert!(dbg.cfg_unsimplified.is_none());
        assert!(dbg.cfg.is_none());
        assert!(dbg.analysis.is_none());
    }

    #[test]
    fn expand_debug_lower_error_has_no_cfg() {
        // `yield_!` in a `while` condition is rejected by lowering.
        let item: syn::ItemFn = parse_quote! {
            fn c() {
                while yield_!(1u32) {
                    f();
                }
            }
        };
        let dbg = expand_debug(quote!(yield = u32, resume = bool), item);
        assert!(dbg.result.is_err());
        assert!(dbg.cfg_unsimplified.is_none());
        assert!(dbg.cfg.is_none());
        assert!(dbg.analysis.is_none());
    }

    #[test]
    fn expand_debug_analyze_error_keeps_both_cfgs() {
        // `v` has no syntactic type and crosses a yield: lowering
        // succeeds, the analysis rejects it.
        let item: syn::ItemFn = parse_quote! {
            fn c() {
                let v = compute();
                yield_!(());
                drop(v);
            }
        };
        let dbg = expand_debug(quote!(), item);
        assert!(dbg.result.is_err());
        assert!(dbg.cfg_unsimplified.is_some());
        assert!(dbg.cfg.is_some());
        assert!(dbg.analysis.is_none());
    }

    #[test]
    fn in_place_arm_generated_for_a_simple_loop() {
        let item: syn::ItemFn = parse_quote! {
            fn c(n: u64) {
                for i in 0..n {
                    yield_!(i);
                }
            }
        };
        let out = expand(quote!(yield = u64), item).unwrap().to_string();
        // The fast arm binds the stored iterator by `ref mut` and never
        // writes the enum on the yield-back path.
        assert!(out.contains("ref mut __self___iter0"), "got: {out}");
    }

    #[test]
    fn in_place_false_disables_fast_arms() {
        let item: syn::ItemFn = parse_quote! {
            fn c(n: u64) {
                for i in 0..n {
                    yield_!(i);
                }
            }
        };
        let out = expand(quote!(yield = u64, in_place = false), item)
            .unwrap()
            .to_string();
        assert!(!out.contains("__self_"), "got: {out}");
    }

    #[test]
    fn alternating_yields_have_no_in_place_arm() {
        let item: syn::ItemFn = parse_quote! {
            fn c() {
                loop {
                    yield_!(1u32);
                    yield_!(2u32);
                }
            }
        };
        let out = expand(quote!(yield = u32), item).unwrap().to_string();
        assert!(!out.contains("__self_"), "got: {out}");
    }

    #[test]
    fn in_place_rewrites_stored_uses_and_writes_rebounds_back() {
        // running_total shape: `sum` is updated through the reference,
        // `i` (re-bound by the `for` head, used after resume) is written
        // back at the yield.
        let item: syn::ItemFn = parse_quote! {
            fn c(n: u64) -> u64 {
                let mut sum: u64 = 0;
                for i in 0..n {
                    let bonus = yield_!(sum);
                    sum = sum.wrapping_add(i).wrapping_add(bonus);
                }
                sum
            }
        };
        let out = expand(quote!(yield = u64, resume = u64), item)
            .unwrap()
            .to_string();
        assert!(out.contains("(* __self_sum)"), "got: {out}");
        assert!(out.contains("* __self_i = i ;"), "got: {out}");
        // The completion path moves the state back out before the
        // standard by-value epilogue.
        assert!(
            out.contains("# [allow (unused_variables)] let State :: S1"),
            "got: {out}"
        );
    }

    #[test]
    fn closures_and_nested_items_are_untouched() {
        let out = rewritten(parse_quote!({
            let c = |v: Option<u32>| Some(v? + 1);
            fn helper(v: Option<u32>) -> Option<u32> {
                Some(v? + 1)
            }
            foreign!(f()?);
        }));
        assert!(!out.contains("Try"));
        assert_eq!(out.matches('?').count(), 3);
    }
}
