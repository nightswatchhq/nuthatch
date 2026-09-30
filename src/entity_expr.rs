//! RFC-0041 §3.3: the expression half of the entity lowering (#870).
//!
//! Expressions over [`Row`], evaluated by column *index* rather than by name - the plan carries the
//! shape, so the same evaluator serves any admitted entity. This is the piece slice 0 did not have:
//! its predicates were Rust functions matching one AST, so nothing could be lowered but that one.
//!
//! Two properties are load-bearing and neither is the obvious behaviour of a naive evaluator.
//!
//! **Overflow faults** (§3.3.1). Every arithmetic operation is checked. A `sum` whose running total
//! leaves `i128` is an error at the row that carried it past, not a total that resumes from the other
//! end of the number line. DuckDB and Postgres error here; DataFusion wraps by default
//! (arrow-datafusion#17539) and is a candidate engine under RFC-0042 - so "whatever the engine does"
//! is not a specification, and the contract lives on the entity.
//!
//! **NULL is unknown, not false.** SQL's three-valued logic, implemented rather than approximated:
//! `NULL AND false` is `false` while `NULL AND true` is `NULL`, and a predicate that evaluates to
//! `NULL` excludes its row without being an error. Getting this wrong produces a relation that
//! differs from DuckDB only on rows with nulls in them, which is the hardest kind of divergence to
//! notice and precisely what the parity gate in §8 exists to catch.

use crate::entity_row::{Row, Scalar};
use anyhow::{bail, Result};

/// Cast targets in the v1 subset. Deliberately three: §3.3 has no float, and a date/time type would
/// need a volatile-function story it does not yet have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Int,
    Str,
    Bool,
}

/// A comparison. Separate from [`Expr`] so the lowerer cannot invent one the evaluator lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// An expression over a positional row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Column(usize),
    Literal(Scalar),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Compare(Cmp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>),
    /// `CASE WHEN c THEN v ... ELSE e END`. A `WHEN` whose condition is `NULL` does not match -
    /// only `TRUE` does.
    Case {
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Coalesce(Vec<Expr>),
    Cast(Box<Expr>, Type),
}

impl Expr {
    pub fn eval(&self, row: &Row) -> Result<Scalar> {
        match self {
            Expr::Column(i) => Ok(row.get(*i).clone()),
            Expr::Literal(s) => Ok(s.clone()),

            Expr::Add(a, b) => arith(self, a, b, row, i128::checked_add),
            Expr::Sub(a, b) => arith(self, a, b, row, i128::checked_sub),
            Expr::Mul(a, b) => arith(self, a, b, row, i128::checked_mul),

            Expr::Compare(op, a, b) => {
                let (l, r) = (a.eval(row)?, b.eval(row)?);
                // NULL compared with anything is unknown, including with NULL. `IS NULL` is the
                // operator for that question and it is spelled separately.
                if l.is_null() || r.is_null() {
                    return Ok(Scalar::Null);
                }
                if std::mem::discriminant(&l) != std::mem::discriminant(&r) {
                    bail!("cannot compare {l:?} with {r:?}: the entity subset has no implicit cross-type coercion")
                }
                let ord = l.cmp(&r);
                Ok(Scalar::Bool(match op {
                    Cmp::Eq => ord.is_eq(),
                    Cmp::Ne => ord.is_ne(),
                    Cmp::Lt => ord.is_lt(),
                    Cmp::Le => ord.is_le(),
                    Cmp::Gt => ord.is_gt(),
                    Cmp::Ge => ord.is_ge(),
                }))
            }

            // Three-valued AND: false dominates, so `NULL AND false` is false. Evaluating both sides
            // rather than short-circuiting keeps a type error on the other side visible - a silently
            // skipped bad expression is a lowering bug that only shows up on some rows.
            Expr::And(a, b) => Ok(match (truth(&a.eval(row)?)?, truth(&b.eval(row)?)?) {
                (Some(false), _) | (_, Some(false)) => Scalar::Bool(false),
                (Some(true), Some(true)) => Scalar::Bool(true),
                _ => Scalar::Null,
            }),
            // Three-valued OR: true dominates.
            Expr::Or(a, b) => Ok(match (truth(&a.eval(row)?)?, truth(&b.eval(row)?)?) {
                (Some(true), _) | (_, Some(true)) => Scalar::Bool(true),
                (Some(false), Some(false)) => Scalar::Bool(false),
                _ => Scalar::Null,
            }),
            Expr::Not(a) => Ok(match truth(&a.eval(row)?)? {
                Some(b) => Scalar::Bool(!b),
                None => Scalar::Null,
            }),

            Expr::IsNull(a) => Ok(Scalar::Bool(a.eval(row)?.is_null())),

            Expr::Case { whens, otherwise } => {
                for (cond, value) in whens {
                    if truth(&cond.eval(row)?)? == Some(true) {
                        return value.eval(row);
                    }
                }
                match otherwise {
                    Some(e) => e.eval(row),
                    // SQL: a CASE with no matching WHEN and no ELSE is NULL, not an error.
                    None => Ok(Scalar::Null),
                }
            }

            Expr::Coalesce(args) => {
                for a in args {
                    let v = a.eval(row)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Scalar::Null)
            }

            Expr::Cast(a, ty) => cast(a.eval(row)?, *ty),
        }
    }
}

