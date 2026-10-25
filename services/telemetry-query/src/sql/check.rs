//! Steps 5 to 7 of the check: the walk over the parsed tree. Everything not named here is
//! refused. Structs are destructured with every field named, so a sqlparser upgrade that adds a
//! field fails to compile until someone decides what it means here; an enum variant this walk
//! does not name is refused.
//!
//! The walk also rewrites the tree into the form the service sends: views named with their
//! database, every table alias with `AS`, `CAST` and `::` as conversion functions, `FLOOR`,
//! `CEIL` and `SUBSTRING` as the functions they are, and every function in the spelling
//! ClickHouse defines.

use contract::api::telemetry_query::{SqlClause, SqlRefusal};
use sqlparser::ast::{
    AccessExpr, Array, BinaryOperator, CaseWhen, CastKind, CeilFloorKind, Cte, DataType,
    DateTimeField, Distinct, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentList, FunctionArguments, GroupByExpr, GroupByWithModifier, Ident, Interval,
    Join, JoinConstraint, JoinOperator, LambdaFunction, LambdaFunctionParameter, LambdaSyntax,
    LimitClause, NamedWindowDefinition, NamedWindowExpr, ObjectName, ObjectNamePart, Offset,
    OffsetRows, OneOrManyWithParens, OrderBy, OrderByExpr, OrderByKind, OrderByOptions,
    OrderBySort, Query, Select, SelectFlavor, SelectItem, SelectItemQualifiedWildcardKind, SetExpr,
    SetOperator, SetQuantifier, Subscript, TableAlias, TableFactor, TableWithJoins, UnaryOperator,
    Value, ValueWithSpan, WildcardAdditionalOptions, WindowFrame, WindowFrameBound,
    WindowFrameUnits, WindowSpec, WindowType, With,
};
use sqlparser::keywords::ALL_KEYWORDS;

use super::functions::{self, Kind, TimeZone};
use super::lexer::is_integer;

/// The database of the telemetry views.
pub const DATABASE: &str = "trustiot";

/// The tables a query may read: the tenant views (services/ingest/migrations/clickhouse).
pub const VIEWS: [&str; 3] = [
    "tenant_telemetry",
    "tenant_telemetry_1m",
    "tenant_telemetry_1h",
];

/// How deep the walk goes into queries, tables and expressions before the query counts as too
/// complex. The parser's own recursion limit does not bound chains such as `a + b + c + …`,
/// which it builds in a loop.
pub const MAX_DEPTH: usize = 256;

type Checked<T = ()> = Result<T, SqlRefusal>;

fn clause(clause: SqlClause) -> SqlRefusal {
    SqlRefusal::ClauseNotAllowed { clause }
}

/// The walk over one statement.
#[derive(Debug, Default)]
pub struct Checker {
    /// The common table expressions in scope, innermost last.
    scopes: Vec<Vec<String>>,
    depth: usize,
    reads_view: bool,
}

impl Checker {
    /// Checks and rewrites the statement `query`.
    ///
    /// # Errors
    ///
    /// The refusal for the first thing the walk does not allow, or
    /// [`SqlRefusal::NoTenantView`] when the query reads none of the views.
    pub fn statement(query: &mut Query) -> Checked {
        let mut checker = Self::default();
        checker.query(query)?;
        if checker.reads_view {
            Ok(())
        } else {
            Err(SqlRefusal::NoTenantView)
        }
    }

    fn descend<T>(&mut self, step: impl FnOnce(&mut Self) -> Checked<T>) -> Checked<T> {
        if self.depth >= MAX_DEPTH {
            return Err(SqlRefusal::TooComplex);
        }
        self.depth += 1;
        let result = step(self);
        self.depth -= 1;
        result
    }

