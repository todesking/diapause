//! Code generation: turns the lowered CFG and its analysis into the
//! state enum, the `Coroutine` impl, and the `__drive` dispatch loop.

use std::collections::{BTreeMap, HashSet};

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::analyze_cfg::{self, Analysis, ArgInfo, Variant};
use crate::args::MacroArgs;
use crate::cfg::{BlockId, Cfg, ResumeBinding, Terminator};
use crate::lower::{self, skip_nested_scopes, ErrorSink};

/// A function argument. Carries the `ident` needed to emit code, unlike
/// `analyze_cfg::ArgInfo`, which drops it because that crate identifies
/// arguments by `BindingId` instead. A non-simple-identifier pattern is
/// replaced by a fresh `__argN` ident (which is what the state stores)
/// and kept in `pattern`; the body then destructures it via a
/// synthesized `let <pattern> = __argN;` at the entry block.
struct ArgVar {
    ident: syn::Ident,
    mutability: Option<syn::Token![mut]>,
    ty: syn::Type,
    /// The original pattern, when it is not a simple identifier.
    pattern: Option<syn::Pat>,
}

impl From<&ArgVar> for ArgInfo {
    fn from(arg: &ArgVar) -> Self {
        ArgInfo {
            mutability: arg.mutability,
            ty: arg.ty.clone(),
        }
    }
}

pub fn expand(attr: TokenStream, item: syn::ItemFn) -> syn::Result<TokenStream> {
    // Hashed before the body is rewritten, from the exact source tokens.
    let fingerprint = source_fingerprint(&attr, &item.sig, &item.block);
    let macro_args: MacroArgs = syn::parse2(attr)?;

    check_signature(&item.sig)?;
    let mut args = parse_args(&item.sig)?;
    let generics = augment_generics(&item.sig, &mut args)?;

    let ret_ty: syn::Type = match &item.sig.output {
        syn::ReturnType::Default => syn::parse_quote!(()),
        syn::ReturnType::Type(_, ty) => (**ty).clone(),
    };
    check_return_type(&ret_ty)?;

    // Early `return e` and `e?` become completion transitions everywhere
    // in the body, including inside opaque statements; the rewritten form
    // only makes sense inside `__drive`, where `self` and `State` resolve.
    let mut body = (*item.block).clone();
    rewrite_early_exits(&mut body, &ret_ty);

    let arg_idents: Vec<syn::Ident> = args.iter().map(|a| a.ident.clone()).collect();
    let arg_pats: Vec<(syn::Pat, syn::Ident)> = args
        .iter()
        .filter_map(|a| a.pattern.clone().map(|p| (p, a.ident.clone())))
        .collect();
    let cfg = lower::lower(&arg_idents, &arg_pats, &body)?;
    let arg_infos: Vec<ArgInfo> = args.iter().map(ArgInfo::from).collect();
    let analysis = analyze_cfg::analyze(&cfg, &arg_infos, &macro_args.resume_ty)?;

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
    let fp_guard = macro_args.fingerprint.then(|| {
        let msg = format!("this state was created by a different version of `{name}`");
        quote! {
            if *__fp != Self::FINGERPRINT {
                ::core::panic!(#msg);
            }
        }
    });
    let fp_bind = macro_args.fingerprint.then(|| quote!(__fp,));
    let fp_init = macro_args.fingerprint.then(|| quote!(__fp: #fingerprint,));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let cx = ExpandCtx {
        generics: &generics,
        where_clause,
        cfg: &cfg,
        analysis: &analysis,
        arg_ident: &arg_ident,
        arg_ty: &arg_ty,
        arg_pat: &arg_pat,
        yield_ty,
        ret_ty: &ret_ty,
        resume_ty,
        fp_guard: fp_guard.as_ref(),
    };

    let StateEnum {
        tokens: state_enum_tokens,
        phantom_init,
    } = build_state_enum(&cx, &derive_attrs, &suspension.poisoned_variant);
    let resume_body = build_resume_dispatch(&cx, &suspension);
    let drive_fn = build_drive_fn(&cx, &suspension.placeholder);
    let fp_check_fn = macro_args.fingerprint.then(|| build_check_fingerprint(&cx));

    Ok(quote! {
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

            impl #impl_generics ::baregen::Coroutine<#resume_ty> for State #ty_generics
            #where_clause
            {
                type Yield = #yield_ty;
                type Return = #ret_ty;

                fn start(&mut self) -> ::baregen::CoroutineState<#yield_ty, #ret_ty> {
                    match self {
                        State::Start { #fp_bind .. } => { #fp_guard }
                        _ => ::core::panic!("Already started"),
                    }
                    self.__drive(::core::option::Option::None)
                }

                fn resume(
                    &mut self,
                    _resume: #resume_ty,
                ) -> ::baregen::CoroutineState<#yield_ty, #ret_ty> {
                    #resume_body
                }
            }

            impl #impl_generics State #ty_generics #where_clause {
                /// Fingerprint of the coroutine source this state type was
                /// generated from: an FNV-1a hash of the attribute
                /// arguments, signature, and body tokens. Editing the
                /// coroutine changes it; comments and formatting do not.
                pub const FINGERPRINT: u64 = #fingerprint;

                #fp_check_fn

                #drive_fn
            }
        }
    })
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
    StateEnum { tokens, phantom_init }
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
        fp_pat: cx.fp_enabled().then(|| quote!(__fp: _,)),
        fp_init: cx.fp_enabled().then(|| quote!(__fp: Self::FINGERPRINT,)),
    };
    let drive_arms: Vec<TokenStream> = cx
        .analysis
        .variants
        .iter()
        .map(|v| codegen.arm(v))
        .collect();
    let resume_ty = cx.resume_ty;
    let yield_ty = cx.yield_ty;
    let ret_ty = cx.ret_ty;

    quote! {
        /// Runs the state machine until the next suspension point
        /// or completion. Not visible outside the module.
        #[allow(unused_mut, unreachable_code)]
        fn __drive(
            &mut self,
            mut __resume: ::core::option::Option<#resume_ty>,
        ) -> ::baregen::CoroutineState<#yield_ty, #ret_ty> {
            // Fields must be moved out through &mut self, so the
            // state is swapped for a placeholder up front; a
            // panic in user code leaves it behind.
            let mut __state = ::core::mem::replace(self, #placeholder);
            loop {
                __state = match __state {
                    #(#drive_arms)*
                    // Unreachable: start/resume checked the state.
                    _ => ::core::panic!("Poisoned"),
                };
            }
        }
    }
}