/// The values an expression may evaluate to: a set over the three types and NULL.
///
/// A set rather than one type because [`Expr::eval`] only evaluates the `CASE` branch it takes, and a
/// typed NULL never reaches a comparison. The check refuses only what fails on every row it
/// evaluates; what may fail on some rows is a value-dependent fault, like overflow, for runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Types {
    int: bool,
    str: bool,
    bool: bool,
    null: bool,
}

impl Types {
    pub const NULL: Types = Types {
        int: false,
        str: false,
        bool: false,
        null: true,
    };

    pub fn of(ty: Type) -> Self {
        Types {
            int: ty == Type::Int,
            str: ty == Type::Str,
            bool: ty == Type::Bool,
            null: false,
        }
    }

    fn union(self, o: Types) -> Types {
        Types {
            int: self.int || o.int,
            str: self.str || o.str,
            bool: self.bool || o.bool,
            null: self.null || o.null,
        }
    }

    fn has(self, ty: Type) -> bool {
        match ty {
            Type::Int => self.int,
            Type::Str => self.str,
            Type::Bool => self.bool,
        }
    }

    /// Whether some value this may take is `ty` or NULL: the operand can succeed on some row.
    pub fn admits(self, ty: Type) -> bool {
        self.null || self.has(ty)
    }

    fn named(self) -> String {
        let mut out = Vec::new();
        for (on, name) in [
            (self.int, "Int"),
            (self.str, "Str"),
            (self.bool, "Bool"),
            (self.null, "NULL"),
        ] {
            if on {
                out.push(name);
            }
        }
        out.join(" or ")
    }
}

