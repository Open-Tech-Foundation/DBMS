//! Surface → IR lowering (`ARCHITECTURE.md` §3.7).
//!
//! The pipeline form lowers directly: the stage list folds left-to-right into
//! a [`Plan`] tree. The clause form is **sugar**: it desugars into the fixed
//! stage order FROM → WHERE → GROUP → HAVING → PROJECT (→ DISTINCT) → ORDER →
//! LIMIT → CURSOR and reuses the same fold — proving "two surfaces, one core"
//! by construction (`SPEC.md` §5.1).
//!
//! Lowering enforces *structural* grammar (a pipeline starts at a scan,
//! `group` aggregates really are aggregates); names, types, and the §6 safety
//! rules belong to the validator (Phase 9).

use common::{CategorizedError, ErrorCategory};
use proto::{AggFunc, ClauseSelect, Expr, Plan, Projection, Select, Stage};

/// How a select can fail to lower into the IR.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LowerError {
    /// A pipeline with no stages.
    #[error("the pipeline is empty")]
    EmptyPipeline,
    /// The first pipeline stage must be a `scan`.
    #[error("the pipeline must start with a scan stage, found {found}")]
    MissingSource {
        /// The stage found at the head.
        found: &'static str,
    },
    /// A `scan` after the first stage (additional tables join in).
    #[error("a scan stage is only valid at the pipeline head")]
    MisplacedScan,
    /// A `group` aggregate output whose expression is not an aggregate call.
    #[error("group output {name:?} is not an aggregate expression")]
    NotAnAggregate {
        /// The offending output name.
        name: String,
    },
    /// An aggregate nested inside a larger expression in a clause `select`
    /// list (v1 supports aggregates only as the whole select item).
    #[error("aggregates must be the whole select item in clause form")]
    NestedAggregate,
    /// A clause `having` without grouping.
    #[error("having requires group_by or an aggregate in the select list")]
    HavingWithoutGroup,
    /// Two aggregate outputs would share a name.
    #[error("duplicate aggregate output name {name:?}")]
    DuplicateAggName {
        /// The repeated name.
        name: String,
    },
    /// A clause select is missing its `from`.
    #[error("the clause form requires a from table")]
    MissingFrom,
}

impl CategorizedError for LowerError {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::Validation
    }
}

/// Lower either surface form into the logical-plan IR.
///
/// Equivalent inputs produce **identical** plans: the clause form first
/// desugars into its fixed-order pipeline, then both forms share one fold.
pub fn lower(select: &Select) -> Result<Plan, LowerError> {
    match select {
        Select::Pipeline(stages) => lower_pipeline(stages),
        Select::Clause(clause) => lower_pipeline(&desugar_clause(clause)?),
    }
}

fn stage_name(stage: &Stage) -> &'static str {
    match stage {
        Stage::Scan(_) => "scan",
        Stage::Match(_) => "match",
        Stage::Join(_) => "join",
        Stage::Group { .. } => "group",
        Stage::Sort(_) => "sort",
        Stage::Project(_) => "project",
        Stage::Distinct(_) => "distinct",
        Stage::Limit { .. } => "limit",
        Stage::Cursor(_) => "cursor",
    }
}

fn lower_pipeline(stages: &[Stage]) -> Result<Plan, LowerError> {
    let Some((head, rest)) = stages.split_first() else {
        return Err(LowerError::EmptyPipeline);
    };
    let Stage::Scan(table) = head else {
        return Err(LowerError::MissingSource {
            found: stage_name(head),
        });
    };
    let mut plan = Plan::Scan {
        table: table.table.clone(),
        alias: table.alias.clone(),
    };
    for stage in rest {
        plan = match stage {
            Stage::Scan(_) => return Err(LowerError::MisplacedScan),
            Stage::Match(pred) => Plan::Filter {
                input: Box::new(plan),
                pred: pred.clone(),
            },
            Stage::Join(join) => Plan::Join {
                kind: join.kind,
                left: Box::new(plan),
                right: Box::new(Plan::Scan {
                    table: join.table.table.clone(),
                    alias: join.table.alias.clone(),
                }),
                on: join.on.clone(),
            },
            Stage::Group { by, aggs } => {
                for (name, expr) in aggs {
                    if !matches!(expr, Expr::Agg { .. }) {
                        return Err(LowerError::NotAnAggregate { name: name.clone() });
                    }
                }
                Plan::Aggregate {
                    input: Box::new(plan),
                    by: by.clone(),
                    aggs: aggs.clone(),
                }
            }
            Stage::Sort(keys) => Plan::Sort {
                input: Box::new(plan),
                keys: keys.clone(),
            },
            Stage::Project(items) => Plan::Project {
                input: Box::new(plan),
                items: items.clone(),
            },
            Stage::Distinct(true) => Plan::Distinct {
                input: Box::new(plan),
            },
            // `{distinct:false}` is an explicit no-op.
            Stage::Distinct(false) => plan,
            Stage::Limit { limit, offset } => Plan::Limit {
                input: Box::new(plan),
                limit: *limit,
                offset: *offset,
            },
            Stage::Cursor(token) => Plan::Cursor {
                input: Box::new(plan),
                token: token.clone(),
            },
        };
    }
    Ok(plan)
}

