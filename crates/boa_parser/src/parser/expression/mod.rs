//! Expression parsing.
//!
//! More information:
//!  - [MDN documentation][mdn]
//!  - [ECMAScript specification][spec]
//!
//! [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators
//! [spec]: https://tc39.es/ecma262/#sec-ecmascript-language-expressions

mod assignment;
mod fpl_or_exp;
mod identifiers;
mod left_hand_side;
mod primary;
mod unary;
mod update;

pub(in crate::parser) mod await_expr;

#[cfg(test)]
mod tests;

use crate::{
    Error,
    lexer::{InputElement, Token, TokenKind},
    parser::{
        AllowAwait, AllowIn, AllowYield, Cursor, OrAbrupt, ParseResult, TokenParser,
        expression::assignment::ExponentiationExpression,
    },
    source::ReadChar,
};
use boa_ast::{
    self as ast, Keyword, Position, Punctuator, Spanned,
    expression::{
        Identifier,
        operator::{
            Binary, BinaryInPrivate,
            binary::{ArithmeticOp, BinaryOp, BitwiseOp, LogicalOp, RelationalOp},
        },
    },
    function::PrivateName,
};
use boa_interner::{Interner, Sym};

pub(super) use self::{assignment::AssignmentExpression, primary::Initializer};
pub(in crate::parser) use {
    fpl_or_exp::FormalParameterListOrExpression,
    identifiers::{BindingIdentifier, LabelIdentifier},
    left_hand_side::LeftHandSideExpression,
    primary::object_initializer::{
        AsyncGeneratorMethod, AsyncMethod, GeneratorMethod, PropertyName,
    },
};

/// Expression parsing.
///
/// More information:
///  - [MDN documentation][mdn]
///  - [ECMAScript specification][spec]
///
/// [mdn]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators
/// [spec]: https://tc39.es/ecma262/#prod-Expression
#[derive(Debug, Clone, Copy)]
pub(super) struct Expression {
    allow_in: AllowIn,
    allow_yield: AllowYield,
    allow_await: AllowAwait,
}

impl Expression {
    /// Creates a new `Expression` parser.
    pub(super) fn new<I, Y, A>(allow_in: I, allow_yield: Y, allow_await: A) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
        A: Into<AllowAwait>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
            allow_await: allow_await.into(),
        }
    }
}

impl<R> TokenParser<R> for Expression
where
    R: ReadChar,
{
    type Output = ast::Expression;

    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        let first = AssignmentExpression::new(self.allow_in, self.allow_yield, self.allow_await)
            .parse(cursor, interner)?;
        let mut rest = Vec::new();
        while let Some(tok) = cursor.peek(0, interner)? {
            match *tok.kind() {
                TokenKind::Punctuator(Punctuator::Comma) => {
                    if cursor.peek(1, interner).or_abrupt()?.kind()
                        == &TokenKind::Punctuator(Punctuator::CloseParen)
                    {
                        break;
                    }

                    if cursor.peek(1, interner).or_abrupt()?.kind()
                        == &TokenKind::Punctuator(Punctuator::Spread)
                    {
                        break;
                    }

                    cursor.advance(interner);

                    rest.push(
                        AssignmentExpression::new(
                            self.allow_in,
                            self.allow_yield,
                            self.allow_await,
                        )
                        .parse(cursor, interner)?,
                    );
                }
                _ => break,
            }
        }

        Ok(comma_chain(first, rest))
    }
}

/// Chains shorter than this keep upstream's left-leaning tree, so nearly all code
/// parses to the same AST and bytecode as upstream.
const LEFT_LEANING_COMMA_OPERANDS: usize = 32;

/// `a, b, c` runs the same operands in the same order whatever the tree's shape, so long
/// chains build balanced: scope analysis, bytecompile and drop recurse log2(n) deep, not n.
fn comma_chain(first: ast::Expression, rest: Vec<ast::Expression>) -> ast::Expression {
    fn comma(lhs: ast::Expression, rhs: ast::Expression) -> ast::Expression {
        Binary::new(BinaryOp::Comma, lhs, rhs).into()
    }
    fn balanced(items: &mut std::vec::IntoIter<ast::Expression>, n: usize) -> ast::Expression {
        if n == 1 {
            return items.next().expect("one operand per slot");
        }
        let lhs = balanced(items, n / 2);
        let rhs = balanced(items, n - n / 2);
        comma(lhs, rhs)
    }
    if rest.len() < LEFT_LEANING_COMMA_OPERANDS {
        return rest.into_iter().fold(first, comma);
    }
    let n = rest.len() + 1;
    let mut all = Vec::with_capacity(n);
    all.push(first);
    all.extend(rest);
    balanced(&mut all.into_iter(), n)
}