impl Expr {
    /// The values this expression may take over a row whose columns have `cols` types, or the refusal
    /// [`Expr::eval`] would raise on every row it evaluated (#1590). Checked at load, so an entity that
    /// could only fault is refused before it starts rather than on the first block that reaches it.
    pub fn static_type(&self, cols: &[Type]) -> Result<Types> {
        // What a condition may be when `truth` reads it: anything but a boolean or NULL faults.
        let condition = |what: &str, t: Types| {
            if t.admits(Type::Bool) {
                Ok(())
            } else {
                bail!("{what} needs Bool, got {}", t.named())
            }
        };
        Ok(match self {
            Expr::Column(i) => Types::of(
                *cols
                    .get(*i)
                    .ok_or_else(|| anyhow::anyhow!("column {i} is outside the row"))?,
            ),
            Expr::Literal(s) => match s {
                Scalar::Null => Types::NULL,
                Scalar::Bool(_) => Types::of(Type::Bool),
                Scalar::Int(_) => Types::of(Type::Int),
                Scalar::Str(_) => Types::of(Type::Str),
            },
            // Both operands are evaluated, and NULL returns before either type is looked at.
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) => {
                let (a, b) = (a.static_type(cols)?, b.static_type(cols)?);
                let null = a.null || b.null;
                let int = a.int && b.int;
                if !null && !int {
                    bail!("arithmetic needs Int, got {} and {}", a.named(), b.named());
                }
                Types {
                    int,
                    null,
                    ..Types::default()
                }
            }
            Expr::Compare(_, a, b) => {
                let (a, b) = (a.static_type(cols)?, b.static_type(cols)?);
                let null = a.null || b.null;
                let same = [Type::Int, Type::Str, Type::Bool]
                    .iter()
                    .any(|t| a.has(*t) && b.has(*t));
                if !null && !same {
                    bail!(
                        "a comparison of {} with {}: the entity subset has no implicit coercion",
                        a.named(),
                        b.named()
                    );
                }
                Types {
                    bool: same,
                    null,
                    ..Types::default()
                }
            }
            Expr::And(a, b) | Expr::Or(a, b) => {
                let (a, b) = (a.static_type(cols)?, b.static_type(cols)?);
                condition("a logical operator", a)?;
                condition("a logical operator", b)?;
                Types {
                    bool: true,
                    null: a.null || b.null,
                    ..Types::default()
                }
            }
            Expr::Not(a) => {
                let a = a.static_type(cols)?;
                condition("NOT", a)?;
                Types {
                    bool: a.bool,
                    null: a.null,
                    ..Types::default()
                }
            }
            Expr::IsNull(a) => {
                a.static_type(cols)?;
                Types::of(Type::Bool)
            }
            // Conditions are read in order and only the taken branch is evaluated. A branch that
            // faults faults only its rows; a condition that must fault ends every path through it.
            Expr::Case { whens, otherwise } => {
                let mut out = Types::default();
                let mut reached_else = true;
                for (i, (when, then)) in whens.iter().enumerate() {
                    match when.static_type(cols) {
                        Ok(t) if t.admits(Type::Bool) => {}
                        failed if i == 0 => {
                            condition("a CASE condition", failed?)?;
                        }
                        _ => {
                            reached_else = false;
                            break;
                        }
                    }
                    if let Ok(t) = then.static_type(cols) {
                        out = out.union(t);
                    }
                }
                if reached_else {
                    match otherwise {
                        Some(e) => {
                            if let Ok(t) = e.static_type(cols) {
                                out = out.union(t);
                            }
                        }
                        None => out = out.union(Types::NULL),
                    }
                }
                if out == Types::default() {
                    bail!("every branch of this CASE fails");
                }
                out
            }
            // Arguments are evaluated in order until one is not NULL, so an argument after one that
            // is never NULL is never reached, and the result is NULL only if every argument can be.
            Expr::Coalesce(args) => {
                let mut out = Types::default();
                let mut all_null = true;
                for (i, a) in args.iter().enumerate() {
                    let t = match a.static_type(cols) {
                        Ok(t) => t,
                        Err(e) if i == 0 => return Err(e),
                        Err(_) => {
                            all_null = false;
                            break;
                        }
                    };
                    out = out.union(Types { null: false, ..t });
                    if !t.null {
                        all_null = false;
                        break;
                    }
                }
                out.null = all_null;
                if out == Types::default() {
                    bail!("every argument of this COALESCE fails");
                }
                out
            }
            Expr::Cast(a, ty) => {
                let a = a.static_type(cols)?;
                let converts = a.int || a.bool || (a.str && *ty != Type::Bool);
                if !converts && !a.null {
                    bail!("cast from VARCHAR to BOOLEAN is not in the entity subset")
                }
                let mut out = if converts {
                    Types::of(*ty)
                } else {
                    Types::default()
                };
                out.null = a.null;
                out
            }
        })
    }
}

/// Checked integer arithmetic with NULL propagation. §3.3.1: an unrepresentable result is a fault.
fn arith(
    whole: &Expr,
    a: &Expr,
    b: &Expr,
    row: &Row,
    op: fn(i128, i128) -> Option<i128>,
) -> Result<Scalar> {
    let (l, r) = (a.eval(row)?, b.eval(row)?);
    if l.is_null() || r.is_null() {
        return Ok(Scalar::Null);
    }
    let (Some(x), Some(y)) = (l.as_int(), r.as_int()) else {
        bail!("arithmetic on non-integer operands {l:?} and {r:?}")
    };
    match op(x, y) {
        Some(v) => Ok(Scalar::Int(v)),
        // Deliberately loud and specific. A wrap here would produce a *plausible* number of the
        // wrong sign or magnitude, stored and sealed as canonical - the failure absent data never
        // has, because absent data announces itself (RFC-0041 §3.3.1).
        None => bail!(
            "arithmetic overflow evaluating {whole:?} on {x} and {y}: the result does not fit i128. \
             An entity faults rather than wrapping."
        ),
    }
}

/// SQL truth: `Some(bool)` for known, `None` for unknown (NULL). A non-boolean is a lowering bug.
fn truth(s: &Scalar) -> Result<Option<bool>> {
    match s {
        Scalar::Bool(b) => Ok(Some(*b)),
        Scalar::Null => Ok(None),
        other => bail!("expected a boolean in a logical position, got {other:?}"),
    }
}