/// Desugar the clause form into its fixed-order pipeline (`SPEC.md` §5.4).
///
/// Grouping is implied by `group_by` *or* by aggregates in the select list:
/// the aggregates become named `group` outputs (the alias, or the function
/// name when unaliased), and the select list then references those outputs
/// by column — exactly the pipeline a user would write by hand.
fn desugar_clause(clause: &ClauseSelect) -> Result<Vec<Stage>, LowerError> {
    let Some(from) = &clause.from else {
        return Err(LowerError::MissingFrom);
    };
    let mut stages = vec![Stage::Scan(from.clone())];
    for join in &clause.joins {
        stages.push(Stage::Join(join.clone()));
    }
    if let Some(where_) = &clause.where_ {
        stages.push(Stage::Match(where_.clone()));
    }

    // Split the select list into aggregate outputs and projection items.
    let mut aggs: Vec<(String, Expr)> = Vec::new();
    let mut project: Option<Vec<Projection>> = None;
    if let Some(items) = &clause.select {
        let mut list = Vec::with_capacity(items.len());
        for item in items {
            let (alias, expr) = match item {
                Projection::Aliased { name, expr } => (Some(name.as_str()), expr),
                Projection::Expr(expr) => (None, expr),
            };
            if let Expr::Agg { func, .. } = expr {
                let name = alias.map_or_else(|| agg_default_name(*func), str::to_string);
                if aggs.iter().any(|(n, _)| *n == name) {
                    return Err(LowerError::DuplicateAggName { name });
                }
                aggs.push((name.clone(), expr.clone()));
                // The projected output is the group's named column.
                list.push(Projection::Expr(Expr::Column {
                    table: None,
                    column: name,
                }));
            } else {
                if contains_agg(expr) {
                    return Err(LowerError::NestedAggregate);
                }
                list.push(item.clone());
            }
        }
        project = Some(list);
    }

    let grouped = !clause.group_by.is_empty() || !aggs.is_empty();
    if grouped {
        stages.push(Stage::Group {
            by: clause.group_by.clone(),
            aggs,
        });
    }
    if let Some(having) = &clause.having {
        if !grouped {
            return Err(LowerError::HavingWithoutGroup);
        }
        stages.push(Stage::Match(having.clone()));
    }
    if let Some(items) = project {
        stages.push(Stage::Project(items));
    }
    if clause.distinct {
        stages.push(Stage::Distinct(true));
    }
    if !clause.order_by.is_empty() {
        stages.push(Stage::Sort(clause.order_by.clone()));
    }
    if clause.limit.is_some() || clause.offset.is_some() {
        stages.push(Stage::Limit {
            limit: clause.limit,
            offset: clause.offset.unwrap_or(0),
        });
    }
    if let Some(token) = &clause.cursor {
        stages.push(Stage::Cursor(token.clone()));
    }
    Ok(stages)
}

/// The output name of an unaliased aggregate in a clause select list.
fn agg_default_name(func: AggFunc) -> String {
    func.name().to_string()
}

/// Does the expression contain an aggregate call anywhere?
fn contains_agg(expr: &Expr) -> bool {
    match expr {
        Expr::Agg { .. } => true,
        Expr::Column { .. } | Expr::Literal(_) => false,
        Expr::Cmp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } | Expr::NullIf { lhs, rhs } => {
            contains_agg(lhs) || contains_agg(rhs)
        }
        Expr::And(items) | Expr::Or(items) | Expr::Coalesce(items) => {
            items.iter().any(contains_agg)
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => contains_agg(inner),
        Expr::Between { expr, lo, hi } => {
            contains_agg(expr) || contains_agg(lo) || contains_agg(hi)
        }
        Expr::InList { expr, list } => contains_agg(expr) || list.iter().any(contains_agg),
        Expr::Like { expr, .. } => contains_agg(expr),
        Expr::Cast { expr, .. } => contains_agg(expr),
    }
}
