//! Transcraber takes a template, like `fn $ident() {}`, a set of bindings like
//! `$ident => foo`, interpolates variables in the template, to get `fn foo() {}`

use ra_syntax::SmolStr;

use crate::{
    mbe_expander::{Binding, Bindings, Fragment},
    parser::{parse_template, Op, RepeatKind, Separator},
    ExpandError,
};

impl Bindings {
    fn contains(&self, name: &str) -> bool {
        self.inner.contains_key(name)
    }

    fn get(&self, name: &str, nesting: &mut [NestingState]) -> Result<&Fragment, ExpandError> {
        let mut b = self.inner.get(name).ok_or_else(|| {
            ExpandError::BindingError(format!("could not find binding `{}`", name))
        })?;
        for s in nesting.iter_mut() {
            s.hit = true;
            b = match b {
                Binding::Fragment(_) => break,
                Binding::Nested(bs) => bs.get(s.idx).ok_or_else(|| {
                    s.at_end = true;
                    ExpandError::BindingError(format!("could not find nested binding `{}`", name))
                })?,
                Binding::Empty => {
                    s.at_end = true;
                    return Err(ExpandError::BindingError(format!(
                        "could not find empty binding `{}`",
                        name
                    )));
                }
            };
        }
        match b {
            Binding::Fragment(it) => Ok(it),
            Binding::Nested(_) => Err(ExpandError::BindingError(format!(
                "expected simple binding, found nested binding `{}`",
                name
            ))),
            Binding::Empty => Err(ExpandError::BindingError(format!(
                "expected simple binding, found empty binding `{}`",
                name
            ))),
        }
    }
}

pub(super) fn transcribe(
    template: &tt::Subtree,
    bindings: &Bindings,
) -> Result<tt::Subtree, ExpandError> {
    assert!(template.delimiter == None);
    let mut ctx = ExpandCtx { bindings: &bindings, nesting: Vec::new() };
    expand_subtree(&mut ctx, template)
}

#[derive(Debug)]
struct NestingState {
    idx: usize,
    hit: bool,
    at_end: bool,
}

#[derive(Debug)]
struct ExpandCtx<'a> {
    bindings: &'a Bindings,
    nesting: Vec<NestingState>,
}

fn expand_subtree(ctx: &mut ExpandCtx, template: &tt::Subtree) -> Result<tt::Subtree, ExpandError> {
    let mut buf: Vec<tt::TokenTree> = Vec::new();
    for op in parse_template(template) {
        match op? {
            Op::TokenTree(tt @ tt::TokenTree::Leaf(..)) => buf.push(tt.clone()),
            Op::TokenTree(tt::TokenTree::Subtree(tt)) => {
                let tt = expand_subtree(ctx, tt)?;
                buf.push(tt.into());
            }
            Op::Var { name, kind: _ } => {
                let fragment = expand_var(ctx, name)?;
                push_fragment(&mut buf, fragment);
            }
            Op::Repeat { subtree, kind, separator } => {
                let fragment = expand_repeat(ctx, subtree, kind, separator)?;
                push_fragment(&mut buf, fragment)
            }
        }
    }
    Ok(tt::Subtree { delimiter: template.delimiter, token_trees: buf })
}

fn expand_var(ctx: &mut ExpandCtx, v: &SmolStr) -> Result<Fragment, ExpandError> {
    let res = if v == "crate" {
        // We simply produce identifier `$crate` here. And it will be resolved when lowering ast to Path.
        let tt =
            tt::Leaf::from(tt::Ident { text: "$crate".into(), id: tt::TokenId::unspecified() })
                .into();
        Fragment::Tokens(tt)
    } else if !ctx.bindings.contains(v) {
        // Note that it is possible to have a `$var` inside a macro which is not bound.
        // For example:
        // ```
        // macro_rules! foo {
        //     ($a:ident, $b:ident, $c:tt) => {
        //         macro_rules! bar {
        //             ($bi:ident) => {
        //                 fn $bi() -> u8 {$c}
        //             }
        //         }
        //     }
        // ```
        // We just treat it a normal tokens
        let tt = tt::Subtree {
            delimiter: None,
            token_trees: vec![
                tt::Leaf::from(tt::Punct {
                    char: '$',
                    spacing: tt::Spacing::Alone,
                    id: tt::TokenId::unspecified(),
                })
                .into(),
                tt::Leaf::from(tt::Ident { text: v.clone(), id: tt::TokenId::unspecified() })
                    .into(),
            ],
        }
        .into();
        Fragment::Tokens(tt)
    } else {
        let fragment = ctx.bindings.get(&v, &mut ctx.nesting)?.clone();
        fragment
    };
    Ok(res)
}

fn expand_repeat(
    ctx: &mut ExpandCtx,
    template: &tt::Subtree,
    kind: RepeatKind,
    separator: Option<Separator>,
) -> Result<Fragment, ExpandError> {
    let mut buf: Vec<tt::TokenTree> = Vec::new();
    ctx.nesting.push(NestingState { idx: 0, at_end: false, hit: false });
    // Dirty hack to make macro-expansion terminate.
    // This should be replaced by a propper macro-by-example implementation
    let limit = 65536;
    let mut has_seps = 0;
    let mut counter = 0;

    loop {
        let res = expand_subtree(ctx, template);
        let nesting_state = ctx.nesting.last_mut().unwrap();
        if nesting_state.at_end || !nesting_state.hit {
            break;
        }
        nesting_state.idx += 1;
        nesting_state.hit = false;

        counter += 1;
        if counter == limit {
            log::warn!(
                "expand_tt excced in repeat pattern exceed limit => {:#?}\n{:#?}",
                template,
                ctx
            );
            break;
        }

        let mut t = match res {
            Ok(t) => t,
            Err(_) => continue,
        };
        t.delimiter = None;
        push_subtree(&mut buf, t);

        if let Some(ref sep) = separator {
            match sep {
                Separator::Ident(ident) => {
                    has_seps = 1;
                    buf.push(tt::Leaf::from(ident.clone()).into());
                }
                Separator::Literal(lit) => {
                    has_seps = 1;
                    buf.push(tt::Leaf::from(lit.clone()).into());
                }

                Separator::Puncts(puncts) => {
                    has_seps = puncts.len();
                    for punct in puncts {
                        buf.push(tt::Leaf::from(*punct).into());
                    }
                }
            }
        }

        if RepeatKind::ZeroOrOne == kind {
            break;
        }
    }

    ctx.nesting.pop().unwrap();
    for _ in 0..has_seps {
        buf.pop();
    }

    if RepeatKind::OneOrMore == kind && counter == 0 {
        return Err(ExpandError::UnexpectedToken);
    }

    // Check if it is a single token subtree without any delimiter
    // e.g {Delimiter:None> ['>'] /Delimiter:None>}
    let tt = tt::Subtree { delimiter: None, token_trees: buf }.into();
    Ok(Fragment::Tokens(tt))
}

fn push_fragment(buf: &mut Vec<tt::TokenTree>, fragment: Fragment) {
    match fragment {
        Fragment::Tokens(tt::TokenTree::Subtree(tt)) => push_subtree(buf, tt),
        Fragment::Tokens(tt) | Fragment::Ast(tt) => buf.push(tt),
    }
}

fn push_subtree(buf: &mut Vec<tt::TokenTree>, tt: tt::Subtree) {
    match tt.delimiter {
        None => buf.extend(tt.token_trees),
        _ => buf.push(tt.into()),
    }
}