/// Casts refuse rather than truncate or silently NULL (§3.3.1). `TRY_CAST` is not in the v1 subset.
fn cast(v: Scalar, ty: Type) -> Result<Scalar> {
    if v.is_null() {
        return Ok(Scalar::Null);
    }
    Ok(match (&v, ty) {
        (Scalar::Int(_), Type::Int)
        | (Scalar::Str(_), Type::Str)
        | (Scalar::Bool(_), Type::Bool) => v,
        (Scalar::Int(i), Type::Str) => Scalar::Str(i.to_string()),
        (Scalar::Bool(b), Type::Str) => Scalar::Str(b.to_string()),
        (Scalar::Bool(b), Type::Int) => Scalar::Int(i128::from(*b)),
        (Scalar::Str(s), Type::Int) => match s.parse::<i128>() {
            Ok(i) => Scalar::Int(i),
            Err(_) => bail!("cast to INTEGER refused: `{s}` is not an exact integer"),
        },
        (Scalar::Int(i), Type::Bool) => Scalar::Bool(*i != 0),
        (Scalar::Str(_), Type::Bool) => {
            bail!("cast from VARCHAR to BOOLEAN is not in the entity subset")
        }
        // Unreachable - NULL returned above - but spelled out rather than caught by a wildcard, so
        // adding a Scalar variant is a compile error here instead of a silent passthrough.
        (Scalar::Null, _) => Scalar::Null,
    })
}