    fn in_scope(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.iter().any(|cte| cte == name))
    }

    fn query(&mut self, query: &mut Query) -> Checked {
        self.descend(|checker| {
            let Query {
                with,
                body,
                order_by,
                limit_clause,
                fetch,
                locks,
                for_clause,
                settings,
                format_clause,
                pipe_operators,
            } = query;
            if settings.is_some() {
                return Err(clause(SqlClause::Settings));
            }
            if format_clause.is_some() {
                return Err(clause(SqlClause::Format));
            }
            if fetch.is_some() {
                return Err(clause(SqlClause::Fetch));
            }
            if !locks.is_empty() {
                return Err(clause(SqlClause::Locks));
            }
            if for_clause.is_some() || !pipe_operators.is_empty() {
                return Err(clause(SqlClause::Expression));
            }
            // ClickHouse binds an ORDER BY or LIMIT after a set operation to its last SELECT,
            // unlike the SQL standard and sqlparser.
            if matches!(**body, SetExpr::SetOperation { .. })
                && (order_by.is_some() || limit_clause.is_some())
            {
                return Err(clause(SqlClause::SetOperationOrder));
            }
            checker.scopes.push(Vec::new());
            let result = checker.query_parts(
                with.as_mut(),
                body,
                order_by.as_mut(),
                limit_clause.as_mut(),
            );
            checker.scopes.pop();
            result
        })
    }

    fn query_parts(
        &mut self,
        with: Option<&mut With>,
        body: &mut SetExpr,
        order_by: Option<&mut OrderBy>,
        limit_clause: Option<&mut LimitClause>,
    ) -> Checked {
        if let Some(with) = with {
            self.with(with)?;
        }
        self.set_expr(body)?;
        if let Some(order_by) = order_by {
            self.order_by(order_by)?;
        }
        if let Some(limit_clause) = limit_clause {
            self.limit_clause(limit_clause)?;
        }
        Ok(())
    }

    fn with(&mut self, with: &mut With) -> Checked {
        let With {
            with_token: _,
            recursive,
            cte_tables,
        } = with;
        if *recursive {
            return Err(clause(SqlClause::RecursiveCte));
        }
        for cte in cte_tables {
            let Cte {
                alias,
                query,
                from,
                materialized,
                closing_paren_token: _,
            } = cte;
            let TableAlias {
                explicit: _,
                name,
                columns,
                at,
            } = alias;
            if !columns.is_empty() || from.is_some() || materialized.is_some() || at.is_some() {
                return Err(clause(SqlClause::CteForm));
            }
            let shadows = VIEWS
                .iter()
                .chain([&DATABASE])
                .any(|table| table.eq_ignore_ascii_case(&name.value));
            let twice = self
                .scopes
                .last()
                .is_some_and(|scope| scope.contains(&name.value));
            if shadows || twice {
                return Err(SqlRefusal::TableNotAllowed {
                    table: name.value.clone(),
                });
            }
            // A common table expression sees the ones before it, not itself.
            self.query(query)?;
            if let Some(scope) = self.scopes.last_mut() {
                scope.push(name.value.clone());
            }
        }
        Ok(())
    }

    fn set_expr(&mut self, body: &mut SetExpr) -> Checked {
        self.descend(|checker| match body {
            SetExpr::Select(select) => checker.select(select),
            SetExpr::Query(query) => checker.query(query),
            SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                match op {
                    SetOperator::Union | SetOperator::Except | SetOperator::Intersect => {}
                    SetOperator::Minus => return Err(clause(SqlClause::Expression)),
                }
                match set_quantifier {
                    SetQuantifier::All | SetQuantifier::Distinct => {}
                    SetQuantifier::None
                    | SetQuantifier::ByName
                    | SetQuantifier::AllByName
                    | SetQuantifier::DistinctByName => {
                        return Err(clause(SqlClause::SetQuantifier));
                    }
                }
                checker.set_expr(left)?;
                checker.set_expr(right)
            }
            SetExpr::Values(_)
            | SetExpr::Insert(_)
            | SetExpr::Update(_)
            | SetExpr::Delete(_)
            | SetExpr::Merge(_)
            | SetExpr::Table(_) => Err(SqlRefusal::NotASelect),
        })
    }

    fn select(&mut self, select: &mut Select) -> Checked {
        let Select {
            select_token: _,
            optimizer_hints,
            distinct,
            select_modifiers,
            top,
            top_before_distinct: _,
            projection,
            exclude,
            into,
            from,
            lateral_views,
            prewhere,
            selection,
            connect_by,
            group_by,
            cluster_by,
            distribute_by,
            sort_by,
            having,
            named_window,
            qualify,
            window_before_qualify: _,
            value_table_mode,
            flavor,
        } = select;
        match flavor {
            SelectFlavor::Standard => {}
            SelectFlavor::FromFirst | SelectFlavor::FromFirstNoSelect => {
                return Err(clause(SqlClause::FromFirst));
            }
        }
        match distinct {
            None | Some(Distinct::Distinct) => {}
            Some(Distinct::On(_)) => return Err(clause(SqlClause::DistinctOn)),
            Some(Distinct::All) => return Err(clause(SqlClause::Expression)),
        }
        if top.is_some() {
            return Err(clause(SqlClause::Top));
        }
        if into.is_some() {
            return Err(clause(SqlClause::Into));
        }
        if prewhere.is_some() {
            return Err(clause(SqlClause::Prewhere));
        }
        if qualify.is_some() {
            return Err(clause(SqlClause::Qualify));
        }
        if !lateral_views.is_empty() {
            return Err(clause(SqlClause::Lateral));
        }
        if exclude.is_some() {
            return Err(clause(SqlClause::Wildcard));
        }
        if !optimizer_hints.is_empty()
            || select_modifiers.is_some()
            || value_table_mode.is_some()
            || !connect_by.is_empty()
            || !cluster_by.is_empty()
            || !distribute_by.is_empty()
            || !sort_by.is_empty()
        {
            return Err(clause(SqlClause::Expression));
        }
        for table in from {
            self.table_with_joins(table)?;
        }
        for item in projection {
            self.select_item(item)?;
        }
        if let Some(selection) = selection {
            self.expr(selection)?;
        }
        self.group_by(group_by)?;
        if let Some(having) = having {
            self.expr(having)?;
        }
        for NamedWindowDefinition(_, window) in named_window {
            match window {
                NamedWindowExpr::NamedWindow(_) => {}
                NamedWindowExpr::WindowSpec(spec) => self.window_spec(spec)?,
            }
        }
        Ok(())
    }

    fn select_item(&mut self, item: &mut SelectItem) -> Checked {
        match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, alias: _ } => {
                self.expr(expr)
            }
            SelectItem::QualifiedWildcard(kind, options) => {
                match kind {
                    SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        plain_name(name).ok_or(clause(SqlClause::Wildcard))?;
                    }
                    SelectItemQualifiedWildcardKind::Expr(_) => {
                        return Err(clause(SqlClause::Wildcard));
                    }
                }
                wildcard_options(options)
            }
            SelectItem::Wildcard(options) => wildcard_options(options),
            SelectItem::ExprWithAliases { .. } => Err(clause(SqlClause::Expression)),
        }
    }

    fn table_with_joins(&mut self, table: &mut TableWithJoins) -> Checked {
        let TableWithJoins { relation, joins } = table;
        self.table_factor(relation)?;
        for join in joins {
            let Join {
                relation,
                global,
                join_operator,
            } = join;
            if *global {
                return Err(clause(SqlClause::JoinKind));
            }
            self.table_factor(relation)?;
            match join_operator {
                JoinOperator::Join(constraint)
                | JoinOperator::Inner(constraint)
                | JoinOperator::Left(constraint)
                | JoinOperator::LeftOuter(constraint)
                | JoinOperator::Right(constraint)
                | JoinOperator::RightOuter(constraint)
                | JoinOperator::FullOuter(constraint)
                | JoinOperator::LeftSemi(constraint)
                | JoinOperator::RightSemi(constraint)
                | JoinOperator::LeftAnti(constraint)
                | JoinOperator::RightAnti(constraint) => self.join_constraint(constraint)?,
                JoinOperator::CrossJoin(JoinConstraint::None) => {}
                _ => return Err(clause(SqlClause::JoinKind)),
            }
        }
        Ok(())
    }

    fn join_constraint(&mut self, constraint: &mut JoinConstraint) -> Checked {
        match constraint {
            JoinConstraint::On(expr) => self.expr(expr),
            JoinConstraint::Using(columns) => {
                for column in columns {
                    match plain_name(column).as_deref() {
                        Some([_]) => {}
                        _ => return Err(clause(SqlClause::Expression)),
                    }
                }
                Ok(())
            }
            JoinConstraint::Natural => Err(clause(SqlClause::JoinKind)),
            JoinConstraint::None => Err(clause(SqlClause::JoinWithoutCondition)),
        }
    }

    fn table_factor(&mut self, factor: &mut TableFactor) -> Checked {
        self.descend(|checker| match factor {
            TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                version,
                with_ordinality,
                partitions,
                json_path,
                sample,
                index_hints,
            } => {
                if args.is_some() {
                    // A table function: numbers(), remote(), merge(), url() and the like.
                    return Err(SqlRefusal::TableNotAllowed {
                        table: written(name),
                    });
                }
                if sample.is_some() {
                    return Err(clause(SqlClause::Sample));
                }
                if !with_hints.is_empty()
                    || version.is_some()
                    || *with_ordinality
                    || !partitions.is_empty()
                    || json_path.is_some()
                    || !index_hints.is_empty()
                {
                    return Err(clause(SqlClause::Expression));
                }
                checker.relation(name)?;
                table_alias(alias.as_mut())
            }
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
                sample,
            } => {
                if *lateral {
                    return Err(clause(SqlClause::Lateral));
                }
                if sample.is_some() {
                    return Err(clause(SqlClause::Sample));
                }
                checker.query(subquery)?;
                table_alias(alias.as_mut())
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                checker.table_with_joins(table_with_joins)?;
                table_alias(alias.as_mut())
            }
            TableFactor::Function { name, .. } => Err(SqlRefusal::TableNotAllowed {
                table: written(name),
            }),
            TableFactor::TableFunction {
                expr: Expr::Function(function),
                ..
            } => Err(SqlRefusal::TableNotAllowed {
                table: written(&function.name),
            }),
            _ => Err(clause(SqlClause::Expression)),
        })
    }

    /// A table name: a common table expression in scope, or a view, which is then named with
    /// its database.
    fn relation(&mut self, name: &mut ObjectName) -> Checked {
        let refused = || SqlRefusal::TableNotAllowed {
            table: written(name),
        };
        let parts = plain_name(name).ok_or_else(refused)?;
        let view = match parts.as_slice() {
            [table] if self.in_scope(table) => return Ok(()),
            [table] => VIEWS.iter().find(|view| *view == table),
            [database, table] if database == DATABASE => VIEWS.iter().find(|view| *view == table),
            _ => None,
        }
        .ok_or_else(refused)?;
        *name = ObjectName(vec![
            ObjectNamePart::Identifier(Ident::new(DATABASE)),
            ObjectNamePart::Identifier(Ident::new(*view)),
        ]);
        self.reads_view = true;
        Ok(())
    }

    fn group_by(&mut self, group_by: &mut GroupByExpr) -> Checked {
        let modifiers = match group_by {
            GroupByExpr::All(modifiers) => modifiers,
            GroupByExpr::Expressions(expressions, modifiers) => {
                for expr in expressions {
                    match expr {
                        Expr::Rollup(sets) | Expr::Cube(sets) | Expr::GroupingSets(sets) => {
                            for expr in sets.iter_mut().flatten() {
                                self.expr(expr)?;
                            }
                        }
                        _ => self.expr(expr)?,
                    }
                }
                modifiers
            }
        };
        for modifier in modifiers {
            match modifier {
                GroupByWithModifier::Rollup | GroupByWithModifier::Cube => {}
                GroupByWithModifier::Totals => return Err(clause(SqlClause::WithTotals)),
                GroupByWithModifier::GroupingSets(_) => {
                    return Err(clause(SqlClause::Expression));
                }
            }
        }
        Ok(())
    }

    fn order_by(&mut self, order_by: &mut OrderBy) -> Checked {
        let OrderBy { kind, interpolate } = order_by;
        if interpolate.is_some() {
            return Err(clause(SqlClause::Interpolate));
        }
        match kind {
            OrderByKind::All(options) => order_options(options),
            OrderByKind::Expressions(expressions) => {
                for expr in expressions {
                    self.order_by_expr(expr)?;
                }
                Ok(())
            }
        }
    }

    fn order_by_expr(&mut self, order_by: &mut OrderByExpr) -> Checked {
        let OrderByExpr {
            expr,
            options,
            with_fill,
        } = order_by;
        if with_fill.is_some() {
            return Err(clause(SqlClause::WithFill));
        }
        order_options(options)?;
        self.expr(expr)
    }

    fn limit_clause(&mut self, limit_clause: &mut LimitClause) -> Checked {
        match limit_clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                if let Some(limit) = limit {
                    integer_literal(limit)?;
                }
                if let Some(Offset { value, rows }) = offset {
                    integer_literal(value)?;
                    if !matches!(rows, OffsetRows::None) {
                        return Err(clause(SqlClause::LimitValue));
                    }
                }
                for expr in limit_by {
                    self.expr(expr)?;
                }
                Ok(())
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                integer_literal(offset)?;
                integer_literal(limit)
            }
        }
    }

    fn window_spec(&mut self, spec: &mut WindowSpec) -> Checked {
        let WindowSpec {
            window_name,
            partition_by,
            order_by,
            window_frame,
        } = spec;
        if window_name.is_some() {
            return Err(clause(SqlClause::Expression));
        }
        for expr in partition_by {
            self.expr(expr)?;
        }
        for expr in order_by {
            self.order_by_expr(expr)?;
        }
        if let Some(WindowFrame {
            units,
            start_bound,
            end_bound,
        }) = window_frame
        {
            match units {
                WindowFrameUnits::Rows | WindowFrameUnits::Range => {}
                WindowFrameUnits::Groups => return Err(clause(SqlClause::WindowFrame)),
            }
            for bound in std::iter::once(start_bound).chain(end_bound.as_mut()) {
                match bound {
                    WindowFrameBound::CurrentRow
                    | WindowFrameBound::Preceding(None)
                    | WindowFrameBound::Following(None) => {}
                    WindowFrameBound::Preceding(Some(expr))
                    | WindowFrameBound::Following(Some(expr)) => self.expr(expr)?,
                }
            }
        }
        Ok(())
    }

    fn expr(&mut self, expr: &mut Expr) -> Checked {
        self.descend(|checker| {
            if let Some(rewritten) = checker.expr_kind(expr)? {
                *expr = rewritten;
            }
            Ok(())
        })
    }

    /// Checks one expression node; `Some` is the node to put in its place.
    fn expr_kind(&mut self, expr: &mut Expr) -> Checked<Option<Expr>> {
        match expr {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {}
            Expr::Value(value) => literal(value)?,
            // sqlparser prints `- -1` as `--1`, a comment to ClickHouse: the inner minus gets
            // parentheses.
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr: inner,
            } if matches!(
                **inner,
                Expr::UnaryOp {
                    op: UnaryOperator::Minus,
                    ..
                }
            ) =>
            {
                let operand = take(inner);
                **inner = Expr::Nested(Box::new(operand));
                self.expr(inner)?;
            }
            Expr::Nested(inner)
            | Expr::IsNull(inner)
            | Expr::IsNotNull(inner)
            | Expr::UnaryOp {
                op: UnaryOperator::Plus | UnaryOperator::Minus | UnaryOperator::Not,
                expr: inner,
            } => self.expr(inner)?,
            Expr::Tuple(items)
            | Expr::Array(Array {
                elem: items,
                named: false,
            }) => {
                for item in items {
                    self.expr(item)?;
                }
            }
            Expr::BinaryOp { left, op, right } => {
                operator(op)?;
                self.expr(left)?;
                self.expr(right)?;
            }
            Expr::Between {
                expr,
                negated: _,
                low,
                high,
            } => {
                self.expr(expr)?;
                self.expr(low)?;
                self.expr(high)?;
            }
            Expr::Like {
                negated: _,
                any,
                expr,
                pattern,
                escape_char,
            }
            | Expr::ILike {
                negated: _,
                any,
                expr,
                pattern,
                escape_char,
            } => {
                if *any {
                    return Err(clause(SqlClause::Operator));
                }
                if escape_char.is_some() {
                    return Err(clause(SqlClause::Escape));
                }
                self.expr(expr)?;
                self.expr(pattern)?;
            }
            Expr::InList {
                expr,
                list,
                negated: _,
            } => {
                self.expr(expr)?;
                // ClickHouse reads a name in an IN list as a table (`x IN (system.one)`).
                for item in list {
                    in_list_literal(item)?;
                }
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated: _,
            } => {
                self.expr(expr)?;
                self.query(subquery)?;
            }
            Expr::Exists {
                subquery,
                negated: _,
            } => self.query(subquery)?,
            Expr::Subquery(subquery) => self.query(subquery)?,
            Expr::Case {
                case_token: _,
                end_token: _,
                operand,
                conditions,
                else_result,
            } => {
                if let Some(operand) = operand {
                    self.expr(operand)?;
                }
                for CaseWhen { condition, result } in conditions {
                    self.expr(condition)?;
                    self.expr(result)?;
                }
                if let Some(else_result) = else_result {
                    self.expr(else_result)?;
                }
            }
            Expr::CompoundFieldAccess { root, access_chain } => {
                self.expr(root)?;
                for access in access_chain {
                    match access {
                        AccessExpr::Subscript(Subscript::Index { index }) => self.expr(index)?,
                        AccessExpr::Dot(Expr::Identifier(_)) => {}
                        AccessExpr::Subscript(Subscript::Slice { .. }) | AccessExpr::Dot(_) => {
                            return Err(clause(SqlClause::Expression));
                        }
                    }
                }
            }
            Expr::Interval(interval) => interval_literal(interval)?,
            Expr::Function(function) => self.function(function)?,
            Expr::Cast {
                kind,
                expr,
                data_type,
                format,
            } => {
                if !matches!(kind, CastKind::Cast | CastKind::DoubleColon) || format.is_some() {
                    return Err(clause(SqlClause::CastType));
                }
                let function = conversion(data_type).ok_or(clause(SqlClause::CastType))?;
                self.expr(expr)?;
                return Ok(Some(call(function, vec![take(expr)])));
            }
            Expr::Floor { expr, field } => return self.rounding("floor", expr, field).map(Some),
            Expr::Ceil { expr, field } => return self.rounding("ceil", expr, field).map(Some),
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                special,
                shorthand: _,
            } => {
                // `special` is the comma form, `SUBSTRING(s, a, b)`; the other is
                // `SUBSTRING(s FROM a FOR b)`.
                if !*special {
                    return Err(clause(SqlClause::SpecialSyntax));
                }
                let mut args = Vec::new();
                for part in std::iter::once(expr)
                    .chain(substring_from.as_mut())
                    .chain(substring_for.as_mut())
                {
                    self.expr(part)?;
                    args.push(take(part));
                }
                return Ok(Some(call("substring", args)));
            }
            Expr::Lambda(_) => return Err(clause(SqlClause::Lambda)),
            Expr::Extract { .. }
            | Expr::AtTimeZone { .. }
            | Expr::Position { .. }
            | Expr::Trim { .. }
            | Expr::TypedString(_) => return Err(clause(SqlClause::SpecialSyntax)),
            Expr::UnaryOp { .. }
            | Expr::IsTrue(_)
            | Expr::IsFalse(_)
            | Expr::IsNotTrue(_)
            | Expr::IsNotFalse(_)
            | Expr::IsUnknown(_)
            | Expr::IsNotUnknown(_)
            | Expr::IsDistinctFrom(..)
            | Expr::IsNotDistinctFrom(..)
            | Expr::SimilarTo { .. }
            | Expr::RLike { .. }
            | Expr::AnyOp { .. }
            | Expr::AllOp { .. }
            | Expr::MemberOf(_) => return Err(clause(SqlClause::Operator)),
            _ => return Err(clause(SqlClause::Expression)),
        }
        Ok(None)
    }

    /// `FLOOR(x[, n])` and `CEIL(x[, n])`, which sqlparser reads as their own nodes, as the
    /// functions `floor` and `ceil`.
    fn rounding(
        &mut self,
        name: &str,
        expr: &mut Expr,
        field: &mut CeilFloorKind,
    ) -> Checked<Expr> {
        self.expr(expr)?;
        let mut args = vec![take(expr)];
        match field {
            CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) => {}
            CeilFloorKind::Scale(scale) => {
                literal(scale)?;
                args.push(Expr::Value(scale.clone()));
            }
            CeilFloorKind::DateTimeField(_) => return Err(clause(SqlClause::SpecialSyntax)),
        }
        Ok(call(name, args))
    }

    fn function(&mut self, function: &mut Function) -> Checked {
        let Function {
            name,
            uses_odbc_syntax,
            parameters,
            args,
            within_group,
            filter,
            null_treatment,
            over,
        } = function;
        let refused = SqlRefusal::FunctionNotAllowed {
            function: written(name),
        };
        let [ObjectNamePart::Identifier(ident)] = name.0.as_mut_slice() else {
            return Err(refused);
        };
        let resolved = functions::resolve(&ident.value).ok_or(refused)?;
        ident.value.clone_from(&resolved.canonical);
        let kind = if resolved.combined {
            Kind::Aggregate
        } else {
            resolved.function.kind
        };
        if *uses_odbc_syntax {
            return Err(clause(SqlClause::Expression));
        }
        if !within_group.is_empty() || filter.is_some() || null_treatment.is_some() {
            return Err(clause(SqlClause::AggregateClause));
        }
        match parameters {
            FunctionArguments::None => {}
            FunctionArguments::List(list) if resolved.function.parametric => {
                literal_parameters(list)?;
            }
            FunctionArguments::List(_) | FunctionArguments::Subquery(_) => {
                return Err(clause(SqlClause::Parameters));
            }
        }
        let FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment,
            args,
            clauses,
        }) = args
        else {
            return Err(clause(SqlClause::Expression));
        };
        match duplicate_treatment {
            None => {}
            Some(DuplicateTreatment::Distinct) if kind == Kind::Aggregate => {}
            Some(DuplicateTreatment::Distinct) => {
                return Err(clause(SqlClause::DistinctArgument));
            }
            Some(DuplicateTreatment::All) => return Err(clause(SqlClause::Expression)),
        }
        if !clauses.is_empty() {
            return Err(clause(SqlClause::AggregateClause));
        }
        let count = args.len();
        for (index, arg) in args.iter_mut().enumerate() {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Lambda(lambda)))
                    if index == 0 && kind == Kind::HigherOrder =>
                {
                    self.lambda(lambda)?;
                }
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => self.expr(expr)?,
                // `count(*)`.
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                    if resolved.canonical == "count" && count == 1 => {}
                FunctionArg::Unnamed(_) => return Err(clause(SqlClause::Wildcard)),
                FunctionArg::Named { .. } | FunctionArg::ExprNamed { .. } => {
                    return Err(clause(SqlClause::NamedArgument));
                }
            }
        }
        if !resolved.combined
            && let Some(time_zone) = resolved.function.time_zone
        {
            time_zone_given(time_zone, args, &resolved.canonical)?;
        }
        if let Some(over) = over {
            if !matches!(kind, Kind::Aggregate | Kind::Window) {
                return Err(clause(SqlClause::Over));
            }
            match over {
                WindowType::WindowSpec(spec) => self.window_spec(spec)?,
                WindowType::NamedWindow(_) => {}
            }
        }
        Ok(())
    }

    fn lambda(&mut self, lambda: &mut LambdaFunction) -> Checked {
        let LambdaFunction {
            params,
            body,
            syntax,
        } = lambda;
        if !matches!(syntax, LambdaSyntax::Arrow) {
            return Err(clause(SqlClause::Lambda));
        }
        let params = match params {
            OneOrManyWithParens::One(param) => std::slice::from_ref(param),
            OneOrManyWithParens::Many(params) => params.as_slice(),
        };
        for LambdaFunctionParameter { name: _, data_type } in params {
            if data_type.is_some() {
                return Err(clause(SqlClause::Lambda));
            }
        }
        self.expr(body)
    }
}

