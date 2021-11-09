/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Compile function calls.

use gazebo::{coerce::coerce, prelude::*};

use crate::{
    codemap::{Span, Spanned},
    collections::symbol_map::Symbol,
    eval::{
        compiler::{
            scope::{CstArgument, CstExpr},
            Compiler,
        },
        fragment::{def::InlineDefBody, expr::ExprCompiledValue, stmt::OptimizeOnFreezeContext},
        Arguments, FrozenDef,
    },
    gazebo::prelude::SliceExt,
    syntax::ast::{ArgumentP, AstString, ExprP},
    values::{
        function::{FrozenBoundMethod, NativeFunction},
        string::interpolation::parse_format_one,
        FrozenStringValue, FrozenValue, FrozenValueTyped, ValueLike,
    },
};

#[derive(Default, Clone, Debug)]
pub(crate) struct ArgsCompiledValue {
    pub(crate) pos_named: Vec<Spanned<ExprCompiledValue>>,
    /// Named arguments compiled.
    ///
    /// Note names are guaranteed to be unique here because names are validated in AST:
    /// named arguments in [`Expr::Call`] are unique.
    pub(crate) names: Vec<(Symbol, FrozenStringValue)>,
    pub(crate) args: Option<Spanned<ExprCompiledValue>>,
    pub(crate) kwargs: Option<Spanned<ExprCompiledValue>>,
}

#[derive(Clone, Debug)]
pub(crate) enum CallCompiled {
    Call(Box<(Spanned<ExprCompiledValue>, ArgsCompiledValue)>),
    Method(Box<(Spanned<ExprCompiledValue>, Symbol, ArgsCompiledValue)>),
}

impl CallCompiled {
    pub(crate) fn call(
        span: Span,
        fun: ExprCompiledValue,
        args: ArgsCompiledValue,
    ) -> ExprCompiledValue {
        if let (Some(fun), Some(pos)) = (fun.as_value(), args.one_pos()) {
            // Try to inline a function like `lambda x: type(x) == "y"`.
            if let Some(fun) = fun.downcast_ref::<FrozenDef>() {
                if let Some(InlineDefBody::ReturnTypeIs(t)) = &fun.def_info.inline_def_body {
                    return ExprCompiledValue::TypeIs(box pos.clone(), *t);
                }
            }
        }

        ExprCompiledValue::Call(Spanned {
            span,
            node: CallCompiled::Call(box (Spanned { span, node: fun }, args)),
        })
    }
}

impl Spanned<CallCompiled> {
    pub(crate) fn optimize_on_freeze(&self, ctx: &OptimizeOnFreezeContext) -> ExprCompiledValue {
        match self.node {
            CallCompiled::Call(box (ref fun, ref args)) => {
                let fun = fun.optimize_on_freeze(ctx);
                let args = args.optimize_on_freeze(ctx);
                CallCompiled::call(self.span, fun.node, args)
            }
            CallCompiled::Method(box (ref this, ref field, ref args)) => {
                let this = this.optimize_on_freeze(ctx);
                let field = field.clone();
                let args = args.optimize_on_freeze(ctx);
                ExprCompiledValue::Call(Spanned {
                    span: self.span,
                    node: CallCompiled::Method(box (this, field, args)),
                })
            }
        }
    }
}

impl ArgsCompiledValue {
    /// Check if arguments is one positional argument.
    pub(crate) fn one_pos(&self) -> Option<&Spanned<ExprCompiledValue>> {
        let ArgsCompiledValue {
            pos_named,
            names,
            args,
            kwargs,
        } = self;
        match (pos_named.as_slice(), names.as_slice(), args, kwargs) {
            ([pos], [], None, None) => Some(pos),
            _ => None,
        }
    }

    pub(crate) fn pos_only(&self) -> Option<&[Spanned<ExprCompiledValue>]> {
        if self.names.is_empty() && self.args.is_none() && self.kwargs.is_none() {
            Some(&self.pos_named)
        } else {
            None
        }
    }

    fn split_pos_names(&self) -> (&[Spanned<ExprCompiledValue>], &[Spanned<ExprCompiledValue>]) {
        self.pos_named
            .as_slice()
            .split_at(self.pos_named.len() - self.names.len())
    }

    /// Invoke a callback if all arguments are frozen values.
    fn all_values<'v, R>(&self, handler: impl FnOnce(Arguments<'v, '_>) -> R) -> Option<R> {
        let (pos, named) = self.split_pos_names();
        let pos = pos
            .try_map(|e| e.as_value().map(FrozenValue::to_value).ok_or(()))
            .ok()?;
        let named = named
            .try_map(|e| e.as_value().map(FrozenValue::to_value).ok_or(()))
            .ok()?;
        let args = self
            .args
            .as_ref()
            .try_map(|args| args.as_value().map(FrozenValue::to_value).ok_or(()))
            .ok()?;
        let kwargs = self
            .kwargs
            .as_ref()
            .try_map(|kwargs| kwargs.as_value().map(FrozenValue::to_value).ok_or(()))
            .ok()?;
        Some(handler(Arguments {
            pos: &pos,
            named: &named,
            names: coerce(&self.names),
            args,
            kwargs,
        }))
    }

    fn optimize_on_freeze(&self, ctx: &OptimizeOnFreezeContext) -> ArgsCompiledValue {
        let ArgsCompiledValue {
            ref pos_named,
            ref names,
            ref args,
            ref kwargs,
        } = *self;
        ArgsCompiledValue {
            pos_named: pos_named.map(|p| p.optimize_on_freeze(ctx)),
            names: names.clone(),
            args: args.as_ref().map(|a| a.optimize_on_freeze(ctx)),
            kwargs: kwargs.as_ref().map(|a| a.optimize_on_freeze(ctx)),
        }
    }
}