/// Does this predicate admit the row? **Only `TRUE` does** - `NULL` excludes it, as SQL's `WHERE`
/// does, and without being an error.
pub fn admits(pred: &Expr, row: &Row) -> Result<bool> {
    Ok(truth(&pred.eval(row)?)? == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(i: usize) -> Box<Expr> {
        Box::new(Expr::Column(i))
    }
    fn int(i: i128) -> Box<Expr> {
        Box::new(Expr::Literal(Scalar::Int(i)))
    }
    fn null() -> Box<Expr> {
        Box::new(Expr::Literal(Scalar::Null))
    }
    fn row(v: Vec<Scalar>) -> Row {
        Row(v)
    }

    #[test]
    fn arithmetic_is_exact_and_by_column_index() {
        let e = Expr::Add(col(0), Expr::Mul(col(1), int(2)).into());
        let r = row(vec![Scalar::Int(10), Scalar::Int(5)]);
        assert_eq!(e.eval(&r).unwrap(), Scalar::Int(20));
    }

    /// §3.3.1, the contract that cannot be retrofitted.
    #[test]
    fn overflow_is_an_error_and_never_a_wrap() {
        let e = Expr::Add(int(i128::MAX), int(1));
        let err = e.eval(&row(vec![])).unwrap_err().to_string();
        assert!(err.contains("overflow"), "{err}");
        assert!(err.contains("faults rather than wrapping"), "{err}");

        // The wrap this refuses would be `i128::MIN` - a plausible number, of the wrong sign.
        assert_eq!(i128::MAX.wrapping_add(1), i128::MIN);
    }

    #[test]
    fn overflow_in_a_subtraction_or_product_faults_too() {
        for e in [
            Expr::Sub(int(i128::MIN), int(1)),
            Expr::Mul(int(i128::MAX), int(2)),
        ] {
            assert!(e.eval(&row(vec![])).is_err(), "{e:?} must fault");
        }
    }

    #[test]
    fn null_propagates_through_arithmetic_rather_than_defaulting_to_zero() {
        let e = Expr::Add(col(0), int(1));
        assert_eq!(e.eval(&row(vec![Scalar::Null])).unwrap(), Scalar::Null);
    }

    /// The half a naive evaluator gets wrong. `NULL AND false` is **false**, because false dominates
    /// - the row is excluded regardless of what the unknown turns out to be.
    #[test]
    fn three_valued_logic_matches_sql() {
        let t = || Box::new(Expr::Literal(Scalar::Bool(true)));
        let f = || Box::new(Expr::Literal(Scalar::Bool(false)));
        let n = null;
        let r = row(vec![]);

        assert_eq!(Expr::And(n(), f()).eval(&r).unwrap(), Scalar::Bool(false));
        assert_eq!(Expr::And(n(), t()).eval(&r).unwrap(), Scalar::Null);
        assert_eq!(Expr::Or(n(), t()).eval(&r).unwrap(), Scalar::Bool(true));
        assert_eq!(Expr::Or(n(), f()).eval(&r).unwrap(), Scalar::Null);
        assert_eq!(Expr::Not(n()).eval(&r).unwrap(), Scalar::Null);
        assert_eq!(Expr::Not(t()).eval(&r).unwrap(), Scalar::Bool(false));
    }

    #[test]
    fn comparison_with_null_is_unknown_not_equal() {
        let r = row(vec![Scalar::Null]);
        assert_eq!(
            Expr::Compare(Cmp::Eq, col(0), null()).eval(&r).unwrap(),
            Scalar::Null,
            "NULL = NULL is unknown; IS NULL is the operator for that question"
        );
        assert_eq!(Expr::IsNull(col(0)).eval(&r).unwrap(), Scalar::Bool(true));
    }

    /// `WHERE` admits only TRUE. An unknown predicate excludes the row and is not an error.
    #[test]
    fn a_null_predicate_excludes_the_row_without_erroring() {
        let p = Expr::Compare(Cmp::Gt, col(0), int(0));
        assert!(admits(&p, &row(vec![Scalar::Int(1)])).unwrap());
        assert!(!admits(&p, &row(vec![Scalar::Int(-1)])).unwrap());
        assert!(!admits(&p, &row(vec![Scalar::Null])).unwrap());
    }

    #[test]
    fn case_needs_a_true_when_and_is_null_without_an_else() {
        let e = Expr::Case {
            whens: vec![
                (Expr::Compare(Cmp::Gt, col(0), int(10)), *int(1)),
                (Expr::Compare(Cmp::Gt, col(0), int(5)), *int(2)),
            ],
            otherwise: None,
        };
        assert_eq!(e.eval(&row(vec![Scalar::Int(20)])).unwrap(), Scalar::Int(1));
        assert_eq!(e.eval(&row(vec![Scalar::Int(7)])).unwrap(), Scalar::Int(2));
        assert_eq!(e.eval(&row(vec![Scalar::Int(1)])).unwrap(), Scalar::Null);
        // A NULL condition does not match - only TRUE does.
        assert_eq!(e.eval(&row(vec![Scalar::Null])).unwrap(), Scalar::Null);
    }

    #[test]
    fn coalesce_takes_the_first_non_null() {
        let e = Expr::Coalesce(vec![*null(), *col(0), *int(9)]);
        assert_eq!(e.eval(&row(vec![Scalar::Int(3)])).unwrap(), Scalar::Int(3));
        assert_eq!(e.eval(&row(vec![Scalar::Null])).unwrap(), Scalar::Int(9));
    }

    #[test]
    fn a_cast_that_cannot_represent_its_input_refuses() {
        // §3.3.1 again: no truncation, no silent NULL. TRY_CAST is not in the subset.
        let bad = Expr::Cast(
            Box::new(Expr::Literal(Scalar::Str("12.5".into()))),
            Type::Int,
        );
        let err = bad.eval(&row(vec![])).unwrap_err().to_string();
        assert!(err.contains("refused"), "{err}");

        let good = Expr::Cast(Box::new(Expr::Literal(Scalar::Str("42".into()))), Type::Int);
        assert_eq!(good.eval(&row(vec![])).unwrap(), Scalar::Int(42));
        // NULL casts to NULL, which is not the same thing as a failed cast.
        let n = Expr::Cast(null(), Type::Int);
        assert_eq!(n.eval(&row(vec![])).unwrap(), Scalar::Null);
    }

    #[test]
    fn cross_type_comparison_is_refused_rather_than_coerced() {
        // Implicit coercion is where engines quietly disagree; the subset has none.
        let e = Expr::Compare(Cmp::Eq, col(0), int(1));
        assert!(e.eval(&row(vec![Scalar::Str("1".into())])).is_err());
    }
}

#[cfg(test)]
mod static_type_tests {
    use super::*;

    fn col(i: usize) -> Box<Expr> {
        Box::new(Expr::Column(i))
    }
    fn lit(s: Scalar) -> Box<Expr> {
        Box::new(Expr::Literal(s))
    }
    fn s(v: &str) -> Box<Expr> {
        lit(Scalar::Str(v.into()))
    }
    fn i(v: i128) -> Box<Expr> {
        lit(Scalar::Int(v))
    }
    fn null() -> Box<Expr> {
        lit(Scalar::Null)
    }
    fn gt0() -> Expr {
        Expr::Compare(Cmp::Gt, col(0), i(0))
    }

    /// Held against the evaluator itself: over rows of an Int, a Str and a Bool column, an expression
    /// the typer refuses must fail on every row, and one it admits must succeed on at least one,
    /// with its result inside the admitted set. As a filter or a SUM input, the same holds for the
    /// condition and integer rules the binder adds.
    #[test]
    fn the_typer_agrees_with_the_evaluator() {
        let cols = [Type::Int, Type::Str, Type::Bool];
        let rows: Vec<Row> = [5, 0, -1]
            .iter()
            .map(|v| {
                Row(vec![
                    Scalar::Int(*v),
                    Scalar::Str("x".into()),
                    Scalar::Bool(*v > 0),
                ])
            })
            .collect();
        let within = |t: Types, v: &Scalar| match v {
            Scalar::Null => t.null,
            Scalar::Int(_) => t.int,
            Scalar::Str(_) => t.str,
            Scalar::Bool(_) => t.bool,
        };
        let cases: Vec<(&str, Expr, bool)> = vec![
            ("NULL + '1'", Expr::Add(null(), s("1")), true),
            ("fee + '1'", Expr::Add(col(0), s("1")), false),
            (
                "COALESCE(fee, fee + '1')",
                Expr::Coalesce(vec![Expr::Column(0), Expr::Add(col(0), s("1"))]),
                true,
            ),
            (
                "COALESCE('1', 0) + 1",
                Expr::Add(Box::new(Expr::Coalesce(vec![*s("1"), *i(0)])), i(1)),
                false,
            ),
            (
                "CASE WHEN fee > 0 THEN '1' ELSE 0 END",
                Expr::Case {
                    whens: vec![(gt0(), *s("1"))],
                    otherwise: Some(i(0)),
                },
                true,
            ),
            (
                "CAST(CASE WHEN fee > 0 THEN 'x' ELSE NULL END AS BOOLEAN)",
                Expr::Cast(
                    Box::new(Expr::Case {
                        whens: vec![(gt0(), *s("x"))],
                        otherwise: Some(null()),
                    }),
                    Type::Bool,
                ),
                true,
            ),
            (
                "CASE WHEN fee > 0 THEN fee + 'x' ELSE 0 END",
                Expr::Case {
                    whens: vec![(gt0(), Expr::Add(col(0), s("x")))],
                    otherwise: Some(i(0)),
                },
                true,
            ),
            (
                "CAST(sym AS BOOLEAN)",
                Expr::Cast(col(1), Type::Bool),
                false,
            ),
            (
                "CAST(NULL AS VARCHAR) = fee",
                Expr::Compare(Cmp::Eq, Box::new(Expr::Cast(null(), Type::Str)), col(0)),
                true,
            ),
            ("fee = sym", Expr::Compare(Cmp::Eq, col(0), col(1)), false),
            ("flag AND fee", Expr::And(col(2), col(0)), false),
            (
                "CASE WHEN sym THEN 1 END",
                Expr::Case {
                    whens: vec![(Expr::Column(1), *i(1))],
                    otherwise: None,
                },
                false,
            ),
        ];
        for (name, e, admitted) in cases {
            let typed = e.static_type(&cols);
            let evals: Vec<Result<Scalar>> = rows.iter().map(|r| e.eval(r)).collect();
            assert_eq!(typed.is_ok(), admitted, "{name}: {typed:?}");
            match typed {
                Ok(t) => {
                    assert!(evals.iter().any(|v| v.is_ok()), "{name} never succeeds");
                    for v in evals.iter().flatten() {
                        assert!(within(t, v), "{name} gave {v:?} outside {t:?}");
                    }
                }
                Err(_) => assert!(
                    evals.iter().all(|v| v.is_err()),
                    "{name} succeeds somewhere"
                ),
            }
        }

        // The binder's rules on top: a filter must be able to be a condition, and SUM needs an Int.
        let cond = |e: &Expr| e.static_type(&cols).unwrap().admits(Type::Bool);
        let int = |e: &Expr| e.static_type(&cols).unwrap().admits(Type::Int);
        assert!(
            !cond(&Expr::Coalesce(vec![*null(), Expr::Column(0)])),
            "COALESCE(NULL, fee)"
        );
        assert!(
            !int(&Expr::Coalesce(vec![*null(), *s("1")])),
            "COALESCE(NULL, '1')"
        );
        assert!(!cond(&Expr::Column(0)), "WHERE fee");
        assert!(cond(&gt0()));
    }
}