/// The parts of `name` when each is a plain identifier.
fn plain_name(name: &ObjectName) -> Option<Vec<String>> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) if ident.quote_style.is_none() => {
                Some(ident.value.clone())
            }
            _ => None,
        })
        .collect()
}

/// `name` as written, for a refusal: plain identifiers joined with `.` (the lexer allows no
/// other identifiers), or `?` for a part of another form.
fn written(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) if ident.quote_style.is_none() => {
                ident.value.as_str()
            }
            _ => "?",
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn wildcard_options(options: &WildcardAdditionalOptions) -> Checked {
    let WildcardAdditionalOptions {
        wildcard_token: _,
        opt_ilike,
        opt_exclude,
        opt_except,
        opt_replace,
        opt_rename,
        opt_alias,
    } = options;
    if opt_ilike.is_some()
        || opt_exclude.is_some()
        || opt_except.is_some()
        || opt_replace.is_some()
        || opt_rename.is_some()
        || opt_alias.is_some()
    {
        return Err(clause(SqlClause::Wildcard));
    }
    Ok(())
}

/// A table alias always gets `AS`. Without it, a keyword is refused: `FROM t FINAL`, `FROM a ANY
/// LEFT JOIN b` and `FROM a PASTE JOIN b` read as aliases to sqlparser and as modifiers to
/// ClickHouse.
fn table_alias(alias: Option<&mut TableAlias>) -> Checked {
    let Some(TableAlias {
        explicit,
        name,
        columns,
        at,
    }) = alias
    else {
        return Ok(());
    };
    if !columns.is_empty() || at.is_some() {
        return Err(clause(SqlClause::Expression));
    }
    if !*explicit && is_keyword(&name.value) {
        return Err(clause(SqlClause::KeywordAlias));
    }
    *explicit = true;
    Ok(())
}