impl Compiler<'_, '_, '_> {
    fn args(&mut self, args: Vec<CstArgument>) -> ArgsCompiledValue {
        let mut res = ArgsCompiledValue::default();
        for x in args {
            match x.node {
                ArgumentP::Positional(x) => res.pos_named.push(self.expr(x)),
                ArgumentP::Named(name, value) => {
                    let fv = self
                        .eval
                        .module_env
                        .frozen_heap()
                        .alloc_string_value(name.node.as_str());
                    res.names.push((Symbol::new(&name.node), fv));
                    res.pos_named.push(self.expr(value));
                }
                ArgumentP::Args(x) => res.args = Some(self.expr(x)),
                ArgumentP::KwArgs(x) => res.kwargs = Some(self.expr(x)),
            }
        }
        res
    }

    fn try_spec_exec(
        &mut self,
        span: Span,
        fun: FrozenValue,
        args: &ArgsCompiledValue,
    ) -> Option<ExprCompiledValue> {
        // Only if all call arguments are frozen values.
        args.all_values(|arguments| {
            let v = fun.to_value().invoke(None, arguments, self.eval).ok()?;
            ExprCompiledValue::try_value(span, v, self.eval.module_env.frozen_heap())
        })?
    }

    fn expr_call_fun_frozen_no_special(
        &mut self,
        span: Span,
        fun: FrozenValue,
        args: Vec<CstArgument>,
    ) -> ExprCompiledValue {
        let args = self.args(args);

        if let Some(fun) = FrozenValueTyped::<NativeFunction>::new(fun) {
            // Try execute the native function speculatively.
            if fun.speculative_exec_safe {
                if let Some(expr) = self.try_spec_exec(span, fun.to_frozen_value(), &args) {
                    return expr;
                }
            }
        }

        if let Some(fun) = FrozenValueTyped::<FrozenBoundMethod>::new(fun) {
            // Try execute the bound method speculatively.
            if fun.method.speculative_exec_safe {
                if let Some(expr) = self.try_spec_exec(span, fun.to_frozen_value(), &args) {
                    return expr;
                }
            }
        }

        CallCompiled::call(span, ExprCompiledValue::Value(fun), args)
    }

    fn expr_call_fun_frozen(
        &mut self,
        span: Span,
        left: FrozenValue,
        mut args: Vec<CstArgument>,
    ) -> ExprCompiledValue {
        let one_positional = args.len() == 1 && args[0].is_positional();
        if left == self.constants.fn_type && one_positional {
            self.fn_type(args.pop().unwrap().node.into_expr())
        } else if left == self.constants.fn_len && one_positional {
            let x = self.expr(args.pop().unwrap().node.into_expr());
            ExprCompiledValue::Len(box x)
        } else {
            self.expr_call_fun_frozen_no_special(span, left, args)
        }
    }

    fn expr_call_fun_compiled(
        &mut self,
        span: Span,
        left: Spanned<ExprCompiledValue>,
        args: Vec<CstArgument>,
    ) -> ExprCompiledValue {
        if let Some(left) = left.as_value() {
            self.expr_call_fun_frozen(span, left, args)
        } else {
            let args = self.args(args);
            ExprCompiledValue::Call(Spanned {
                span,
                node: CallCompiled::Call(box (left, args)),
            })
        }
    }

    fn expr_call_method(
        &mut self,
        span: Span,
        e: CstExpr,
        s: AstString,
        mut args: Vec<CstArgument>,
    ) -> ExprCompiledValue {
        let e = self.expr(e);

        // Optimize `"aaa{}bbb".format(arg)`.
        if let Some(e) = e.as_string() {
            if s.node == "format" && args.len() == 1 {
                if let ArgumentP::Positional(..) = args[0].node {
                    if let Some((before, after)) = parse_format_one(&e) {
                        let before = self
                            .eval
                            .module_env
                            .frozen_heap()
                            .alloc_string_value(&before);
                        let after = self
                            .eval
                            .module_env
                            .frozen_heap()
                            .alloc_string_value(&after);
                        let arg = match args.pop().unwrap().node {
                            ArgumentP::Positional(arg) => arg,
                            _ => unreachable!(),
                        };
                        assert!(args.is_empty());
                        let arg = self.expr(arg);
                        return ExprCompiledValue::FormatOne(box (before, arg, after));
                    }
                }
            }
        }

        let s = Symbol::new(&s.node);
        if let Some(e) = e.as_value() {
            if let Some(v) = ExprCompiledValue::compile_time_getattr(
                e,
                &s,
                self.eval.module_env.heap(),
                self.eval.module_env.frozen_heap(),
            ) {
                return self.expr_call_fun_frozen_no_special(span, v, args);
            }
        }
        let args = self.args(args);
        ExprCompiledValue::Call(Spanned {
            span,
            node: CallCompiled::Method(box (e, s, args)),
        })
    }

    pub(crate) fn expr_call(
        &mut self,
        span: Span,
        left: CstExpr,
        args: Vec<CstArgument>,
    ) -> ExprCompiledValue {
        match left.node {
            ExprP::Dot(box e, s) => self.expr_call_method(span, e, s, args),
            _ => {
                let expr = self.expr(left);
                self.expr_call_fun_compiled(span, expr, args)
            }
        }
    }
}
