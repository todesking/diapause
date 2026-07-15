//! Code generation: turns the lowered CFG and its analysis into the
//! state enum, the `Coroutine` impl, and the `__drive` dispatch loop.

use std::collections::{BTreeMap, HashSet};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::analyze_cfg::{self, Analysis, ArgInfo};
use crate::args::MacroArgs;
use crate::lower::{self, BlockId, Cfg, Terminator};

/// A function argument (simple-identifier pattern only).
struct ArgVar {
    ident: syn::Ident,
    mutability: Option<syn::Token![mut]>,
    ty: syn::Type,
}

pub fn expand(attr: TokenStream, item: syn::ItemFn) -> syn::Result<TokenStream> {
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
    let cfg = lower::lower(&arg_idents, &body)?;
    let arg_infos: Vec<ArgInfo> = args
        .iter()
        .map(|a| ArgInfo {
            mutability: a.mutability,
            ty: a.ty.clone(),
        })
        .collect();
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

    let arg_ident: Vec<_> = args.iter().map(|a| &a.ident).collect();
    let arg_ty: Vec<_> = args.iter().map(|a| &a.ty).collect();

    let variant_idents = variant_idents(&cfg);

    // Variant declarations in BlockId order: deterministic, and linear
    // bodies produce a `Start, S1..Sn` layout.
    let state_variants: Vec<TokenStream> = (0..cfg.blocks.len())
        .filter(|&b| b != cfg.entry && !cfg.blocks[b].inline)
        .map(|b| {
            let ident = variant_idents[b].as_ref().unwrap();
            let field_defs = analysis.variant_fields[b].as_ref().unwrap().iter().map(|f| {
                let ident = &f.ident;
                let ty = &f.ty;
                quote!(#ident: #ty)
            });
            quote!(#ident { #(#field_defs),* })
        })
        .collect();

    // A generic parameter used only inside the body would leave the enum
    // with an unconstrained parameter (E0392); a PhantomData field in
    // Start keeps such parameters anchored.
    let all_field_tys = arg_ty
        .iter()
        .copied()
        .chain(analysis.variant_fields.iter().flatten().flatten().map(|f| &f.ty));
    let phantom_ty = phantom_for_unused_params(&generics, all_field_tys);
    let phantom_field = phantom_ty
        .as_ref()
        .map(|ty| quote!(__phantom: ::core::marker::PhantomData<#ty>,));
    let phantom_init = phantom_ty
        .as_ref()
        .map(|_| quote!(__phantom: ::core::marker::PhantomData,));

    // Without yields the body is a single transition, so no panic can
    // occur between a state write and the return: Done doubles as the
    // placeholder and Poisoned is omitted.
    let n_yields = cfg.blocks.iter().filter(|b| b.resume_point).count();
    let (poisoned_variant, placeholder) = if n_yields == 0 {
        (quote!(), quote!(State::Done))
    } else {
        (quote!(Poisoned,), quote!(State::Poisoned))
    };

    let mut resume_bindings: BTreeMap<BlockId, &lower::ResumeBinding> = BTreeMap::new();
    for block in &cfg.blocks {
        if let Terminator::Yield {
            resume_binding: Some(rb),
            next,
            ..
        } = &block.terminator
        {
            resume_bindings.insert(*next, rb);
        }
    }

    let arg_pat: Vec<_> = args
        .iter()
        .map(|a| bind_pat(&a.mutability, &a.ident))
        .collect();
    let codegen = Codegen {
        cfg: &cfg,
        analysis: &analysis,
        variant_idents: &variant_idents,
        resume_bindings,
        yield_ty,
        ret_ty: &ret_ty,
        start_pattern: quote!(State::Start { #(#arg_pat,)* .. }),
    };
    let drive_arms: Vec<TokenStream> = (0..cfg.blocks.len())
        .filter(|&b| !cfg.blocks[b].inline)
        .map(|b| codegen.arm(b))
        .collect();

    // resume() permits only suspension variants; internal variants are
    // reachable only through forged states (serde etc.).
    let s_idents: Vec<&syn::Ident> = (0..cfg.blocks.len())
        .filter(|&b| cfg.blocks[b].resume_point)
        .map(|b| variant_idents[b].as_ref().unwrap())
        .collect();
    let has_internal = (0..cfg.blocks.len())
        .any(|b| b != cfg.entry && !cfg.blocks[b].inline && !cfg.blocks[b].resume_point);
    let poisoned_arm =
        (n_yields > 0).then(|| quote!(State::Poisoned => ::core::panic!("Poisoned"),));
    let invalid_arm = has_internal.then(|| quote!(_ => ::core::panic!("Invalid state"),));

    let resume_body = if n_yields == 0 {
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
        quote! {
            match self {
                #(State::#s_idents { .. } => {})*
                State::Start { .. } => ::core::panic!("Not started"),
                State::Done => ::core::panic!("Already done"),
                #poisoned_arm
                #invalid_arm
            }
            self.__drive(::core::option::Option::Some(_resume))
        }
    };

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        #(#fn_attrs)*
        #vis fn #name #impl_generics (#(#arg_ident: #arg_ty),*) -> #name::State #ty_generics
        #where_clause
        {
            #name::State::Start { #(#arg_ident,)* #phantom_init }
        }

        #vis mod #name {
            #[allow(unused_imports)]
            use super::*;

            #(#derive_attrs)*
            pub enum State #generics #where_clause {
                Start { #(#arg_ident: #arg_ty,)* #phantom_field },
                #(#state_variants,)*
                Done,
                #poisoned_variant
            }

            impl #impl_generics ::baregen::Coroutine<#resume_ty> for State #ty_generics
            #where_clause
            {
                type Yield = #yield_ty;
                type Return = #ret_ty;

                fn start(&mut self) -> ::baregen::CoroutineState<#yield_ty, #ret_ty> {
                    match self {
                        State::Start { .. } => {}
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
    })
}

// === Dispatch-arm generation ===

struct Codegen<'a> {
    cfg: &'a Cfg,
    analysis: &'a Analysis,
    variant_idents: &'a [Option<syn::Ident>],
    resume_bindings: BTreeMap<BlockId, &'a lower::ResumeBinding>,
    yield_ty: &'a syn::Type,
    ret_ty: &'a syn::Type,
    start_pattern: TokenStream,
}

impl Codegen<'_> {
    /// One dispatch arm for a non-inline block: unpack the variant,
    /// rebind the resume value, then run the block.
    fn arm(&self, b: BlockId) -> TokenStream {
        let pattern = if b == self.cfg.entry {
            self.start_pattern.clone()
        } else {
            let ident = self.variant_idents[b].as_ref().unwrap();
            let pats = self.analysis.variant_fields[b]
                .as_ref()
                .unwrap()
                .iter()
                .map(|f| bind_pat(&f.mutability, &f.ident));
            quote!(State::#ident { #(#pats),* })
        };
        // A resume-point variant's only predecessor is its yield, so the
        // take() runs exactly once per __drive call and cannot fail.
        let resume_stmt = self.resume_bindings.get(&b).map(|rb| {
            let mutability = &rb.mutability;
            let ident = &self.cfg.bindings[rb.binding.0].ident;
            let ty = rb.ty.as_ref().map(|ty| quote!(: #ty));
            quote! {
                let #mutability #ident #ty =
                    __resume.take().expect("BUG: resume value already consumed");
            }
        });
        let body = self.block_code(b);
        quote!(#pattern => { #resume_stmt #body })
    }

    /// A block's statements and transition, with crossed borrows
    /// re-established first and removed original borrow `let`s omitted.
    fn block_code(&self, b: BlockId) -> TokenStream {
        let reborrows = self.analysis.reborrows[b].iter().map(|rb| {
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
        let ident = self.variant_idents[b].as_ref().unwrap();
        let fields = self.analysis.variant_fields[b]
            .as_ref()
            .unwrap()
            .iter()
            .map(|f| &f.ident);
        quote!(State::#ident { #(#fields),* })
    }
}

/// Field shorthand with the original binding mode: `mut x` rebinds the
/// stored variable mutably when the state is unpacked.
fn bind_pat(mutability: &Option<syn::Token![mut]>, ident: &syn::Ident) -> TokenStream {
    quote!(#mutability #ident)
}

// === Variant naming ===

/// Assigns variant names: `Start` for the entry, `S{k}` for resume
/// points (source yield order = block creation order, which block ids
/// preserve), `B{k}` for the remaining variant blocks (reverse
/// postorder). Inline blocks get none. Both numberings are deterministic
/// for a given source, which serde representations rely on.
fn variant_idents(cfg: &Cfg) -> Vec<Option<syn::Ident>> {
    let mut idents = vec![None; cfg.blocks.len()];
    idents[cfg.entry] = Some(format_ident!("Start"));
    let mut s = 0;
    for (b, block) in cfg.blocks.iter().enumerate() {
        if block.resume_point {
            s += 1;
            idents[b] = Some(format_ident!("S{s}"));
        }
    }
    let mut k = 0;
    for b in reverse_postorder(cfg) {
        if idents[b].is_none() && !cfg.blocks[b].inline {
            k += 1;
            idents[b] = Some(format_ident!("B{k}"));
        }
    }
    idents
}

fn reverse_postorder(cfg: &Cfg) -> Vec<BlockId> {
    let mut visited = vec![false; cfg.blocks.len()];
    let mut post = Vec::new();
    // Iterative DFS; `true` marks a node whose successors are done.
    let mut stack = vec![(cfg.entry, false)];
    while let Some((b, expanded)) = stack.pop() {
        if expanded {
            post.push(b);
            continue;
        }
        if std::mem::replace(&mut visited[b], true) {
            continue;
        }
        stack.push((b, true));
        for s in cfg.blocks[b].terminator.successors().into_iter().rev() {
            if !visited[s] {
                stack.push((s, false));
            }
        }
    }
    post.reverse();
    post
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
        fn visit_expr_closure_mut(&mut self, _: &mut syn::ExprClosure) {}
        fn visit_expr_async_mut(&mut self, _: &mut syn::ExprAsync) {}
        fn visit_item_mut(&mut self, _: &mut syn::Item) {}
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
        error: Option<syn::Error>,
    }
    impl Check {
        fn record(&mut self, e: syn::Error) {
            match &mut self.error {
                Some(prev) => prev.combine(e),
                None => self.error = Some(e),
            }
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
    let mut check = Check { error: None };
    check.visit_type(ty);
    match check.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn parse_args(sig: &syn::Signature) -> syn::Result<Vec<ArgVar>> {
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
            match &*pat_type.pat {
                syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => Ok(ArgVar {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty: (*pat_type.ty).clone(),
                }),
                other => Err(syn::Error::new_spanned(
                    other,
                    "argument patterns must be simple identifiers",
                )),
            }
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
        collect_idents(ty.to_token_stream(), &mut used);
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

fn collect_idents(tokens: TokenStream, out: &mut HashSet<String>) {
    for tt in tokens {
        match tt {
            proc_macro2::TokenTree::Ident(id) => {
                out.insert(id.to_string());
            }
            proc_macro2::TokenTree::Group(g) => collect_idents(g.stream(), out),
            _ => {}
        }
    }
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