/// sqlparser's keywords, and the table and join modifiers of ClickHouse it may not know.
fn is_keyword(word: &str) -> bool {
    const CLICKHOUSE: [&str; 11] = [
        "ALL", "ANTI", "ANY", "ARRAY", "ASOF", "FINAL", "GLOBAL", "LOCAL", "PASTE", "SAMPLE",
        "SEMI",
    ];
    let upper = word.to_ascii_uppercase();
    ALL_KEYWORDS.contains(&upper.as_str()) || CLICKHOUSE.contains(&upper.as_str())
}

fn order_options(options: &OrderByOptions) -> Checked {
    let OrderByOptions {
        sort,
        nulls_first: _,
    } = options;
    match sort {
        None | Some(OrderBySort::Asc | OrderBySort::Desc) => Ok(()),
        Some(OrderBySort::Using(_)) => Err(clause(SqlClause::Expression)),
    }
}

/// The literals a query may hold: plain decimal numbers (the lexer checked their text), `'…'`
/// strings, booleans and `NULL`.
fn literal(value: &ValueWithSpan) -> Checked {
    match &value.value {
        Value::Number(_, false)
        | Value::SingleQuotedString(_)
        | Value::Boolean(_)
        | Value::Null => Ok(()),
        _ => Err(SqlRefusal::LiteralNotAllowed {
            line: u32::try_from(value.span.start.line).unwrap_or(u32::MAX),
            column: u32::try_from(value.span.start.column).unwrap_or(u32::MAX),
        }),
    }
}