/// Builds the inherent `check_fingerprint` method (only when the
/// `fingerprint` flag was given): the graceful counterpart of the
/// panicking guard in `start`/`resume`.
fn build_check_fingerprint(cx: &ExpandCtx) -> TokenStream {
    let data_variants = cx
        .analysis
        .variants
        .iter()
        .filter(|v| v.block != cx.cfg.entry)
        .map(|v| &v.ident);
    quote! {
        /// Checks that this state was created by the same coroutine
        /// source (see [`Self::FINGERPRINT`]). Call it right after
        /// deserializing to detect a mismatch gracefully;
        /// `start`/`resume` panic on the same condition. Terminal
        /// states (`Done`, `Poisoned`) carry no fingerprint and
        /// always pass.
        pub fn check_fingerprint(
            &self,
        ) -> ::core::result::Result<(), ::baregen::FingerprintMismatch> {
            let found = match self {
                State::Start { __fp, .. } => *__fp,
                #(State::#data_variants { __fp, .. } => *__fp,)*
                _ => return ::core::result::Result::Ok(()),
            };
            if found == Self::FINGERPRINT {
                ::core::result::Result::Ok(())
            } else {
                ::core::result::Result::Err(::baregen::FingerprintMismatch {
                    expected: Self::FINGERPRINT,
                    found,
                })
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
        // A resume-point variant's only predecessor is its yield, so the
        // take() runs exactly once per __drive call and cannot fail.
        let resume_stmt = self.resume_bindings.get(&v.block).map(|rb| {
            let mutability = &rb.mutability;
            let ident = &self.cfg.bindings[rb.binding.0].ident;
            let ty = rb.ty.as_ref().map(|ty| quote!(: #ty));
            quote! {
                let #mutability #ident #ty =
                    __resume.take().expect("BUG: resume value already consumed");
            }
        });
        let body = self.block_code(v.block);
        quote!(#pattern => { #resume_stmt #body })
    }

    /// A block's statements and transition, with crossed borrows
    /// re-established first and removed original borrow `let`s omitted.
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
        let stmts = self.cfg.blocks[b]
            .stmts
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.analysis.removed_stmts[b].contains(i))
            .map(|(_, stmt)| stmt);
        let term = self.terminator_code(b);
        quote!(#(#reborrows)* #(#stmts)* #term)
    }

    /// The transition out of a block: an expression producing the next
    /// state value, or a diverging suspension/completion.
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
                    return ::baregen::CoroutineState::Yielded(__yielded);
                }
            }
            Terminator::Return(e) => {
                let ret_ty = self.ret_ty;
                quote! {
                    let __ret: #ret_ty = #e;
                    *self = State::Done;
                    return ::baregen::CoroutineState::Complete(__ret);
                }
            }
            Terminator::IterNext {
                iter, pat, body, exit,
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
    /// arm, variant blocks become the next state value.
    fn edge(&self, b: BlockId) -> TokenStream {
        if self.cfg.blocks[b].inline {
            let code = self.block_code(b);
            quote!({ #code })
        } else {
            self.state_value(b)
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
}

/// Field shorthand with the original binding mode: `mut x` rebinds the
/// stored variable mutably when the state is unpacked.
fn bind_pat(mutability: &Option<syn::Token![mut]>, ident: &syn::Ident) -> TokenStream {
    quote!(#mutability #ident)
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

// === Early-exit rewriting (`return` and `?`) ===

/// Rewrites `return e` and `e?` anywhere in the body (inside opaque
/// statements and `yield_!` value expressions too; closures, async
/// blocks, and nested items are separate scopes and excluded) into
/// completion transitions. Valid at any expression position; the exit
/// value is evaluated before the state write so a panic inside it (or
/// inside a `From` conversion) still poisons.
fn rewrite_early_exits(body: &mut syn::Block, ret_ty: &syn::Type) {
    struct Rewriter<'a> {
        ret_ty: &'a syn::Type,
    }
    impl VisitMut for Rewriter<'_> {
        fn visit_expr_mut(&mut self, e: &mut syn::Expr) {
            syn::visit_mut::visit_expr_mut(self, e);
            let ret_ty = self.ret_ty;
            match e {
                syn::Expr::Return(r) => {
                    let value = r.expr.take().map_or_else(|| quote!(()), |v| quote!(#v));
                    *e = syn::parse_quote!({
                        let __ret: #ret_ty = #value;
                        *self = State::Done;
                        return ::baregen::CoroutineState::Complete(__ret);
                    });
                }
                syn::Expr::Try(t) => {
                    let operand = &t.expr;
                    // Spanning the generated tokens to the `?` makes a
                    // trait-bound error (unsupported operand type, or a
                    // Result/Option mismatch) point at the offending `?`
                    // instead of the whole attribute.
                    let span = t.question_token.span();
                    *e = syn::parse_quote_spanned!(span =>
                        match ::baregen::BareTry::branch(#operand) {
                            ::core::ops::ControlFlow::Continue(__v) => __v,
                            ::core::ops::ControlFlow::Break(__r) => {
                                let __ret: #ret_ty =
                                    ::baregen::BareFromResidual::from_residual(__r);
                                *self = State::Done;
                                return ::baregen::CoroutineState::Complete(__ret);
                            }
                        }
                    );
                }
                _ => {}
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

// === Signature handling ===

fn check_signature(sig: &syn::Signature) -> syn::Result<()> {
    let unsupported = |span_source: &dyn quote::ToTokens, what: &str| {
        Err(syn::Error::new_spanned(
            span_source,
            format!("#[baregen::coroutine] does not support {what}"),
        ))
    };
    if let Some(c) = &sig.constness {
        return unsupported(c, "const functions");
    }
    if let Some(a) = &sig.asyncness {
        return unsupported(a, "async functions");
    }
    if let Some(u) = &sig.unsafety {
        return unsupported(u, "unsafe functions");
    }
    if let Some(abi) = &sig.abi {
        return unsupported(abi, "extern functions");
    }
    if let Some(v) = &sig.variadic {
        return unsupported(v, "variadic functions");
    }
    Ok(())
}

/// The return type becomes `type Return` of the impl, where lifetime
/// elision and `impl Trait` are not available.
fn check_return_type(ty: &syn::Type) -> syn::Result<()> {
    struct Check {
        error: ErrorSink,
    }
    impl Check {
        fn record(&mut self, e: syn::Error) {
            self.error.push(e);
        }
    }
    impl<'ast> Visit<'ast> for Check {
        fn visit_type_reference(&mut self, r: &'ast syn::TypeReference) {
            if r.lifetime.is_none() {
                self.record(syn::Error::new(
                    r.span(),
                    "elided lifetimes in the return type are not supported; \
                     use a named lifetime",
                ));
            }
            syn::visit::visit_type_reference(self, r);
        }
        fn visit_lifetime(&mut self, lt: &'ast syn::Lifetime) {
            if lt.ident == "_" {
                self.record(syn::Error::new(
                    lt.span(),
                    "elided lifetimes in the return type are not supported; \
                     use a named lifetime",
                ));
            }
        }
        fn visit_type_impl_trait(&mut self, it: &'ast syn::TypeImplTrait) {
            self.record(syn::Error::new(
                it.span(),
                "`impl Trait` in the return type is not supported",
            ));
        }
    }
    let mut check = Check {
        error: ErrorSink::default(),
    };
    check.visit_type(ty);
    check.error.into_result(())
}

fn parse_args(sig: &syn::Signature) -> syn::Result<Vec<ArgVar>> {
    let simple = |pat: &syn::Pat| match pat {
        syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => Some(pi.clone()),
        _ => None,
    };
    // Names taken by simple-identifier arguments; the fresh `__argN`
    // names must not collide with them (they share the State enum's
    // Start-variant field namespace).
    let taken: HashSet<String> = sig
        .inputs
        .iter()
        .filter_map(|input| match input {
            syn::FnArg::Typed(pt) => simple(&pt.pat).map(|pi| pi.ident.to_string()),
            syn::FnArg::Receiver(_) => None,
        })
        .collect();
    let mut fresh = (0usize..)
        .map(|i| format!("__arg{i}"))
        .filter(|name| !taken.contains(name));
    sig.inputs
        .iter()
        .map(|input| {
            let pat_type = match input {
                syn::FnArg::Receiver(r) => {
                    return Err(syn::Error::new_spanned(
                        r,
                        "#[baregen::coroutine] cannot be applied to methods",
                    ));
                }
                syn::FnArg::Typed(pt) => pt,
            };
            let ty = (*pat_type.ty).clone();
            Ok(match simple(&pat_type.pat) {
                Some(pi) => ArgVar {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty,
                    pattern: None,
                },
                // Any other pattern (destructuring, `_`, `ref`, `@`):
                // the state stores the value under a fresh name and the
                // entry block destructures it.
                None => ArgVar {
                    ident: syn::Ident::new(&fresh.next().unwrap(), pat_type.pat.span()),
                    mutability: None,
                    ty,
                    pattern: Some((*pat_type.pat).clone()),
                },
            })
        })
        .collect()
}

/// Rewrites argument types so they can appear on the State enum: elided
/// lifetimes get fresh named parameters and `impl Trait` becomes a named
/// type parameter. Returns the function's generics augmented with the
/// fresh parameters.
fn augment_generics(sig: &syn::Signature, args: &mut [ArgVar]) -> syn::Result<syn::Generics> {
    let mut generics = sig.generics.clone();
    let mut rewriter = TypeRewriter {
        used_lifetimes: generics
            .lifetimes()
            .map(|l| l.lifetime.ident.to_string())
            .collect(),
        used_type_params: generics
            .type_params()
            .map(|t| t.ident.to_string())
            .collect(),
        fresh_lifetimes: Vec::new(),
        fresh_type_params: Vec::new(),
    };
    for arg in args.iter_mut() {
        rewriter.visit_type_mut(&mut arg.ty);
    }
    // Lifetime parameters must precede type parameters.
    for (i, lt) in rewriter.fresh_lifetimes.into_iter().enumerate() {
        generics
            .params
            .insert(i, syn::GenericParam::Lifetime(syn::LifetimeParam::new(lt)));
    }
    for tp in rewriter.fresh_type_params {
        generics.params.push(syn::GenericParam::Type(tp));
    }
    Ok(generics)
}

struct TypeRewriter {
    used_lifetimes: HashSet<String>,
    used_type_params: HashSet<String>,
    fresh_lifetimes: Vec<syn::Lifetime>,
    fresh_type_params: Vec<syn::TypeParam>,
}

impl TypeRewriter {
    fn fresh_lifetime(&mut self, span: proc_macro2::Span) -> syn::Lifetime {
        let name = ('a'..='z')
            .map(|c| c.to_string())
            .chain((0..).map(|i| format!("lt{i}")))
            .find(|name| !self.used_lifetimes.contains(name))
            .unwrap();
        self.used_lifetimes.insert(name.clone());
        let lt = syn::Lifetime::new(&format!("'{name}"), span);
        self.fresh_lifetimes.push(lt.clone());
        lt
    }
}

impl VisitMut for TypeRewriter {
    fn visit_type_reference_mut(&mut self, r: &mut syn::TypeReference) {
        if r.lifetime.is_none() {
            r.lifetime = Some(self.fresh_lifetime(r.and_token.span()));
        }
        syn::visit_mut::visit_type_reference_mut(self, r);
    }

    fn visit_lifetime_mut(&mut self, lt: &mut syn::Lifetime) {
        if lt.ident == "_" {
            *lt = self.fresh_lifetime(lt.span());
        }
    }

    fn visit_type_mut(&mut self, ty: &mut syn::Type) {
        syn::visit_mut::visit_type_mut(self, ty);
        if let syn::Type::ImplTrait(it) = ty {
            let name = (0..)
                .map(|i| format!("__T{i}"))
                .find(|name| !self.used_type_params.contains(name))
                .unwrap();
            self.used_type_params.insert(name.clone());
            let ident = syn::Ident::new(&name, it.span());
            let bounds = it.bounds.clone();
            self.fresh_type_params
                .push(syn::parse_quote!(#ident: #bounds));
            *ty = syn::parse_quote!(#ident);
        }
    }
}

/// Returns the PhantomData payload type anchoring generic parameters that
/// appear in no variant field, or `None` if every parameter is used.
///
/// Detection is a token-level ident scan of the field types: a false
/// "used" only omits the phantom and surfaces as E0392.
fn phantom_for_unused_params<'a>(
    generics: &syn::Generics,
    field_tys: impl Iterator<Item = &'a syn::Type>,
) -> Option<syn::Type> {
    use quote::ToTokens;

    let mut used = HashSet::new();
    for ty in field_tys {
        lower::collect_token_idents(ty.to_token_stream(), &mut used);
    }

    let unused_types: Vec<&syn::Ident> = generics
        .type_params()
        .map(|t| &t.ident)
        .filter(|id| !used.contains(&id.to_string()))
        .collect();
    let unused_lifetimes: Vec<&syn::Lifetime> = generics
        .lifetimes()
        .map(|l| &l.lifetime)
        .filter(|lt| !used.contains(&lt.ident.to_string()))
        .collect();
    if unused_types.is_empty() && unused_lifetimes.is_empty() {
        return None;
    }
    // fn pointers keep the phantom covariant and unconditionally
    // Send/Sync/Copy/Clone.
    Some(syn::parse_quote!((#(fn() -> #unused_types,)* #(fn() -> &#unused_lifetimes (),)*)))
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
        assert!(out.contains(":: baregen :: BareTry :: branch (f ())"));
        assert!(out.contains(":: baregen :: BareFromResidual :: from_residual"));
        assert!(out.contains("* self = State :: Done ;"));
        assert!(!out.contains('?'));
    }

    #[test]
    fn nested_try_operands_are_rewritten() {
        let out = rewritten(parse_quote!({
            g(f()?)?;
        }));
        assert!(!out.contains('?'));
        assert_eq!(out.matches("BareTry :: branch").count(), 2);
    }

    #[test]
    fn try_inside_yield_macro_tokens_is_rewritten() {
        let out = rewritten(parse_quote!({
            let x = yield_!(f()?);
        }));
        assert!(out.contains("yield_ ! (match :: baregen :: BareTry :: branch (f ())"));
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
    fn closures_and_nested_items_are_untouched() {
        let out = rewritten(parse_quote!({
            let c = |v: Option<u32>| Some(v? + 1);
            fn helper(v: Option<u32>) -> Option<u32> {
                Some(v? + 1)
            }
            foreign!(f()?);
        }));
        assert!(!out.contains("BareTry"));
        assert_eq!(out.matches('?').count(), 3);
    }
}