/// Parses every binary operator looser than `**` in one frame, by operator precedence: a
/// parser per grammar level cost nine frames per operand, ~4 KiB of wasm stack per nesting.
#[derive(Debug, Clone, Copy)]
struct ShortCircuitExpression {
    allow_in: AllowIn,
    allow_yield: AllowYield,
    allow_await: AllowAwait,
}

impl ShortCircuitExpression {
    /// Creates a new `ShortCircuitExpression` parser.
    pub(super) fn new<I, Y, A>(allow_in: I, allow_yield: Y, allow_await: A) -> Self
    where
        I: Into<AllowIn>,
        Y: Into<AllowYield>,
        A: Into<AllowAwait>,
    {
        Self {
            allow_in: allow_in.into(),
            allow_yield: allow_yield.into(),
            allow_await: allow_await.into(),
        }
    }
}

const PREC_LOGICAL_OR: u8 = 1; // `||` and `??`, which cannot mix without parentheses
const PREC_RELATIONAL: u8 = 7;
const PREC_MULTIPLICATIVE: u8 = 10;

const fn precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Logical(LogicalOp::Or | LogicalOp::Coalesce) => PREC_LOGICAL_OR,
        BinaryOp::Logical(LogicalOp::And) => 2,
        BinaryOp::Bitwise(BitwiseOp::Or) => 3,
        BinaryOp::Bitwise(BitwiseOp::Xor) => 4,
        BinaryOp::Bitwise(BitwiseOp::And) => 5,
        BinaryOp::Relational(
            RelationalOp::Equal
            | RelationalOp::NotEqual
            | RelationalOp::StrictEqual
            | RelationalOp::StrictNotEqual,
        ) => 6,
        BinaryOp::Relational(_) => PREC_RELATIONAL,
        BinaryOp::Bitwise(BitwiseOp::Shl | BitwiseOp::Shr | BitwiseOp::UShr) => 8,
        BinaryOp::Arithmetic(ArithmeticOp::Add | ArithmeticOp::Sub) => 9,
        BinaryOp::Arithmetic(_) | BinaryOp::Comma => PREC_MULTIPLICATIVE,
    }
}

/// A left operand and its operator, waiting for the right operand. `#x in o` has no
/// `Binary` node, so its private name waits in a variant of its own.
enum Pending {
    Binary(ast::Expression, BinaryOp),
    PrivateIn(PrivateName),
}

impl Pending {
    const fn precedence(&self) -> u8 {
        match self {
            Self::Binary(_, op) => precedence(*op),
            Self::PrivateIn(_) => PREC_RELATIONAL,
        }
    }

    fn reduce(self, rhs: ast::Expression) -> ast::Expression {
        match self {
            Self::Binary(lhs, op) => Binary::new(op, lhs, rhs).into(),
            Self::PrivateIn(name) => BinaryInPrivate::new(name, rhs).into(),
        }
    }
}

/// The binary operator `tok` stands for, if it is one parsed here. `in` only counts when
/// `allow_in`, but an escaped `in` or `instanceof` is an error either way.
fn binary_operator(tok: &Token, allow_in: AllowIn) -> ParseResult<Option<BinaryOp>> {
    Ok(match *tok.kind() {
        TokenKind::Punctuator(
            op @ (Punctuator::Coalesce
            | Punctuator::BoolOr
            | Punctuator::BoolAnd
            | Punctuator::Or
            | Punctuator::Xor
            | Punctuator::And
            | Punctuator::Eq
            | Punctuator::NotEq
            | Punctuator::StrictEq
            | Punctuator::StrictNotEq
            | Punctuator::LessThan
            | Punctuator::GreaterThan
            | Punctuator::LessThanOrEq
            | Punctuator::GreaterThanOrEq
            | Punctuator::LeftSh
            | Punctuator::RightSh
            | Punctuator::URightSh
            | Punctuator::Add
            | Punctuator::Sub
            | Punctuator::Mul
            | Punctuator::Div
            | Punctuator::Mod),
        ) => op.as_binary_op(),
        TokenKind::Keyword((Keyword::InstanceOf | Keyword::In, true)) => {
            return Err(Error::general(
                "Keyword must not contain escaped characters",
                tok.span().start(),
            ));
        }
        TokenKind::Keyword((op @ Keyword::InstanceOf, false)) => op.as_binary_op(),
        TokenKind::Keyword((op @ Keyword::In, false)) if allow_in.0 => op.as_binary_op(),
        _ => None,
    })
}