fn integer_literal(expr: &Expr) -> Checked {
    match expr {
        Expr::Value(ValueWithSpan {
            value: Value::Number(text, false),
            span: _,
        }) if is_integer(text) => Ok(()),
        _ => Err(clause(SqlClause::LimitValue)),
    }
}

/// An element of an `IN` list: a literal, a negative number, or a tuple of those.
fn in_list_literal(expr: &Expr) -> Checked {
    match expr {
        Expr::Value(value) => literal(value),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(
            **expr,
            Expr::Value(ValueWithSpan {
                value: Value::Number(_, false),
                ..
            })
        ) =>
        {
            Ok(())
        }
        Expr::Tuple(items) => items.iter().try_for_each(in_list_literal),
        _ => Err(clause(SqlClause::InList)),
    }
}

/// `INTERVAL n UNIT`: an integer literal and a unit from second to year.
fn interval_literal(interval: &Interval) -> Checked {
    let Interval {
        value,
        leading_field,
        leading_precision,
        last_field,
        fractional_seconds_precision,
    } = interval;
    let unit = matches!(
        leading_field,
        Some(
            DateTimeField::Second
                | DateTimeField::Minute
                | DateTimeField::Hour
                | DateTimeField::Day
                | DateTimeField::Week(None)
                | DateTimeField::Month
                | DateTimeField::Quarter
                | DateTimeField::Year
        )
    );
    let amount = matches!(
        &**value,
        Expr::Value(ValueWithSpan {
            value: Value::Number(text, false),
            ..
        }) if is_integer(text)
    );
    if unit
        && amount
        && leading_precision.is_none()
        && last_field.is_none()
        && fractional_seconds_precision.is_none()
    {
        Ok(())
    } else {
        Err(clause(SqlClause::Interval))
    }
}

fn operator(op: &BinaryOperator) -> Checked {
    match op {
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo
        | BinaryOperator::StringConcat
        | BinaryOperator::Gt
        | BinaryOperator::Lt
        | BinaryOperator::GtEq
        | BinaryOperator::LtEq
        | BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::And
        | BinaryOperator::Or => Ok(()),
        _ => Err(clause(SqlClause::Operator)),
    }
}

/// The parameters of a parametric aggregate (`quantile(0.9)`): literal numbers and strings.
fn literal_parameters(list: &FunctionArgumentList) -> Checked {
    let FunctionArgumentList {
        duplicate_treatment,
        args,
        clauses,
    } = list;
    let literals = args.iter().all(|arg| {
        matches!(
            arg,
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                value: Value::Number(_, false) | Value::SingleQuotedString(_),
                ..
            })))
        )
    });
    if duplicate_treatment.is_none() && clauses.is_empty() && literals {
        Ok(())
    } else {
        Err(clause(SqlClause::Parameters))
    }
}

/// The time zone rule (PoC lesson 6, docs/feasibility/2026-10-09-cloud-poc.md): a function that
/// buckets, formats or reads time names its zone as a string literal.
fn time_zone_given(time_zone: TimeZone, args: &[FunctionArg], name: &str) -> Checked {
    let is_text = |arg: Option<&FunctionArg>| {
        matches!(
            arg,
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                ValueWithSpan {
                    value: Value::SingleQuotedString(_),
                    ..
                }
            ))))
        )
    };
    let first_is_text = is_text(args.first());
    let arity = match time_zone {
        TimeZone::Always(arity) => Some(arity),
        TimeZone::UnlessText(arity) => (!first_is_text).then_some(arity),
        TimeZone::WhenText(arity) => first_is_text.then_some(arity),
    };
    match arity {
        Some(arity) if args.len() != arity || !is_text(args.last()) => {
            Err(SqlRefusal::TimeZoneRequired {
                function: name.to_owned(),
            })
        }
        _ => Ok(()),
    }
}