/// Consumes `#name in` and returns the name, if that is what comes next. Only valid where a
/// `RelationalExpression` may start: `#x in o` is not a `ShiftExpression`.
fn private_in<R: ReadChar>(
    cursor: &mut Cursor<R>,
    interner: &mut Interner,
) -> ParseResult<Option<PrivateName>> {
    let token = cursor.peek(0, interner).or_abrupt()?;
    let TokenKind::PrivateIdentifier(identifier) = *token.kind() else {
        return Ok(None);
    };
    let identifier_span = token.span();
    let token = cursor.peek(1, interner).or_abrupt()?;
    match token.kind() {
        TokenKind::Keyword((Keyword::In, true)) => Err(Error::general(
            "Keyword must not contain escaped characters",
            token.span().start(),
        )),
        TokenKind::Keyword((Keyword::In, false)) => {
            cursor.advance(interner);
            cursor.advance(interner);
            Ok(Some(PrivateName::new(identifier, identifier_span)))
        }
        _ => Ok(None),
    }
}

impl<R> TokenParser<R> for ShortCircuitExpression
where
    R: ReadChar,
{
    type Output = FormalParameterListOrExpression;

    fn parse(self, cursor: &mut Cursor<R>, interner: &mut Interner) -> ParseResult<Self::Output> {
        let mut pending: Vec<Pending> = Vec::new();
        let mut prev_prec = 0; // the operator before this operand; 0 before the first
        let mut seen_logical = false;
        let mut seen_coalesce = false;

        let mut rhs = loop {
            if self.allow_in.0
                && prev_prec < PREC_RELATIONAL
                && let Some(name) = private_in(cursor, interner)?
            {
                pending.push(Pending::PrivateIn(name));
                prev_prec = PREC_RELATIONAL;
                continue;
            }

            // The per-level parsers set the goal on entering MultiplicativeExpression, which
            // the right operand of `*`, `/` and `%` skipped.
            if prev_prec != PREC_MULTIPLICATIVE {
                cursor.set_goal(InputElement::Div);
            }
            let operand = ExponentiationExpression::new(self.allow_yield, self.allow_await)
                .parse(cursor, interner)?;
            let operand = if pending.is_empty() {
                match operand {
                    FormalParameterListOrExpression::Expression(expr) => expr,
                    params => return Ok(params),
                }
            } else {
                operand.try_into_expression()?
            };

            let Some(tok) = cursor.peek(0, interner)? else {
                break operand;
            };
            let Some(op) = binary_operator(tok, self.allow_in)? else {
                break operand;
            };
            match op {
                BinaryOp::Logical(LogicalOp::And | LogicalOp::Or) => {
                    if seen_coalesce {
                        return Err(Error::expected(
                            ["??".to_owned()],
                            tok.to_string(interner),
                            tok.span(),
                            "logical expression (cannot use '??' without parentheses within '||' or '&&')",
                        ));
                    }
                    seen_logical = true;
                }
                BinaryOp::Logical(LogicalOp::Coalesce) => {
                    if seen_logical {
                        return Err(Error::expected(
                            ["&&".to_owned(), "||".to_owned()],
                            tok.to_string(interner),
                            tok.span(),
                            "cannot use '??' unparenthesized within '||' or '&&'",
                        ));
                    }
                    seen_coalesce = true;
                }
                _ => {}
            }
            cursor.advance(interner);

            let prec = precedence(op);
            let mut lhs = operand;
            while let Some(top) = pending.last() {
                // boa always built `a || b || c` leaning right; keep that tree so the bytecode
                // does not change. Every other level leans left, as the grammar does.
                let right_assoc = op == BinaryOp::Logical(LogicalOp::Or)
                    && matches!(top, Pending::Binary(_, BinaryOp::Logical(LogicalOp::Or)));
                let top_prec = top.precedence();
                if top_prec < prec || (top_prec == prec && right_assoc) {
                    break;
                }
                let top = pending.pop().expect("peeked above");
                lhs = top.reduce(lhs);
            }
            pending.push(Pending::Binary(lhs, op));
            prev_prec = prec;
        };

        while let Some(top) = pending.pop() {
            rhs = top.reduce(rhs);
        }
        Ok(rhs.into())
    }
}

/// Returns an error if `arguments` or `eval` are used as identifier in strict mode.
fn check_strict_arguments_or_eval(ident: Identifier, position: Position) -> ParseResult<()> {
    match ident.sym() {
        Sym::ARGUMENTS => Err(Error::general(
            "unexpected identifier `arguments` in strict mode",
            position,
        )),
        Sym::EVAL => Err(Error::general(
            "unexpected identifier `eval` in strict mode",
            position,
        )),
        _ => Ok(()),
    }
}