/// The conversion function of a `CAST` target: only types whose name sqlparser prints the way
/// ClickHouse reads it after the rewrite.
fn conversion(data_type: &DataType) -> Option<&'static str> {
    let custom = match data_type {
        DataType::Float64 => return Some("toFloat64"),
        DataType::Float32 => return Some("toFloat32"),
        DataType::Int64 => return Some("toInt64"),
        DataType::Int32 => return Some("toInt32"),
        DataType::UInt64 => return Some("toUInt64"),
        DataType::UInt32 => return Some("toUInt32"),
        DataType::String(None) => return Some("toString"),
        DataType::Custom(name, modifiers) if modifiers.is_empty() => plain_name(name)?,
        _ => return None,
    };
    match custom.as_slice() {
        [name] => match name.as_str() {
            "Float64" => Some("toFloat64"),
            "Float32" => Some("toFloat32"),
            "Int64" => Some("toInt64"),
            "Int32" => Some("toInt32"),
            "UInt64" => Some("toUInt64"),
            "UInt32" => Some("toUInt32"),
            "String" => Some("toString"),
            _ => None,
        },
        _ => None,
    }
}

/// A call of `name` with `args`, as sqlparser parses `name(args…)`.
fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args
                .into_iter()
                .map(|arg| FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)))
                .collect(),
            clauses: Vec::new(),
        }),
        within_group: Vec::new(),
        filter: None,
        null_treatment: None,
        over: None,
    })
}

fn take(expr: &mut Expr) -> Expr {
    std::mem::replace(expr, Expr::Value(Value::Null.into()))
}
